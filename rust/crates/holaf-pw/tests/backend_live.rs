//! Live integration tests for the real PipeWire backend.
//!
//! These tests require a **reachable headless PipeWire daemon**; they are
//! skipped (with a clear stderr note) when none is configured.  In the CI
//! sandbox, launch the headless environment first:
//!
//! ```bash
//! bash /opt/holaf-headless/start-pipewire-headless.sh
//! export XDG_RUNTIME_DIR=/tmp/pw-runtime PIPEWIRE_RUNTIME_DIR=/tmp/pw-runtime \
//!        DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/pw-runtime/bus
//! cd rust && cargo test -p holaf-pw --test backend_live
//! ```
//!
//! The gate is intentionally strict so `cargo test --workspace` never touches
//! a real user daemon by accident: the runtime dir must point at the headless
//! sandbox (`/tmp/pw-runtime` / `…holaf…`) or `HOLAF_PW_LIVE=1` must be set.

use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use holaf_core::backend::Backend;
use holaf_core::model::{EffectKey, Strip, StripType};
use holaf_pw::PipeWireBackend;

/// Both live tests drive the *same* daemon (one shared graph, `stop()` between
/// phases), so they must never overlap: cargo runs tests in parallel by
/// default.  Acquire this guard first in every live test.
fn live_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Is the test allowed to talk to a live PipeWire daemon?
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

/// Parse `pw-dump` into a JSON array of registry objects.
fn pw_dump() -> Option<serde_json::Value> {
    let text = run("pw-dump", &[])?;
    serde_json::from_str(&text).ok()
}

/// Global id of the first object whose `node.name` == `name`, via pw-dump.
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

fn external_has_node(name: &str) -> bool {
    node_id_by_name(name).is_some()
}

/// Count links whose endpoints are the two given node ids (pw-dump Link globals).
fn link_count(out_node: u32, in_node: u32) -> usize {
    let Some(dump) = pw_dump() else {
        return 0;
    };
    dump.as_array()
        .map(|arr| {
            arr.iter()
                .filter(|o| {
                    o.get("type").and_then(|t| t.as_str()) == Some("PipeWire:Interface:Link")
                        && o.get("info")
                            .and_then(|i| i.get("props"))
                            .and_then(|p| p.get("link.output.node"))
                            .and_then(|v| v.as_u64())
                            == Some(out_node as u64)
                        && o.get("info")
                            .and_then(|i| i.get("props"))
                            .and_then(|p| p.get("link.input.node"))
                            .and_then(|v| v.as_u64())
                            == Some(in_node as u64)
                })
                .count()
        })
        .unwrap_or(0)
}

/// Number of links between *any* node of `from` and *any* node of `to`, by node
/// name (used to prove that a topology change leaves no stale route behind).
fn link_count_between(from: &[&str], to: &[&str]) -> usize {
    let Some(dump) = pw_dump() else { return 0 };
    let Some(arr) = dump.as_array() else { return 0 };
    let name_of = |id: &serde_json::Value| -> Option<String> {
        let wanted = id.as_u64()?;
        arr.iter().find_map(|o| {
            if o.get("id")?.as_u64()? != wanted {
                return None;
            }
            Some(o.get("info")?.get("props")?.get("node.name")?.as_str()?.to_string())
        })
    };
    arr.iter()
        .filter(|o| o.get("type").and_then(|t| t.as_str()) == Some("PipeWire:Interface:Link"))
        .filter(|o| {
            let props = o.get("info").and_then(|i| i.get("props"));
            let (Some(out), Some(inn)) = (
                props.and_then(|p| p.get("link.output.node")),
                props.and_then(|p| p.get("link.input.node")),
            ) else {
                return false;
            };
            let (Some(out), Some(inn)) = (name_of(out), name_of(inn)) else {
                return false;
            };
            from.contains(&out.as_str()) && to.contains(&inn.as_str())
        })
        .count()
}

/// Read the current value of an EQ band back from the daemon via
/// `pw-cli enum-params` on the given chain stream node.
fn eq_band_value(node_id: u32, band: &str) -> Option<f32> {
    let text = run("pw-cli", &["enum-params", &node_id.to_string(), "Props"])?;
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.contains(band) {
            if let Some(next) = lines.get(i + 1) {
                if next.contains("Float") {
                    let tok = next
                        .split_whitespace()
                        .find(|w| w.parse::<f32>().is_ok())?;
                    return tok.parse().ok();
                }
            }
        }
    }
    None
}

fn output_strip(uid: &str, eq_active: bool) -> Strip {
    let mut strip = Strip::with_uid(uid, "J2 Out", StripType::Output);
    strip.effects.get_mut(&EffectKey::Eq).unwrap().active = eq_active;
    strip
}

