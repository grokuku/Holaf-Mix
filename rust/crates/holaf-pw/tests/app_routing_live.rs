//! Live proof that per-application routing really works against a daemon:
//! `app_list` sees a playing stream and not the daemon's own ones, and
//! `app_move` **moves** it onto a strip's bus (verified by reading it back).
//!
//! Skipped unless a headless PipeWire sandbox is configured — same gate as
//! `backend_live.rs`, so `cargo test --workspace` never touches a real session:
//!
//! ```bash
//! eval "$(./scripts/start-pipewire-headless.sh --export)"
//! cd rust && cargo test -p holaf-pw --test app_routing_live -- --nocapture
//! ```
//!
//! The audio source is a real client (`pw-cat --playback` of a generated WAV),
//! not a mocked payload: the point is precisely that a *third-party* stream
//! appears, is moveable, and that the daemon's own recorders never do.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use holaf_core::backend::Backend;
use holaf_core::model::{Strip, StripType};
use holaf_pw::PipeWireBackend;

/// The bus of the strip the stream is moved onto (`Holaf_Bus_<slug(label)>`).
const BUS: &str = "Holaf_Bus_App_Live";

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

fn fresh_sandbox_daemon() {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("PIPEWIRE_RUNTIME_DIR"))
        .unwrap_or_default();
    if !(dir.contains("pw-runtime") || dir.contains("holaf")) {
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

fn run(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A 60 s stereo WAV (`pw-cat --playback` needs a file libsndfile recognises).
///
/// Long on purpose: `pw-cat` **exits** when the file ends, and a stream that
/// disappears mid-test would be measured as "the daemon does not list it".
fn write_tone(path: &Path) -> bool {
    let rate = 48_000u32;
    let seconds = 60u32;
    let mut data = Vec::with_capacity((rate * seconds * 4) as usize);
    for i in 0..rate * seconds {
        let value =
            (0.2 * 32_767.0 * (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin())
                as i16;
        data.extend_from_slice(&value.to_le_bytes());
        data.extend_from_slice(&value.to_le_bytes());
    }
    let data_len = data.len() as u32;
    let mut file = Vec::with_capacity(44 + data.len());
    file.extend_from_slice(b"RIFF");
    file.extend_from_slice(&(36 + data_len).to_le_bytes());
    file.extend_from_slice(b"WAVEfmt ");
    file.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    file.extend_from_slice(&1u16.to_le_bytes()); // PCM
    file.extend_from_slice(&2u16.to_le_bytes()); // stereo
    file.extend_from_slice(&rate.to_le_bytes());
    file.extend_from_slice(&(rate * 4).to_le_bytes()); // byte rate
    file.extend_from_slice(&4u16.to_le_bytes()); // block align
    file.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    file.extend_from_slice(b"data");
    file.extend_from_slice(&data_len.to_le_bytes());
    file.extend_from_slice(&data);
    std::fs::write(path, file).is_ok()
}

/// A null sink to play into (`pw-cat` needs a sink, and the sandbox has no
/// hardware device).  Waits until the server really lists it.
fn ensure_null_sink() -> Option<String> {
    const SINK: &str = "Holaf_App_Live_Sink";
    if pactl_sink_exists(SINK) {
        return Some(SINK.to_string());
    }
    run(
        "pactl",
        &[
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK}"),
        ],
    )?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if pactl_sink_exists(SINK) {
            return Some(SINK.to_string());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn pactl_sink_exists(name: &str) -> bool {
    run("pactl", &["-f", "json", "list", "sinks"])
        .map(|json| {
            holaf_pw::sink_inputs::parse_sinks(&json)
                .values()
                .any(|sink| sink == name)
        })
        .unwrap_or(false)
}

/// Put stream `index` on `sink` **for the test setup** (`pactl` directly).
///
/// Which sink WirePlumber picks for a new stream depends on its default-sink
/// policy (a freshly restarted sandbox has none), so the starting point is
/// *made* explicit here instead of being assumed.  The behaviour under test is
/// [`Backend::app_move`], not this.
fn pactl_move(index: &str, sink: &str) -> bool {
    run("pactl", &["move-sink-input", index, sink]).is_some()
}

/// Start a playback stream; `properties` are `-P key=value` node properties
/// (`node.name`, `application.name`, `application.process.id`).
///
/// `detached` decides who its parent is: with `setsid` the stream is a foreign
/// application, without it a direct child of *this* process (which hosts the
/// backend) — the shape the daemon's own helpers have.
fn spawn_stream(wav: &Path, target: &str, properties: &[&str], detached: bool) -> Option<Child> {
    let mut command = if detached {
        let mut command = Command::new("setsid");
        command.arg("pw-cat");
        command
    } else {
        Command::new("pw-cat")
    };
    command.args(["--playback", "--target", target]);
    for property in properties {
        command.args(["-P", property]);
    }
    command
        .arg(wav)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

fn apps_of(listed: &serde_json::Value) -> &Vec<serde_json::Value> {
    listed["apps"].as_array().expect("apps array")
}

fn excluded_reasons(listed: &serde_json::Value) -> Vec<String> {
    listed["excluded"]
        .as_array()
        .map(|excluded| {
            excluded
                .iter()
                .map(|e| e["reason"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Wait until `predicate` holds on a fresh `app_list`, returning the last answer.
fn wait_for_app_list(
    backend: &mut PipeWireBackend,
    predicate: impl Fn(&serde_json::Value) -> bool,
    what: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = serde_json::Value::Null;
    while Instant::now() < deadline {
        last = backend.app_list().expect("app_list");
        if predicate(&last) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what}: last app_list was {last}");
}

#[test]
fn a_stream_can_be_listed_and_moved_onto_a_bus() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!(
            "SKIP app_routing_live: no headless PipeWire configured (set XDG_RUNTIME_DIR to the \
             headless runtime or HOLAF_PW_LIVE=1)"
        );
        return;
    }
    if run("pactl", &["info"]).is_none() {
        eprintln!("SKIP app_routing_live: no pactl (pipewire-pulse) on this machine");
        return;
    }
    fresh_sandbox_daemon();
    let mut backend = match PipeWireBackend::connect() {
        Ok(b) => b,
        Err(err) => {
            eprintln!("SKIP app_routing_live: cannot reach a PipeWire daemon: {err}");
            return;
        }
    };
    let _ = backend.stop();

    // A virtual input strip: its bus is the destination of the move.
    let mut strip = Strip::with_uid("app-live-in", "App Live", StripType::Input);
    strip.device_name = None;
    backend
        .apply_snapshot(&[strip])
        .expect("apply_snapshot with one virtual input");
    let status = backend.status().expect("status");
    let bus = status["strips"][0]["node_name"]
        .as_str()
        .expect("the strip publishes its bus name")
        .to_string();
    assert_eq!(bus, BUS, "the bus name follows the label");

    let Some(playback_sink) = ensure_null_sink() else {
        eprintln!("SKIP app_routing_live: cannot create a null sink to play into");
        let _ = backend.stop();
        return;
    };
    assert_ne!(playback_sink, BUS, "the stream must start elsewhere");

    let wav = std::env::temp_dir().join("holaf-app-live-tone.wav");
    assert!(write_tone(&wav), "cannot write {}", wav.display());
    let own_pid = std::process::id();

    // A helper process whose pid we can borrow: its parent is *this* process, so
    // a stream claiming that pid is "a child of the daemon".
    let mut helper = Command::new("sleep")
        .arg("60")
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let child_pid = helper.id();

    // 1) A foreign application → listed.
    let mut foreign = spawn_stream(&wav, &playback_sink, &["application.name=ForeignApp"], true)
        .expect("spawn pw-cat (foreign)");
    // 2) Our own process → excluded (`own process`).
    let mut ours = spawn_stream(
        &wav,
        &playback_sink,
        &[
            "application.name=OursByPid",
            &format!("application.process.id={own_pid}"),
        ],
        true,
    )
    .expect("spawn pw-cat (own pid)");
    // 3) A child of our process (the metering-helper shape) → excluded.
    let mut ours_child = spawn_stream(
        &wav,
        &playback_sink,
        &[
            "application.name=OursByPpid",
            &format!("application.process.id={child_pid}"),
        ],
        true,
    )
    .expect("spawn pw-cat (child pid)");
    // 4) A stream carrying one of our graph node names → excluded.
    let mut our_node = spawn_stream(
        &wav,
        &playback_sink,
        &["application.name=Chatty", "node.name=Holaf_strip_fake"],
        true,
    )
    .expect("spawn pw-cat (Holaf node name)");

    // The foreign stream must be listed **and routed** (WirePlumber needs a
    // moment to link it) before a move can mean anything.  Where it landed is
    // WirePlumber's business; if it landed on the bus already, put it back on
    // the plain sink so the move has something to do.
    let mut listed = wait_for_app_list(
        &mut backend,
        |listed| {
            apps_of(listed).iter().any(|app| {
                app["name"] == serde_json::json!("ForeignApp") && app["sink"].is_string()
            })
        },
        "the foreign stream must be listed and routed",
    );
    let foreign_id = apps_of(&listed)
        .iter()
        .find(|app| app["name"] == serde_json::json!("ForeignApp"))
        .and_then(|app| app["id"].as_str())
        .expect("the foreign stream id")
        .to_string();
    if apps_of(&listed).iter().any(|app| {
        app["name"] == serde_json::json!("ForeignApp") && app["sink"] == serde_json::json!(BUS)
    }) {
        eprintln!("note: WirePlumber put the stream on the bus immediately - moving it away first");
        assert!(
            pactl_move(&foreign_id, &playback_sink),
            "cannot stage the stream on {playback_sink}"
        );
        listed = wait_for_app_list(
            &mut backend,
            |listed| {
                apps_of(listed).iter().any(|app| {
                    app["name"] == serde_json::json!("ForeignApp")
                        && app["sink"] == serde_json::json!(playback_sink)
                })
            },
            "the stream must be staged on the playback sink",
        );
    }
    eprintln!(
        "app_list →\n{}",
        serde_json::to_string_pretty(&listed).unwrap()
    );

    let names: Vec<String> = apps_of(&listed)
        .iter()
        .map(|app| app["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        names.contains(&"ForeignApp".to_string()),
        "the foreign stream must be listed: {listed}"
    );
    for excluded in ["OursByPid", "OursByPpid", "Chatty"] {
        assert!(
            !names.contains(&excluded.to_string()),
            "{excluded} must not be listed: {listed}"
        );
    }
    let reasons = excluded_reasons(&listed);
    for expected in [
        "own process",
        "child process (metering recorder)",
        "Holaf node",
    ] {
        assert!(
            reasons.iter().any(|reason| reason == expected),
            "expected exclusion {expected:?} in {reasons:?}"
        );
    }
    assert_eq!(
        listed["stub"],
        serde_json::json!(false),
        "app management must no longer advertise itself as a stub"
    );

    let moveable = apps_of(&listed)
        .iter()
        .find(|app| app["name"] == serde_json::json!("ForeignApp"))
        .expect("the foreign stream")
        .clone();
    let app_id = moveable["id"]
        .as_str()
        .expect("the id is a string")
        .to_string();
    assert_eq!(
        moveable["sink"],
        serde_json::json!(playback_sink),
        "the stream starts on the playback sink"
    );

    // 3) Move it, then verify by reading the list back.
    let moved = backend.app_move(&app_id, BUS).expect("app_move");
    eprintln!("app_move → {moved}");
    assert_eq!(
        moved["ok"],
        serde_json::json!(true),
        "move refused: {moved}"
    );
    assert_eq!(moved["now_on"], serde_json::json!(BUS));

    let after = wait_for_app_list(
        &mut backend,
        |listed| {
            apps_of(listed).iter().any(|app| {
                app["id"] == serde_json::json!(app_id) && app["sink"] == serde_json::json!(BUS)
            })
        },
        "the moved stream must be listed on the bus",
    );
    eprintln!("read-back → {after}");
    // …and the sink really is the strip's bus node (`pw-dump` agrees).
    assert!(
        external_link_exists(BUS),
        "the bus node must exist in the live graph"
    );

    // 4) Moving it again is idempotent (the 5 s enforcement depends on it).
    let again = backend.app_move(&app_id, BUS).expect("app_move twice");
    assert_eq!(again["ok"], serde_json::json!(true), "{again}");

    // Cleanup: kill the streams and the strip, then check nothing of ours is
    // left in the graph (the live tests share one daemon).
    for child in [
        &mut foreign,
        &mut ours,
        &mut ours_child,
        &mut our_node,
        &mut helper,
    ] {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_file(&wav);
    let _ = backend.stop();
    let status = backend.status().expect("status after stop");
    assert_eq!(
        status["holaf_nodes"].as_array().map(Vec::len),
        Some(0),
        "no Holaf node may survive the stop: {status}"
    );
}

/// Does `node.name` exist in the live graph (`pw-dump`)?
fn external_link_exists(name: &str) -> bool {
    let Some(dump) = run("pw-dump", &[]) else {
        return false;
    };
    let Ok(dump) = serde_json::from_str::<serde_json::Value>(&dump) else {
        return false;
    };
    dump.as_array()
        .map(|objects| {
            objects.iter().any(|o| {
                o.get("info")
                    .and_then(|info| info.get("props"))
                    .and_then(|props| props.get("node.name"))
                    .and_then(serde_json::Value::as_str)
                    == Some(name)
            })
        })
        .unwrap_or(false)
}

/// `app_list` on a server that has nothing playing answers an empty list with
/// `stub: false` — the UI must show "no application is playing", not "this
/// engine cannot move applications".
#[test]
fn an_idle_server_answers_an_empty_but_capable_list() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() || run("pactl", &["info"]).is_none() {
        eprintln!("SKIP app_routing_live: no headless PipeWire/pactl configured");
        return;
    }
    fresh_sandbox_daemon();
    let mut backend = match PipeWireBackend::connect() {
        Ok(b) => b,
        Err(err) => {
            eprintln!("SKIP app_routing_live: cannot reach a PipeWire daemon: {err}");
            return;
        }
    };
    let listed = backend.app_list().expect("app_list");
    assert_eq!(listed["stub"], serde_json::json!(false));
    assert_eq!(listed["source"], serde_json::json!("pactl"));
    let apps = listed["apps"].as_array().expect("apps array");
    assert!(
        apps.is_empty(),
        "the sandbox must be idle at the start of a run: {listed}"
    );

    // A move with a bogus index is refused, and says why.
    let moved = backend.app_move("999999", BUS).expect("app_move");
    assert_eq!(moved["ok"], serde_json::json!(false), "{moved}");
    assert!(
        moved["note"]
            .as_str()
            .unwrap_or_default()
            .contains("999999"),
        "the refusal must name the stream: {moved}"
    );
}
