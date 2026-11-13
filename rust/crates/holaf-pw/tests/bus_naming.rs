//! Live tests for the **public bus naming convention** (see
//! `holaf_core::graph` module docs).
//!
//! What a strip is called must survive the strip itself: an external program
//! (browser, `pactl`, JACK client, the user's scripts) targets a bus by name, so
//! deleting a strip in the UI and creating it again must yield the same name.
//! The name is therefore derived from the **label** (`"Mix Out"` →
//! `Holaf_Bus_Mix_Out`), while the uid lives on in the `holaf.uid` registry
//! property — the identity the reconciler, the rename detection and the teardown
//! actually use.
//!
//! | test | property pinned |
//! |---|---|
//! | [`bus_names_follow_the_label_and_never_collide`] | label slug + `_2` collision suffix, `status.strips[].node_name` |
//! | [`a_second_apply_never_duplicates_the_bus`] | re-applying the same snapshot is idempotent (no orphan bus, no duplicate) |
//! | [`renaming_a_strip_rebuilds_its_bus_and_keeps_the_routes`] | rename → old bus gone, new bus created, routes re-derived |
//! | [`removing_a_renamed_strip_destroys_its_bus`] | teardown finds the node by `holaf.uid`, not by a planned name |
//! | [`a_legacy_strip_node_is_replaced_not_orphaned`] | upgrade path: the old `Holaf_Strip_<uid>` node is destroyed |
//!
//! They share the live gate of `backend_live.rs`: they only touch a daemon whose
//! runtime dir looks like the headless sandbox, or when `HOLAF_PW_LIVE=1` is set.

use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use holaf_core::backend::Backend;
use holaf_core::graph;
use holaf_core::model::{EffectKey, Strip, StripType};
use holaf_pw::PipeWireBackend;

// ---------------------------------------------------------------------------
// harness (same recipe as `apply_robustness.rs`)
// ---------------------------------------------------------------------------

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

fn sandbox_runtime() -> bool {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("PIPEWIRE_RUNTIME_DIR"))
        .unwrap_or_default();
    dir.contains("pw-runtime") || dir.contains("holaf")
}

/// Restart the headless sandbox daemon (never a real session — see
/// `apply_robustness.rs` for why every test starts from a pristine graph).
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

fn fresh_backend() -> Option<PipeWireBackend> {
    fresh_sandbox_daemon();
    let mut backend = match PipeWireBackend::connect() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("SKIP bus_naming: cannot reach a PipeWire daemon: {e}");
            return None;
        }
    };
    let _ = backend.stop();
    Some(backend)
}

/// Every live node (name, uid marker) of the daemon — read from `pw-dump`, i.e.
/// exactly what an external program sees.
fn live_nodes() -> Vec<(String, Option<String>)> {
    let Some(dump) = pw_dump() else {
        return Vec::new();
    };
    let Some(arr) = dump.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter(|o| o.get("type").and_then(|t| t.as_str()) == Some("PipeWire:Interface:Node"))
        .filter_map(|o| {
            let props = o.get("info")?.get("props")?;
            Some((
                props.get("node.name")?.as_str()?.to_string(),
                props
                    .get(graph::UID_PROP)
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            ))
        })
        .collect()
}

/// Names of the live nodes owned by strip `uid` (whatever they are called).
fn nodes_of_uid(uid: &str) -> Vec<String> {
    live_nodes()
        .into_iter()
        .filter(|(_, owner)| owner.as_deref() == Some(uid))
        .map(|(name, _)| name)
        .collect()
}

fn has_node(name: &str) -> bool {
    live_nodes().iter().any(|(n, _)| n == name)
}

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

/// `(port.name, port.name)` of every live link between two node names.
fn port_links_between(from: &str, to: &str) -> Vec<(String, String)> {
    let Some(dump) = pw_dump() else {
        return Vec::new();
    };
    let Some(arr) = dump.as_array() else {
        return Vec::new();
    };
    let name_of = |id: u64| -> Option<String> {
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
            if name_of(out_node)? != from || name_of(in_node)? != to {
                return None;
            }
            Some((
                port_name(props.get("link.output.port")?.as_u64()?)?,
                port_name(props.get("link.input.port")?.as_u64()?)?,
            ))
        })
        .collect()
}

/// An output strip without effects (a plain bus, no FX chain).
fn out_strip(uid: &str, label: &str) -> Strip {
    Strip::with_uid(uid, label, StripType::Output)
}

/// An input strip routed to `target`.
fn in_strip(uid: &str, label: &str, target: &str) -> Strip {
    let mut strip = Strip::with_uid(uid, label, StripType::Input);
    strip.routes = vec![target.to_string()];
    strip
}