fn input_strip(uid: &str, routes: &[&str]) -> Strip {
    let mut strip = Strip::with_uid(uid, "J2 In", StripType::Input);
    strip.routes = routes.iter().map(|r| r.to_string()).collect();
    strip
}

#[test]
fn apply_snapshot_creates_chain_and_reads_back_eq_params() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP backend_live: no headless PipeWire configured (set XDG_RUNTIME_DIR to the headless runtime or HOLAF_PW_LIVE=1)");
        return;
    }

    let mut backend = match PipeWireBackend::connect() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("SKIP backend_live: cannot reach a PipeWire daemon: {e}");
            return;
        }
    };

    // Fresh start: guarantee no debris from a previous run.
    let _ = backend.stop();

    let o1 = "o1-live";
    let i1 = "i1-live";
    let strips = vec![
        output_strip(o1, true),
        input_strip(i1, &[o1]),
    ];
    let res = backend.apply_snapshot(&strips).expect("apply_snapshot");
    assert_eq!(res["applied"], 2, "both strips applied");

    // 1) null-sink + FX chain exist (backend status + external pw-dump).
    let status = backend.status().expect("status");
    let nodes = status["holaf_nodes"].as_array().unwrap();
    let names: Vec<&str> = nodes
        .iter()
        .filter_map(|n| n.as_str())
        .collect();
    assert!(
        names.iter().any(|n| *n == "Holaf_Strip_o1-live"),
        "null-sink not in status holaf_nodes: {names:?}"
    );
    assert!(
        names.iter().any(|n| *n == "input.Holaf_FX_o1-live"),
        "chain input stream not in status: {names:?}"
    );
    assert!(
        names.iter().any(|n| *n == "output.Holaf_FX_o1-live"),
        "chain output stream not in status: {names:?}"
    );

    // Give the daemon a moment then cross-check externally via pw-dump.
    std::thread::sleep(Duration::from_millis(300));
    assert!(external_has_node("Holaf_Strip_o1-live"), "pw-dump sees the null-sink");
    assert!(external_has_node("input.Holaf_FX_o1-live"), "pw-dump sees the chain input");
    assert!(external_has_node("output.Holaf_FX_o1-live"), "pw-dump sees the chain output");

    // The input null-sink is linked into the chain's capture sink (routing).
    let in_sink = node_id_by_name("Holaf_Strip_i1-live").expect("input sink node id");
    let chain_sink = node_id_by_name("input.Holaf_FX_o1-live").expect("chain capture node id");
    let route_links = link_count(in_sink, chain_sink);
    assert!(
        route_links >= 2,
        "input null-sink monitor must be linked to the chain capture sink (got {route_links} links)"
    );

    // 2) fx_set_params changes an EQ band and the daemon reflects it.
    let mut params = holaf_core::fx::default_params(EffectKey::Eq);
    params.insert("100Hz gain".into(), -3.5);
    let fx = backend
        .fx_set_params(o1, EffectKey::Eq, &params)
        .expect("fx_set_params");
    assert_eq!(fx["params"]["100Hz gain"], -3.5);

    std::thread::sleep(Duration::from_millis(300));
    let chain_in_id = node_id_by_name("input.Holaf_FX_o1-live").expect("chain in id for read-back");
    let read_back = eq_band_value(chain_in_id, "100Hz gain").expect("read-back of the band");
    let read_left = eq_band_value(chain_in_id, "eq_L:100Hz gain").unwrap_or(read_back);
    assert!(
        (read_left - (-3.5)).abs() < 0.05,
        "EQ band not applied on the daemon (read {read_left})"
    );

    // 3) apply_snapshot with EQ inactive → the chain is destroyed (obsolete),
    //    the null-sink stays (virtual strip).
    let mut no_eq = output_strip(o1, false);
    no_eq.volume = 0.0; // touch another field to exercise set_volume later
    let res = backend
        .apply_snapshot(&[no_eq.clone(), input_strip(i1, &[o1])])
        .expect("apply_snapshot (eq inactive)");
    assert_eq!(res["applied"], 2);
    std::thread::sleep(Duration::from_millis(300));

    let status = backend.status().expect("status");
    let names: Vec<&str> = status["holaf_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n.as_str())
        .collect();
    assert!(
        names.iter().any(|n| *n == "Holaf_Strip_o1-live"),
        "strip sink must remain"
    );
    assert!(
        !names.iter().any(|n| n.contains("Holaf_FX_o1-live")),
        "chain must be destroyed when eq inactive: {names:?}"
    );
    assert!(!external_has_node("input.Holaf_FX_o1-live"), "pw-dump: chain gone");

    // Routing now targets the raw null-sink of the output strip.
    let out_sink = node_id_by_name("Holaf_Strip_o1-live").expect("output sink node id");
    let direct_links = link_count(in_sink, out_sink);
    assert!(
        direct_links >= 2,
        "input must be re-linked directly to the (chain-less) output null-sink (got {direct_links})"
    );

    // 3b) Re-enabling the chain while the route stays must NOT leave the
    //     previous (chain-less) direct link behind: the strip would otherwise
    //     feed the output through two paths at once (doubled level, comb
    //     filtering).
    let res = backend
        .apply_snapshot(&[output_strip(o1, true), input_strip(i1, &[o1])])
        .expect("apply_snapshot (eq active again)");
    assert_eq!(res["applied"], 2);
    std::thread::sleep(Duration::from_millis(400));
    let sources = [format!("Holaf_Strip_{i1}"), format!("output.Holaf_FX_{i1}")];
    let targets = [
        format!("Holaf_Strip_{o1}"),
        format!("input.Holaf_FX_{o1}"),
    ];
    let sources_ref: Vec<&str> = sources.iter().map(String::as_str).collect();
    let targets_ref: Vec<&str> = targets.iter().map(String::as_str).collect();
    let routed = link_count_between(&sources_ref, &targets_ref);
    assert_eq!(
        routed, 2,
        "a route must be exactly one stereo pair of links, never the stale one plus the new one          (got {routed}: {sources_ref:?} -> {targets_ref:?})"
    );

    // 4) volume / mute / default-sink are accepted.
    backend.set_volume(o1, 0.5).expect("set_volume");
    backend.set_mute(o1, true).expect("set_mute");
    let _ = backend.set_default_sink(o1); // metadata write; ignore headless quirks

    // 5) Cleanup must be complete: zero Holaf debris left.
    backend.stop().expect("stop");
    std::thread::sleep(Duration::from_millis(300));
    for name in [
        "Holaf_Strip_o1-live",
        "Holaf_Strip_i1-live",
        "input.Holaf_FX_o1-live",
        "output.Holaf_FX_o1-live",
    ] {
        assert!(
            !external_has_node(name),
            "cleanup left debris node '{name}'"
        );
    }
}

