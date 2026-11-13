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
use holaf_pw::{FxHostMode, PipeWireBackend};

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

/// Is the runtime dir the *sandbox* (so restarting it is safe)?
fn sandbox_runtime() -> bool {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("PIPEWIRE_RUNTIME_DIR"))
        .unwrap_or_default();
    dir.contains("pw-runtime") || dir.contains("holaf")
}

/// Restart the headless sandbox daemon so every live test starts from a
/// pristine graph (never touches a real session).
///
/// Measured in this sandbox: PipeWire 0.3.65 stops publishing `Port` globals
/// after many create/destroy cycles on one long-lived daemon, after which every
/// link creation fails ("input null-sink monitor must be linked to the chain
/// capture sink (got 0 links)").  The FX-host tests add chain churn (respawn,
/// restore), so each test now restarts the daemon first, exactly like
/// `apply_robustness.rs` does.
fn fresh_sandbox_daemon() {
    if !sandbox_runtime() {
        return;
    }
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../scripts/start-pipewire-headless.sh"
    );
    let _ = Command::new("bash").arg(script).arg("restart").output();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if run("pw-dump", &[]).is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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

/// Public bus node names of the two test strips: `output_strip` labels its
/// strip "J2 Out", `input_strip` "J2 In" — the bus name follows the **label**,
/// never the uid (see `holaf_core::graph::bus_node_name`).
const OUT_BUS: &str = "Holaf_Bus_J2_Out";
const IN_BUS: &str = "Holaf_Bus_J2_In";

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

    fresh_sandbox_daemon();
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
        names.iter().any(|n| *n == OUT_BUS),
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
    assert!(external_has_node(OUT_BUS), "pw-dump sees the null-sink");
    assert!(external_has_node("input.Holaf_FX_o1-live"), "pw-dump sees the chain input");
    assert!(external_has_node("output.Holaf_FX_o1-live"), "pw-dump sees the chain output");

    // The input null-sink is linked into the chain's capture sink (routing).
    let in_sink = node_id_by_name(IN_BUS).expect("input sink node id");
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
        names.iter().any(|n| *n == OUT_BUS),
        "strip sink must remain"
    );
    assert!(
        !names.iter().any(|n| n.contains("Holaf_FX_o1-live")),
        "chain must be destroyed when eq inactive: {names:?}"
    );
    assert!(!external_has_node("input.Holaf_FX_o1-live"), "pw-dump: chain gone");

    // Routing now targets the raw null-sink of the output strip.
    let out_sink = node_id_by_name(OUT_BUS).expect("output sink node id");
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
    let sources = [IN_BUS.to_string(), format!("output.Holaf_FX_{i1}")];
    let targets = [
        OUT_BUS.to_string(),
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
        OUT_BUS,
        IN_BUS,
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
    fresh_sandbox_daemon();
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
        format!("{IN_BUS}.monitor"),
        "a virtual strip is metered on its null-sink monitor"
    );
    assert_eq!(
        sources[vout],
        format!("{OUT_BUS}.monitor"),
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
        format!("{IN_BUS}.monitor"),
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
/// With a chain hosted **in this process** ([`FxHostMode::InProcess`]) and no
/// order issued at all, the idle pump counter must keep growing (the worker is
/// servicing the mainloop instead of blocking on the channel); without a chain
/// it must stay frozen (the worker blocks and costs 0 % CPU).  Both directions
/// are checked so the test cannot be satisfied by a busy loop.
///
/// The **default** hosting mode delegates all of this to a child process; see
/// [`dedicated_fx_host_owns_the_mainloop_and_respawns`], which asserts the
/// mirror-image invariant (the daemon does *not* pump, the child is alive).
#[test]
fn in_process_hosting_pumps_the_mainloop_while_the_daemon_is_idle() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP backend_live: no headless PipeWire configured (set XDG_RUNTIME_DIR to the headless runtime or HOLAF_PW_LIVE=1)");
        return;
    }
    fresh_sandbox_daemon();
    let mut backend = match PipeWireBackend::connect_with(FxHostMode::InProcess) {
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

// ---------------------------------------------------------------------------
// The dedicated FX host: who services the chain, and what happens when it dies
// ---------------------------------------------------------------------------

/// Is `pid` a live process (and not a zombie)?
fn pid_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            // Field 3 (after the parenthesised comm) is the state letter.
            let state = stat
                .rsplit(") ")
                .next()
                .and_then(|tail| tail.chars().next());
            !matches!(state, Some('Z') | None)
        }
        Err(_) => false,
    }
}

fn fx_host_status(backend: &mut PipeWireBackend) -> serde_json::Value {
    backend.status().expect("status")["fx_host"].clone()
}