/// `status.strips[uid].node_name` — the public handle the daemon publishes.
fn reported_node_name(backend: &mut PipeWireBackend, uid: &str) -> Option<String> {
    let status = backend.status().ok()?;
    status["strips"]
        .as_array()?
        .iter()
        .find(|s| s["uid"] == uid)?
        .get("node_name")?
        .as_str()
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// naming
// ---------------------------------------------------------------------------

/// The bus name is the label's slug; two strips sharing a label get a
/// deterministic `_2` suffix; the daemon publishes the name it used.
#[test]
fn bus_names_follow_the_label_and_never_collide() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP bus_naming: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    backend
        .apply_snapshot(&[
            out_strip("name-a", "Mix Out"),
            out_strip("name-b", "Mix Out"),
            in_strip("name-c", "Desktop", "name-a"),
        ])
        .expect("apply_snapshot");

    assert!(
        wait_until(Duration::from_secs(3), || has_node("Holaf_Bus_Mix_Out")
            && has_node("Holaf_Bus_Mix_Out_2")
            && has_node("Holaf_Bus_Desktop")),
        "expected label-derived bus names, got {:?}",
        live_nodes()
    );
    // The public handle is published per strip (clients must not rebuild it).
    assert_eq!(
        reported_node_name(&mut backend, "name-a").as_deref(),
        Some("Holaf_Bus_Mix_Out")
    );
    assert_eq!(
        reported_node_name(&mut backend, "name-b").as_deref(),
        Some("Holaf_Bus_Mix_Out_2")
    );
    assert_eq!(
        reported_node_name(&mut backend, "name-c").as_deref(),
        Some("Holaf_Bus_Desktop")
    );
    // …and the daemon's own report matches the registry, one node per strip.
    for (uid, name) in [
        ("name-a", "Holaf_Bus_Mix_Out"),
        ("name-b", "Holaf_Bus_Mix_Out_2"),
        ("name-c", "Holaf_Bus_Desktop"),
    ] {
        assert_eq!(nodes_of_uid(uid), vec![name.to_string()], "strip {uid}");
    }
    // The route uses the label-derived names (no uid anywhere in the path).
    assert!(
        wait_until(Duration::from_secs(3), || {
            port_links_between("Holaf_Bus_Desktop", "Holaf_Bus_Mix_Out").len() == 2
        }),
        "the route must be wired between the two bus names, got {:?}",
        live_nodes()
    );

    backend.stop().expect("stop");
}

/// Re-applying the same snapshot (what the UI does on every edit) must not
/// create a second bus for the same strip: the node is recognised by its
/// `holaf.uid` marker, not by a freshly computed name.
#[test]
fn a_second_apply_never_duplicates_the_bus() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP bus_naming: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    let strips = vec![out_strip("dup-a", "Mix Out"), in_strip("dup-b", "Desk", "dup-a")];
    backend.apply_snapshot(&strips).expect("first apply");
    assert!(wait_until(Duration::from_secs(3), || !nodes_of_uid("dup-a").is_empty()));

    for round in 2..=3 {
        backend
            .apply_snapshot(&strips)
            .unwrap_or_else(|e| panic!("apply #{round} failed: {}", e.message));
        assert_eq!(
            nodes_of_uid("dup-a"),
            vec!["Holaf_Bus_Mix_Out".to_string()],
            "apply #{round} duplicated the bus of strip 'dup-a': {:?}",
            live_nodes()
        );
        assert_eq!(
            nodes_of_uid("dup-b"),
            vec!["Holaf_Bus_Desk".to_string()],
            "apply #{round} duplicated the bus of strip 'dup-b'"
        );
    }

    backend.stop().expect("stop");
}

// ---------------------------------------------------------------------------
// rename / teardown
// ---------------------------------------------------------------------------

/// Renaming a strip in the UI follows the label: the old bus is destroyed (with
/// its links) and the new one is created — and the graph is re-derived, so the
/// routes and the FX chain anchor follow the new name.
#[test]
fn renaming_a_strip_rebuilds_its_bus_and_keeps_the_routes() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP bus_naming: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    backend
        .apply_snapshot(&[
            out_strip("ren-out", "Mix Out"),
            in_strip("ren-in", "Desktop", "ren-out"),
        ])
        .expect("apply_snapshot");
    assert!(wait_until(Duration::from_secs(3), || {
        port_links_between("Holaf_Bus_Desktop", "Holaf_Bus_Mix_Out").len() == 2
    }));

    // Rename, keeping the same uid (exactly what the UI does).
    backend
        .apply_snapshot(&[
            out_strip("ren-out", "Bus Principal"),
            in_strip("ren-in", "Desktop", "ren-out"),
        ])
        .expect("apply_snapshot (renamed)");

    assert!(
        wait_until(Duration::from_secs(3), || has_node("Holaf_Bus_Bus_Principal")),
        "the renamed strip must publish its new bus name, got {:?}",
        live_nodes()
    );
    assert!(
        !has_node("Holaf_Bus_Mix_Out"),
        "the old bus must be destroyed (no orphan), got {:?}",
        live_nodes()
    );
    assert_eq!(
        nodes_of_uid("ren-out"),
        vec!["Holaf_Bus_Bus_Principal".to_string()],
        "exactly one bus per strip"
    );
    assert_eq!(
        reported_node_name(&mut backend, "ren-out").as_deref(),
        Some("Holaf_Bus_Bus_Principal")
    );
    // The route followed the rename (it is re-derived from the plan).
    assert!(
        wait_until(Duration::from_secs(3), || {
            port_links_between("Holaf_Bus_Desktop", "Holaf_Bus_Bus_Principal").len() == 2
        }),
        "the route must be wired to the renamed bus, got {:?}",
        live_nodes()
    );

    backend.stop().expect("stop");
}

