//! Live regression tests for the **startup** robustness of the reconciler.
//!
//! Every test here pins one of the failures measured on the real daemon while
//! diagnosing the "continuous fast on/off chopping on a USB card with the EQ
//! active, insensitive to later toggles" report:
//!
//! | test | bug it pins |
//! |---|---|
//! | [`volume_reaches_the_node_on_the_first_apply_with_the_pulseaudio_curve`] | the first `apply_snapshot` never applied any strip's volume, and the raw slider value was written as a *linear* gain (+14 dB) |
//! | [`volume_also_scales_the_monitor_the_vu_meters_read`] | the monitor tap has its own gain: it stayed at unity, so every VU bar read pre-fader ("meters pinned at low sliders") |
//! | [`a_missing_device_does_not_abort_the_snapshot_and_heals_when_it_appears`] | one unplugged device aborted the whole snapshot **and** every later apply (`chain_gone`); the chain of the absent strip was autoconnected into another strip's sink |
//! | [`a_toggle_re_derives_the_routes_and_repairs_the_chain_links`] | `fx.set_active` did nothing when the chain already existed, so a graph built wrong stayed wrong |
//! | [`stereo_routes_pair_fl_with_fl_and_fr_with_fr`] | exact FL→FL / FR→FR wiring, and the documented mono duplication |
//!
//! They share the live gate of `backend_live.rs`: they only touch a daemon whose
//! runtime dir looks like the headless sandbox, or when `HOLAF_PW_LIVE=1` is set.

use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use holaf_core::backend::Backend;
use holaf_core::model::{EffectKey, Strip, StripType};
use holaf_pw::PipeWireBackend;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// The tests drive the *same* daemon, so they must never overlap.
fn live_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn live_allowed() -> bool {
    if std::env::var("HOLAF_PW_LIVE").as_deref() == Ok("1") {
        return true;
    }
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("PIPEWIRE_RUNTIME_DIR"))
        .unwrap_or_default();
    dir.contains("pw-runtime") || dir.contains("holaf")
}

fn run(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pw_dump() -> Option<serde_json::Value> {
    serde_json::from_str(&run("pw-dump", &[])?).ok()
}

fn node_id_by_name(name: &str) -> Option<u32> {
    let dump = pw_dump()?;
    dump.as_array()?.iter().find_map(|o| {
        let props = o.get("info")?.get("props")?;
        if props.get("node.name")?.as_str() == Some(name) {
            o.get("id")?.as_u64().map(|id| id as u32)
        } else {
            None
        }
    })
}

/// `(port.name, port.name)` for every live link between two node names.
fn port_links_between(from: &str, to: &str) -> Vec<(String, String)> {
    let Some(dump) = pw_dump() else {
        return Vec::new();
    };
    let Some(arr) = dump.as_array() else {
        return Vec::new();
    };
    let node_name = |id: u64| -> Option<String> {
        arr.iter().find_map(|o| {
            if o.get("id")?.as_u64()? != id {
                return None;
            }
            Some(o.get("info")?.get("props")?.get("node.name")?.as_str()?.to_string())
        })
    };
    let port_name = |id: u64| -> Option<String> {
        arr.iter().find_map(|o| {
            if o.get("id")?.as_u64()? != id {
                return None;
            }
            Some(o.get("info")?.get("props")?.get("port.name")?.as_str()?.to_string())
        })
    };
    arr.iter()
        .filter(|o| o.get("type").and_then(|t| t.as_str()) == Some("PipeWire:Interface:Link"))
        .filter_map(|o| {
            let props = o.get("info")?.get("props")?;
            let out_node = props.get("link.output.node")?.as_u64()?;
            let in_node = props.get("link.input.node")?.as_u64()?;
            if node_name(out_node)? != from || node_name(in_node)? != to {
                return None;
            }
            Some((
                port_name(props.get("link.output.port")?.as_u64()?)?,
                port_name(props.get("link.input.port")?.as_u64()?)?,
            ))
        })
        .collect()
}

/// `pw-cli enum-params <node> Props` → the `channelVolumes` / `monitorVolumes`
/// arrays as reported by PipeWire itself (the only honest read-back: the
/// daemon's own `status` echoes the requested volume, not the applied gain).
fn volumes_of(node_name: &str) -> Option<(Vec<f32>, Vec<f32>)> {
    let id = node_id_by_name(node_name)?;
    let text = run("pw-cli", &["enum-params", &id.to_string(), "Props"])?;
    let mut channel = Vec::new();
    let mut monitor = Vec::new();
    let mut current: Option<&mut Vec<f32>> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.contains("channelVolumes") {
            current = Some(&mut channel);
        } else if line.contains("monitorVolumes") {
            current = Some(&mut monitor);
        } else if line.starts_with("Prop:") {
            current = None;
        } else if line.starts_with("Float ") {
            if let (Some(target), Some(value)) = (
                current.as_deref_mut(),
                line.split_whitespace().nth(1).and_then(|v| v.parse::<f32>().ok()),
            ) {
                target.push(value);
            }
        }
    }
    Some((channel, monitor))
}

