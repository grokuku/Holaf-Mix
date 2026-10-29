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
use std::time::Duration;

use holaf_core::backend::Backend;
use holaf_core::model::{EffectKey, Strip, StripType};
use holaf_pw::PipeWireBackend;

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