/// A chain anchored to a bus must follow a rename too: its `target.object` is
/// the old name, so it has to be rebuilt against the new one.
#[test]
fn renaming_a_strip_with_an_fx_chain_moves_the_anchor() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP bus_naming: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    let mut out = out_strip("fxren", "Mix Out");
    out.effects.get_mut(&EffectKey::Eq).unwrap().active = true;
    backend
        .apply_snapshot(&[out.clone()])
        .expect("apply_snapshot with FX");
    assert!(wait_until(Duration::from_secs(3), || {
        port_links_between("output.Holaf_FX_fxren", "Holaf_Bus_Mix_Out").len() == 2
    }));

    let mut renamed = out.clone();
    renamed.label = "Mix Final".into();
    backend
        .apply_snapshot(&[renamed])
        .expect("apply_snapshot (renamed with FX)");

    assert!(
        wait_until(Duration::from_secs(4), || {
            port_links_between("output.Holaf_FX_fxren", "Holaf_Bus_Mix_Final").len() == 2
        }),
        "the chain must be re-anchored to the renamed bus, got {:?}",
        live_nodes()
    );
    assert!(
        !has_node("Holaf_Bus_Mix_Out"),
        "the old bus must be gone: {:?}",
        live_nodes()
    );

    backend.stop().expect("stop");
}

/// Removing a strip whose label changed since the last plan must still destroy
/// its bus: the node is found through `holaf.uid`.
#[test]
fn removing_a_renamed_strip_destroys_its_bus() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP bus_naming: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    backend
        .apply_snapshot(&[
            out_strip("keep", "Keep"),
            out_strip("gone", "Éphémère"),
        ])
        .expect("apply_snapshot");
    assert!(
        wait_until(Duration::from_secs(3), || has_node("Holaf_Bus_Ephemere")),
        "the accented label must slug to ASCII, got {:?}",
        live_nodes()
    );

    backend
        .apply_snapshot(&[out_strip("keep", "Keep")])
        .expect("apply_snapshot without the strip");
    assert!(
        wait_until(Duration::from_secs(3), || nodes_of_uid("gone").is_empty()),
        "the removed strip's bus must be destroyed, got {:?}",
        live_nodes()
    );
    assert!(!has_node("Holaf_Bus_Ephemere"));
    assert!(has_node("Holaf_Bus_Keep"), "the other strip must survive");

    backend.stop().expect("stop");
}

/// Upgrade path: a `Holaf_Strip_<uid>` node left by an older build (or by the
/// legacy Python engine) must be replaced, never kept next to the new bus (two
/// sinks for one strip would split the audio path).
#[test]
fn a_legacy_strip_node_is_replaced_not_orphaned() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP bus_naming: no headless PipeWire configured");
        return;
    }
    let Some(mut backend) = fresh_backend() else {
        return;
    };

    let legacy = graph::legacy_strip_node_name("old-uid");
    let spec = format!(
        "create-node adapter {{ factory.name=support.null-audio-sink node.name={legacy} \
         node.description={legacy} media.class=Audio/Sink object.linger=true \
         audio.position=[ FL FR ] }}"
    );
    run("pw-cli", &[&spec]);
    assert!(wait_until(Duration::from_secs(5), || has_node(&legacy)));

    backend
        .apply_snapshot(&[out_strip("old-uid", "Mix Out")])
        .expect("apply_snapshot");
    assert!(
        wait_until(Duration::from_secs(3), || has_node("Holaf_Bus_Mix_Out")),
        "the new bus must be created, got {:?}",
        live_nodes()
    );
    assert!(
        !has_node(&legacy),
        "the legacy '{legacy}' node must be destroyed, got {:?}",
        live_nodes()
    );

    backend.stop().expect("stop");
}