fn create_null_sink(name: &str) -> bool {
    let spec = format!(
        "create-node adapter {{ factory.name=support.null-audio-sink node.name={name} \
         node.description={name} media.class=Audio/Sink object.linger=true \
         audio.position=[ FL FR ] }}"
    );
    run("pw-cli", &[&spec]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if node_id_by_name(name).is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn destroy_node(name: &str) {
    if let Some(id) = node_id_by_name(name) {
        run("pw-cli", &["destroy", &id.to_string()]);
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn connect_or_skip() -> Option<PipeWireBackend> {
    match PipeWireBackend::connect() {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("SKIP apply_robustness: cannot reach a PipeWire daemon: {e}");
            None
        }
    }
}

/// Is the daemon we are talking to the headless test sandbox (as opposed to a
/// user's real session)?
fn sandbox_runtime() -> bool {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("PIPEWIRE_RUNTIME_DIR"))
        .unwrap_or_default();
    dir.contains("pw-runtime") || dir.contains("holaf")
}

/// Restart the headless sandbox daemon so every test starts from a pristine
/// graph (never touch a real session: gated on [`sandbox_runtime`]).
///
/// Two reasons, both measured in this sandbox:
///
/// - the tests share one daemon, and a test that leaves debris would corrupt
///   the next one;
/// - PipeWire 0.3.65 stops publishing `Port` globals after many
///   create/destroy cycles on one long-lived daemon, after which *every* link
///   creation fails with "no usable port pair" (documented in the project
///   notes; the daemon now reports it as a pending link instead of silently
///   succeeding).
fn fresh_sandbox_daemon() {
    if !sandbox_runtime() {
        return;
    }
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../scripts/start-pipewire-headless.sh"
    );
    let _ = Command::new("bash").arg(script).arg("restart").output();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if run("pw-dump", &[]).is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Connect to a pristine sandbox daemon (see [`fresh_sandbox_daemon`]) and tear
/// down whatever graph it still has, so the test starts from zero nodes.
fn fresh_backend() -> Option<PipeWireBackend> {
    fresh_sandbox_daemon();
    let mut backend = connect_or_skip()?;
    let _ = backend.stop();
    Some(backend)
}

/// A virtual output strip with the EQ on (i.e. it wants an FX chain).
fn output_eq(uid: &str, volume: f32) -> Strip {
    let mut strip = Strip::with_uid(uid, format!("Out {uid}"), StripType::Output);
    strip.volume = volume;
    strip.effects.get_mut(&EffectKey::Eq).unwrap().active = true;
    strip
}

/// A virtual output strip without any effect (no chain).
fn output_plain(uid: &str, volume: f32) -> Strip {
    let mut strip = Strip::with_uid(uid, format!("Out {uid}"), StripType::Output);
    strip.volume = volume;
    strip
}

fn input_routed(uid: &str, routes: &[&str]) -> Strip {
    let mut strip = Strip::with_uid(uid, format!("In {uid}"), StripType::Input);
    strip.routes = routes.iter().map(|r| r.to_string()).collect();
    strip
}

/// Poll until `check` holds (used for the daemon's own background repair pass,
/// which must work without any client order).
fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    check()
}

// ---------------------------------------------------------------------------
// volume
// ---------------------------------------------------------------------------

/// **The first `apply_snapshot` must apply the volume**, with the PulseAudio
/// curve (`0.44` → `0.44³ = 0.0852`), exactly like the Python engine
/// (`pactl set-sink-volume <node> 44%`).
///
/// Measured before the fix: the first apply left every node at `1.0` (the strip
/// was published in the engine state only at the *end* of the apply, so the
/// volume lookup missed), and when it did apply it wrote the raw slider value —
/// `+14 dB` louder than the Python engine.  Both halves of the "much louder than
/// before, VU meters pinned" report.
#[test]
fn volume_reaches_the_node_on_the_first_apply_with_the_pulseaudio_curve() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP apply_robustness: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    let uid = "vol-live";
    backend
        .apply_snapshot(&[output_plain(uid, 0.44)])
        .expect("apply_snapshot");

    let node = format!("Holaf_Strip_{uid}");
    let read = volumes_of(&node).expect("node must exist right after the first apply");
    let (channel, _monitor) = read;
    assert!(
        !channel.is_empty(),
        "the first apply_snapshot applied no channelVolumes at all (node at unity)"
    );
    // 0.44³ — the value `pactl set-sink-volume … 44%` leaves in PipeWire.
    let expected = 0.44_f32 * 0.44 * 0.44;
    for gain in &channel {
        assert!(
            (gain - expected).abs() < 1e-3,
            "channelVolumes {gain} != {} (0.44³): the volume curve is not the \
             PulseAudio one (a raw 0.44 is +14 dB louder than the Python engine)",
            expected
        );
    }
    // The volume must be applied on the FIRST apply — not only on a second one.
    backend
        .apply_snapshot(&[output_plain(uid, 0.44)])
        .expect("second apply_snapshot");
    let (channel2, _) = volumes_of(&node).expect("node still there");
    assert_eq!(channel, channel2, "the second apply must not change the gain");

    // Unity stays unity (no double conversion).
    backend
        .apply_snapshot(&[output_plain(uid, 1.0)])
        .expect("apply_snapshot (unity)");
    let (channel_max, _) = volumes_of(&node).expect("node still there");
    assert!(
        channel_max.iter().all(|g| (g - 1.0).abs() < 1e-3),
        "volume 1.0 must write a unity gain, got {channel_max:?}"
    );

    backend.stop().expect("stop");
}