/// `pactl list short sources` → the source names the PulseAudio API exposes.
/// `None` when no `pipewire-pulse` server (or no `pactl`) is reachable, in
/// which case the external cross-check below is skipped: the plain PipeWire
/// daemon alone does not publish pulse-level names.
fn pulse_source_names() -> Option<Vec<String>> {
    let text = run("pactl", &["list", "short", "sources"])?;
    let names: Vec<String> = text
        .lines()
        .filter_map(|line| line.split('\t').nth(1).map(str::to_string))
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

/// `meter.sources` must hand the Python VU-meter engine names it can actually
/// record from (`PULSE_SOURCE=<name>`): a `<node>.monitor` source for every
/// virtual strip (chain or not), the device source for a physical input — and
/// each name must exist as a pulse source once the graph is built.
#[test]
fn meter_sources_expose_recordable_pulse_sources() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP backend_live: no headless PipeWire configured (set XDG_RUNTIME_DIR to the headless runtime or HOLAF_PW_LIVE=1)");
        return;
    }
    let mut backend = match PipeWireBackend::connect() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("SKIP backend_live: cannot reach a PipeWire daemon: {e}");
            return;
        }
    };
    let _ = backend.stop();

    let vin = "meter-in"; // virtual input, EQ active → chain exports a source
    let vout = "meter-out"; // virtual output → null-sink monitor
    let mut in_strip = input_strip(vin, &[]);
    in_strip.effects.get_mut(&EffectKey::Eq).unwrap().active = true;
    let out_strip = output_strip(vout, true);
    backend
        .apply_snapshot(&[in_strip, out_strip])
        .expect("apply_snapshot");

    let res = backend.meter_sources().expect("meter.sources");
    assert_eq!(res["running"], true, "engine must report running: {res:?}");
    let sources = res["sources"].as_object().expect("sources object");
    assert_eq!(
        sources[vin],
        format!("Holaf_Strip_{vin}.monitor"),
        "a virtual strip is metered on its null-sink monitor"
    );
    assert_eq!(
        sources[vout],
        format!("Holaf_Strip_{vout}.monitor"),
        "an output strip is metered on its master node's monitor"
    );

    // The names must be real: every metered source exists in the pulse source
    // list (the exact API the Python metering uses through PortAudio).
    std::thread::sleep(Duration::from_millis(400));
    match pulse_source_names() {
        Some(names) => {
            for (uid, name) in sources {
                let name = name.as_str().unwrap();
                assert!(
                    names.iter().any(|n| n == name),
                    "meter source '{name}' (strip '{uid}') is not a pulse source: {names:?}"
                );
            }
        }
        None => eprintln!(
            "SKIP pulse cross-check: no pipewire-pulse server / pactl available \
             (names asserted structurally only)"
        ),
    }

    // The mapping is chain-independent: unloading the chain must not change
    // the metered source (the null-sink stays the strip's signal entry point).
    let mut no_eq = input_strip(vin, &[]);
    no_eq.effects.get_mut(&EffectKey::Eq).unwrap().active = false;
    backend
        .apply_snapshot(&[no_eq, output_strip(vout, false)])
        .expect("apply_snapshot (no fx)");
    let res = backend.meter_sources().expect("meter.sources (no fx)");
    assert_eq!(
        res["sources"][vin],
        format!("Holaf_Strip_{vin}.monitor"),
        "the metered source survives a chain teardown"
    );

    backend.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// Regression: the daemon must service its PipeWire mainloop while it is IDLE