/// The default hosting mode must put the chain in a **dedicated child process**
/// and leave the daemon's own mainloop idle.
///
/// # Why this is the fix
///
/// A `module-filter-chain` lives inside the client process that loaded it: its
/// streams are made schedulable by that process's mainloop and its `process`
/// callbacks run on its data loop.  Hosting it in `holafd` therefore mixes the
/// chain with IPC, reconciliation, repair and logging on the same thread — the
/// reported "chops only in Rust mode, on a USB device" symptom.  A dedicated
/// process that does nothing but run `pw_main_loop_run()` is exactly what the
/// Python engine has always done with `pw-cli`, and it never chopped.
///
/// # What is asserted
///
/// 1. `status.fx_host.mode == "external"` with a **live** child pid, and the
///    chains listed there;
/// 2. during 400 ms of complete inactivity the daemon performs **no** idle
///    pumping (the child owns the loop now) while the child stays alive;
/// 3. the chain is really usable from the daemon's side (its flat `Props` can
///    still be written through the daemon-side node, i.e. hot-reload works
///    across the process boundary);
/// 4. killing the child makes the next order respawn it and restore the chain
///    **without the user doing anything** (same chain uids, live pid again).
#[test]
fn dedicated_fx_host_owns_the_mainloop_and_respawns() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP backend_live: no headless PipeWire configured (set XDG_RUNTIME_DIR to the headless runtime or HOLAF_PW_LIVE=1)");
        return;
    }
    fresh_sandbox_daemon();
    let mut backend = match PipeWireBackend::connect() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("SKIP backend_live: cannot reach a PipeWire daemon: {e}");
            return;
        }
    };
    let _ = backend.stop();

    let uid = "fxhost-live";
    backend
        .apply_snapshot(&[output_strip(uid, true)])
        .expect("apply_snapshot with FX");

    // 1) the chain is hosted by a live child, not by us.
    let host = fx_host_status(&mut backend);
    assert_eq!(host["mode"], "external", "default hosting must be external: {host}");
    let pid = host["pid"].as_u64().expect("status.fx_host.pid") as u32;
    assert!(pid_alive(pid), "the FX host (pid {pid}) must be running");
    assert_ne!(pid, std::process::id(), "the FX host must be another process");
    let chains: Vec<String> = host["chains"]
        .as_array()
        .expect("status.fx_host.chains")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(chains, vec![uid.to_string()], "the chain must be listed");

    // The chain's stream nodes exist in the daemon (hosted elsewhere, wired
    // here) and the intra-strip link was made.
    let names: Vec<String> = backend.status().expect("status")["holaf_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == &format!("input.Holaf_FX_{uid}")),
        "chain capture node missing: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == &format!("output.Holaf_FX_{uid}")),
        "chain playback node missing: {names:?}"
    );

    // 2) the daemon must NOT be servicing a chain while idle any more.
    let before = mainloop_pumps(&mut backend);
    std::thread::sleep(Duration::from_millis(400));
    let after = mainloop_pumps(&mut backend);
    assert_eq!(
        before, after,
        "the daemon pumped its mainloop {} time(s) while idle: with a dedicated FX host          it has no chain to service and must block on its order channel",
        after - before
    );
    assert!(pid_alive(pid), "the FX host must have kept running while idle");

    // 3) hot-reload still crosses the process boundary.
    let params = holaf_core::fx::default_params(EffectKey::Eq);
    backend
        .fx_set_params(uid, EffectKey::Eq, &params)
        .expect("fx_set_params reaches the externally hosted chain");

    // 4) the child dies → the next order respawns it and restores the chain.
    let killed = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill the FX host");
    assert!(killed.success());
    // Wait for the kernel to reap it, then poke the daemon: `apply_snapshot`
    // is what a UI order looks like, and it must heal without any special case.
    for _ in 0..100 {
        if !pid_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let params2 = holaf_core::fx::default_params(EffectKey::Eq);
    backend
        .apply_snapshot(&[output_strip(uid, true)])
        .expect("apply_snapshot after the FX host died");
    let _ = backend.fx_set_params(uid, EffectKey::Eq, &params2);

    let host = fx_host_status(&mut backend);
    // Note: the *pid* is deliberately not compared — a container can hand the
    // same pid to the replacement process.  The respawn counter, the restored
    // chain list and a live child are what actually prove the recovery.
    let pid2 = host["pid"].as_u64().expect("status.fx_host.pid") as u32;
    assert!(pid_alive(pid2), "the respawned FX host must be running");
    assert_eq!(host["mode"], "external", "{host}");
    assert!(
        host["respawns"].as_u64().unwrap_or(0) >= 1,
        "the respawn must be counted: {host}"
    );
    let chains: Vec<String> = host["chains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        chains,
        vec![uid.to_string()],
        "the chain must have been restored in the new host: {host}"
    );

    // Teardown must leave no FX host behind.
    backend.stop().expect("stop");
    assert!(!pid_alive(pid2), "the FX host (pid {pid2}) must not outlive stop()");
}