/// The monitor tap (`<node>.monitor`, the source the Python VU-meter engine
/// records) has **its own** gain: the Python engine sets it with
/// `pactl set-source-volume <monitor> <pct>%`.  Leaving it at unity made every
/// VU bar read the pre-fader level — the reported "both output meters pinned
/// while the sliders are low".
#[test]
fn volume_also_scales_the_monitor_the_vu_meters_read() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP apply_robustness: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    let uid = "vol-mon";
    backend
        .apply_snapshot(&[output_plain(uid, 0.5)])
        .expect("apply_snapshot");
    let node = format!("Holaf_Strip_{uid}");
    let (channel, monitor) = volumes_of(&node).expect("node exists");
    let expected = 0.5_f32 * 0.5 * 0.5; // −18.06 dB, the PulseAudio 50 %
    assert!(
        channel.iter().all(|g| (g - expected).abs() < 1e-3),
        "channelVolumes {channel:?} != {expected}"
    );
    assert!(
        monitor.iter().all(|g| (g - expected).abs() < 1e-3),
        "monitorVolumes {monitor:?} must follow the strip volume ({expected}): \
         the VU meters read that tap, so a unity monitor shows the pre-fader \
         level (\"meters pinned at low sliders\")"
    );
    backend.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// startup graph construction
// ---------------------------------------------------------------------------