// ---------------------------------------------------------------------------

/// Number of mainloop iterations the daemon performed while idle (status).
fn mainloop_pumps(backend: &mut PipeWireBackend) -> u64 {
    backend.status().expect("status")["mainloop_pumps"]
        .as_u64()
        .expect("status.mainloop_pumps")
}

/// The daemon must keep servicing its PipeWire mainloop **while no IPC order is
/// in flight**.
///
/// # The bug this guards
///
/// The filter-chains live in this process: `module-filter-chain` is loaded into
/// our own `pw_context`, so its two client nodes (capture/playback streams) are
/// negotiated, activated and kept alive by mainloop traffic — only then does
/// their RT `process` callback (libpipewire's data-loop thread) get scheduled.
/// The Python engine never hit this because it hosts chains in a dedicated
/// `pw-cli` process whose whole job is running its mainloop.
///
/// When the backend worker loop blocked on its order channel (`recv()`), the
/// chain was serviced *only inside* IPC orders.  Measured on the headless
/// 0.3.65 sandbox: with the loop blocked, a playback stream connecting to
/// `input.Holaf_FX_<uid>` stayed in `connecting` forever (the graph never
/// started); pumped only by a 25 Hz `status` poll, the chain ran but was
/// starved between polls — on a timing-sensitive device (USB card with 1 ms
/// packets) that is heard as a continuous fast on/off chopping, while a
/// null-sink tolerates the gaps, which is exactly why the symptom looked
/// "device-specific" (headphones crackle, speakers do not).
///
/// # What is asserted
///
/// With a chain hosted and **no order issued at all**, the idle pump counter
/// must keep growing (the worker is servicing the mainloop instead of blocking
/// on the channel); without a chain it must stay frozen (the worker blocks and
/// costs 0 % CPU).  Both directions are checked so the test cannot be satisfied
/// by a busy loop.
#[test]
fn mainloop_is_pumped_while_the_daemon_is_idle() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP backend_live: no headless PipeWire configured (set XDG_RUNTIME_DIR to the headless runtime or HOLAF_PW_LIVE=1)");
        return;
    }
    let mut backend = match PipeWireBackend::connect() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("SKIP backend_live: cannot reach a PipeWire daemon: {e}");
            return;
        }
    };
    let _ = backend.stop();

    // --- with a hosted chain: the mainloop must be serviced -----------------
    let uid = "idle-live";
    backend
        .apply_snapshot(&[output_strip(uid, true)])
        .expect("apply_snapshot");
    let before = mainloop_pumps(&mut backend);
    // Nothing talks to the daemon during this window — exactly the state the
    // user is in when the UI is idle and a chain keeps playing.
    std::thread::sleep(Duration::from_millis(400));
    let after = mainloop_pumps(&mut backend);
    assert!(
        after - before >= 20,
        "the daemon serviced its PipeWire mainloop only {} time(s) in 400 ms with a \
         filter-chain loaded: a hosted chain is starved outside IPC orders \
         ('FX chain crackles / chops only in Rust mode')",
        after - before
    );

    // --- with no chain hosted: block on the channel, cost 0 % CPU -----------
    backend.stop().expect("stop");
    let idle_before = mainloop_pumps(&mut backend);
    std::thread::sleep(Duration::from_millis(400));
    let idle_after = mainloop_pumps(&mut backend);
    assert_eq!(
        idle_before, idle_after,
        "with no chain hosted the worker must block on its order channel (no idle pumping)"
    );
}