/// A device that is **not in the registry** when the snapshot is applied must
/// not damage the graph, and the graph must heal by itself once the device
/// appears — without any client order.
///
/// Measured before the fix: `apply_snapshot` returned
/// `link_nodes_missing cannot link 'output.Holaf_FX_<uid>' → '<device>'`, so
/// `running` stayed `false`, the strips *after* the failing one were never
/// reconciled, no route was created, the chain of the absent strip was
/// autoconnected by WirePlumber into **another strip's null-sink** (a wrong
/// audio path), and every later apply aborted with `chain_gone`.
#[test]
fn a_missing_device_does_not_abort_the_snapshot_and_heals_when_it_appears() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP apply_robustness: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    // The "USB card" that is not plugged in yet, plus one healthy strip.
    let device = "alsa_output.usb-HOLAF-ABSENT.analog-stereo";
    let mut absent = Strip::with_uid("absent", "Casque USB", StripType::Output);
    absent.device_name = Some(device.to_string());
    absent.volume = 0.44;
    absent.effects.get_mut(&EffectKey::Eq).unwrap().active = true;
    let strips = vec![
        absent,
        output_eq("healthy", 0.44),
        input_routed("feed", &["absent"]),
    ];

    let res = backend
        .apply_snapshot(&strips)
        .expect("apply_snapshot must not fail because a device is missing");
    assert_eq!(res["applied"], 3);
    assert_eq!(res["running"], true, "the engine must still be running");
    assert_eq!(
        res["degraded"], true,
        "a missing device must be reported: {res}"
    );
    let pending = res["pending"].to_string();
    assert!(
        pending.contains(device),
        "status must name the missing device: {pending}"
    );

    // The healthy strip is fully built (chain + intra-strip link) …
    assert!(node_id_by_name("Holaf_Strip_healthy").is_some());
    assert!(node_id_by_name("input.Holaf_FX_healthy").is_some());
    assert!(
        wait_until(Duration::from_secs(2), || {
            port_links_between("output.Holaf_FX_healthy", "Holaf_Strip_healthy").len() == 2
        }),
        "the healthy strip's chain must be wired to its sink"
    );
    // … and the absent strip's chain was **not** built (no module whose
    // `target.object` cannot resolve: that is what made WirePlumber connect it
    // to an unrelated sink).
    assert!(
        node_id_by_name("input.Holaf_FX_absent").is_none(),
        "no FX chain may be created while its target node is missing"
    );
    assert!(
        port_links_between("output.Holaf_FX_absent", "Holaf_Strip_healthy").is_empty(),
        "the absent strip's chain must never be autoconnected into another strip"
    );

    // A second apply must still work (the old code answered `chain_gone`).
    backend
        .apply_snapshot(&strips)
        .expect("a second apply_snapshot must not fail");

    // The device appears (hot-plug).  NO order is sent: the daemon's own repair
    // pass must build the chain and wire it.
    assert!(create_null_sink(device), "test sink creation");
    let healed = wait_until(Duration::from_secs(6), || {
        port_links_between("output.Holaf_FX_absent", device).len() == 2
            && port_links_between("Holaf_Strip_feed", "input.Holaf_FX_absent").len() == 2
    });
    let healed_links = (
        port_links_between("output.Holaf_FX_absent", device),
        port_links_between("Holaf_Strip_feed", "input.Holaf_FX_absent"),
    );
    assert!(
        healed,
        "the graph must heal by itself once the device appears (chain → device \
         and route → chain capture sink); got {healed_links:?}"
    );
    let status = backend.status().expect("status");
    assert_eq!(
        status["degraded"], false,
        "nothing must stay pending after the device appeared: {status}"
    );

    backend.stop().expect("stop");
    destroy_node(device);
}

/// One strip whose graph cannot be built must not abort the whole snapshot: the
/// other strips keep their chains, links and routes.
#[test]
fn a_broken_strip_never_aborts_the_rest_of_the_snapshot() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP apply_robustness: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    let mut broken = Strip::with_uid("broken", "Broken", StripType::Output);
    broken.device_name = Some("alsa_output.usb-HOLAF-BROKEN.analog-stereo".into());
    broken.effects.get_mut(&EffectKey::Eq).unwrap().active = true;
    let strips = vec![
        output_eq("ok1", 1.0),
        broken,
        output_eq("ok2", 1.0),
        input_routed("ok3", &["ok2"]),
    ];
    let res = backend.apply_snapshot(&strips).expect("apply_snapshot");
    assert_eq!(res["applied"], 4);
    assert_eq!(res["running"], true);
    assert_eq!(res["degraded"], true, "{res}");

    // Both healthy output chains are wired, and the route exists.
    for uid in ["ok1", "ok2"] {
        assert!(
            wait_until(Duration::from_secs(2), || {
                port_links_between(
                    &format!("output.Holaf_FX_{uid}"),
                    &format!("Holaf_Strip_{uid}"),
                )
                .len()
                    == 2
            }),
            "strip '{uid}' must keep its chain link even though another strip is broken"
        );
    }
    assert!(
        wait_until(Duration::from_secs(2), || {
            port_links_between("Holaf_Strip_ok3", "input.Holaf_FX_ok2").len() == 2
        }),
        "the route of the healthy strips must be created (it used to be skipped \
         because the snapshot aborted before the routing phase)"
    );

    backend.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// hot toggle
// ---------------------------------------------------------------------------

/// `fx.set_active` must re-derive the topology even when the chain already
/// exists — a toggle is the user's only lever on a graph that was built wrong.
///
/// Measured before the fix: with the chain present, a toggle only pushed
/// parameters, so a missing intra-strip link (or a route left on the raw node)
/// survived every toggle — "if it is broken it stays broken".
#[test]
fn a_toggle_re_derives_the_routes_and_repairs_the_chain_links() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP apply_robustness: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    let uid = "toggle-live";
    let src = "toggle-feed";
    backend
        .apply_snapshot(&[
            output_eq(uid, 1.0),
            input_routed(src, &[uid]),
        ])
        .expect("apply_snapshot");

    let chain_out = format!("output.Holaf_FX_{uid}");
    let sink = format!("Holaf_Strip_{uid}");
    let chain_in = format!("input.Holaf_FX_{uid}");
    let src_node = format!("Holaf_Strip_{src}");
    assert!(wait_until(Duration::from_secs(2), || {
        port_links_between(&chain_out, &sink).len() == 2
            && port_links_between(&src_node, &chain_in).len() == 2
    }));

    // Break the graph the way a failed startup leaves it: no chain→sink link.
    for ch in ["FL", "FR"] {
        run(
            "pw-link",
            &[
                "-d",
                &format!("{chain_out}:output_{ch}"),
                &format!("{sink}:playback_{ch}"),
            ],
        );
    }
    assert_eq!(port_links_between(&chain_out, &sink).len(), 0);

    // Disabling an effect is enough: the toggle re-derives everything.
    backend
        .fx_set_active(uid, EffectKey::Eq, false)
        .expect("fx.set_active false");
    assert!(
        wait_until(Duration::from_secs(3), || {
            port_links_between(&chain_out, &sink).len() == 2
        }),
        "disabling an effect must repair the chain link (the chain is kept while \
         no effect is active, so a toggle is the only lever a user has)"
    );
    assert_eq!(
        port_links_between(&src_node, &chain_in).len(),
        2,
        "the route must be re-derived too (it must stay on the chain capture sink)"
    );

    backend.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// channel wiring
// ---------------------------------------------------------------------------

/// A stereo route must be wired **FL→FL and FR→FR**, exactly two links per
/// route (plus the two of the chain anchor).  A mono source duplicates its left
/// channel onto both destination channels — no more, no less (linking the right
/// channel too would add +6 dB on a correlated signal, which is why both
/// engines do it this way).
#[test]
fn stereo_routes_pair_fl_with_fl_and_fr_with_fr() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP apply_robustness: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    // --- stereo source → stereo sink ---------------------------------------
    backend
        .apply_snapshot(&[
            output_plain("st-out", 1.0),
            input_routed("st-in", &["st-out"]),
        ])
        .expect("apply_snapshot (stereo)");
    assert!(wait_until(Duration::from_secs(2), || {
        port_links_between("Holaf_Strip_st-in", "Holaf_Strip_st-out").len() == 2
    }));
    let mut pairs = port_links_between("Holaf_Strip_st-in", "Holaf_Strip_st-out");
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("monitor_FL".to_string(), "playback_FL".to_string()),
            ("monitor_FR".to_string(), "playback_FR".to_string()),
        ],
        "a stereo route must pair FL→FL and FR→FR (never FL→FR)"
    );

    // --- mono source → stereo sink -----------------------------------------
    let mut mono = Strip::with_uid("mono-in", "Mic mono", StripType::Input);
    mono.is_mono = true;
    mono.routes = vec!["st-out".to_string()];
    backend
        .apply_snapshot(&[output_plain("st-out", 1.0), mono])
        .expect("apply_snapshot (mono)");
    assert!(wait_until(Duration::from_secs(2), || {
        port_links_between("Holaf_Strip_mono-in", "Holaf_Strip_st-out").len() == 2
    }));
    let mut pairs = port_links_between("Holaf_Strip_mono-in", "Holaf_Strip_st-out");
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("monitor_FL".to_string(), "playback_FL".to_string()),
            ("monitor_FL".to_string(), "playback_FR".to_string()),
        ],
        "a mono source must feed both destination channels from its left one \
         (Python parity: `_auto_link_ports(force_mono=True)`), and nothing else"
    );

    backend.stop().expect("stop");
}
