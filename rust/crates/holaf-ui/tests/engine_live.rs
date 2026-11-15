// The harness needs `EngineBridge::spawn_pipewire`, i.e. the real backend.
#![cfg(feature = "engine-pw")]

//! Live end-to-end harness: the **Rust UI state machine** driving a **real
//! PipeWire graph**.
//!
//! Everything else in this crate is tested without an audio server (that is the
//! point of `UiAction`/`EngineCommand` being plain data).  This file is the one
//! test that proves the wiring is real, and it is therefore *skipped* unless a
//! headless sandbox daemon is configured — the same gate as
//! `holaf-pw/tests/backend_live.rs`:
//!
//! ```bash
//! eval "$(./scripts/start-pipewire-headless.sh --export)"
//! cd rust && cargo test -p holaf-ui --test engine_live -- --nocapture
//! ```
//!
//! What it checks, in order:
//!
//! 1. `AppState::from_config` loads a two-strip config (one output with EQ
//!    available) and `attach_engine` pushes the snapshot;
//! 2. the graph really exists (the bus node `Holaf_Bus_UI_Out` is published);
//! 3. a **simulated fader drag** (`UiAction::Volume`) reaches the daemon: the
//!    node's `channelVolumes` is the PulseAudio cubic gain of the requested
//!    volume (0.44 → 0.44³), not a raw 0.44;
//! 4. a **FX toggle** (`UiAction::ToggleFx`) loads the strip's filter-chain
//!    (`Holaf_FX_out-bus` appears);
//! 5. the **meters** move: a 1 kHz sine played into the bus is read by the
//!    capture children, shows a non-zero VU level, and the level decays to
//!    silence when the playback stops;
//! 6. `AppState::shutdown` leaves **zero debris**: no `Holaf_*` node, no
//!    `pw-cat --record` child, the bridge is gone.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use holaf_core::model::EffectKey;
use holaf_ui::app::{AppState, EngineState, UiAction};
use holaf_ui::bridge::EngineBridge;

// ---------------------------------------------------------------------------
// Sandbox plumbing (mirrors holaf-pw/tests/backend_live.rs)
// ---------------------------------------------------------------------------

fn live_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Is this machine allowed to talk to a live PipeWire daemon?  Strict on
/// purpose: `cargo test --workspace` must never touch a real user session.
fn live_allowed() -> bool {
    if std::env::var("HOLAF_PW_LIVE").as_deref() == Ok("1") {
        return true;
    }
    runtime_dir().contains("pw-runtime") || runtime_dir().contains("holaf")
}

fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("PIPEWIRE_RUNTIME_DIR"))
        .unwrap_or_default()
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
    dump.as_array()?.iter().find_map(|object| {
        let props = object.get("info")?.get("props")?;
        if props.get("node.name")?.as_str() == Some(name) {
            object.get("id")?.as_u64().map(|id| id as u32)
        } else {
            None
        }
    })
}

/// Every published node name, for the "does the chain exist?" / "no debris"
/// assertions.
fn node_names() -> Vec<String> {
    pw_dump()
        .and_then(|dump| dump.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|object| {
            object
                .get("info")?
                .get("props")?
                .get("node.name")?
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

/// `pw-cli enum-params <node> Props` → the applied `channelVolumes` /
/// `monitorVolumes` (the only honest read-back: the daemon's `status` echoes the
/// requested value, not the gain PipeWire ended up with).
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
                line.split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<f32>().ok()),
            ) {
                target.push(value);
            }
        }
    }
    Some((channel, monitor))
}

/// Restart the sandbox daemon so the test starts from a pristine graph (this
/// sandbox stops publishing `Port` globals after many create/destroy cycles —
/// see `scripts/start-pipewire-headless.sh`).
fn fresh_sandbox_daemon() {
    if !live_allowed() {
        return;
    }
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../scripts/start-pipewire-headless.sh"
    );
    let _ = Command::new("bash").arg(script).arg("restart").output();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if pw_dump().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn scratch(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("holaf-ui-live-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A 16-bit stereo WAV holding a 1 kHz sine at `amplitude` (full scale = 1.0).
///
/// `pw-cat --playback` reads files through libsndfile, which does not recognise
/// raw float streams — but it does recognise WAV, so the harness writes one
/// rather than depending on an external audio file.
fn write_sine_wav(path: &Path, seconds: f32, amplitude: f32) {
    const RATE: u32 = 48_000;
    let frames = (RATE as f32 * seconds) as u32;
    let mut samples = Vec::with_capacity(frames as usize * 4);
    for n in 0..frames {
        let value = (amplitude
            * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / RATE as f32).sin()
            * i16::MAX as f32) as i16;
        samples.extend_from_slice(&value.to_le_bytes());
        samples.extend_from_slice(&value.to_le_bytes());
    }
    let data_len = samples.len() as u32;
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    file.write_all(b"WAVEfmt ").unwrap();
    file.write_all(&16_u32.to_le_bytes()).unwrap(); // fmt chunk size
    file.write_all(&1_u16.to_le_bytes()).unwrap(); // PCM
    file.write_all(&2_u16.to_le_bytes()).unwrap(); // stereo
    file.write_all(&RATE.to_le_bytes()).unwrap();
    file.write_all(&(RATE * 4).to_le_bytes()).unwrap(); // byte rate
    file.write_all(&4_u16.to_le_bytes()).unwrap(); // block align
    file.write_all(&16_u16.to_le_bytes()).unwrap(); // bits per sample
    file.write_all(b"data").unwrap();
    file.write_all(&data_len.to_le_bytes()).unwrap();
    file.write_all(&samples).unwrap();
    file.sync_all().unwrap();
}

/// Poll `state` until `predicate` holds (bounded), returning whether it did.
fn wait_for(
    state: &mut AppState,
    timeout: Duration,
    mut predicate: impl FnMut(&AppState) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        state.tick(Instant::now());
        if predicate(state) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    predicate(state)
}

fn wait_for_node(name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if node_id_by_name(name).is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

const OUT_BUS: &str = "Holaf_Bus_UI_Out";

/// The config the harness loads: one input routed into one virtual output whose
/// EQ is available (the chain is toggled on later, through the UI action).
const TEST_CONFIG: &str = r#"{
    "window": {"height": 620},
    "strips": [
        {
            "uid": "in-mic",
            "label": "UI In",
            "kind": "input",
            "mode": "virtual",
            "volume": 0.8,
            "routes": ["out-bus"],
            "effects": {"eq": {"active": false, "params": {}}}
        },
        {
            "uid": "out-bus",
            "label": "UI Out",
            "kind": "output",
            "mode": "virtual",
            "volume": 0.44,
            "device_name": null,
            "effects": {"eq": {"active": false, "params": {}}}
        }
    ]
}"#;

#[test]
fn the_ui_drives_a_real_pipewire_graph() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!(
            "SKIP engine_live: no headless PipeWire configured \
             (run: eval \"$(./scripts/start-pipewire-headless.sh --export)\")"
        );
        return;
    }
    fresh_sandbox_daemon();

    // --- (a) load a realistic config --------------------------------
    let dir = scratch("graph");
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, TEST_CONFIG).unwrap();
    let mut state = AppState::from_config(&config_path);
    assert_eq!(state.strips.len(), 2, "config: {:?}", state.status);
    assert_eq!(state.strips[1].label, "UI Out");
    let out_index = state.strip_index("out-bus").unwrap();

    // --- (b) attach the real engine: the snapshot is pushed -------------
    state.attach_engine(EngineBridge::spawn_pipewire(holaf_pw::MonoDownmix::default()));
    assert!(
        wait_for(&mut state, Duration::from_secs(20), |state| state.engine
            == EngineState::Connected),
        "the engine never reported Ready ({:?})",
        state.engine
    );

    // --- (c) the graph exists -------------------------------------------
    assert!(
        wait_for_node(OUT_BUS, Duration::from_secs(20)),
        "the bus node {OUT_BUS} was never published; nodes: {:?}",
        node_names()
    );

    // --- (d) a simulated fader drag reaches the daemon -------------------
    let now = Instant::now();
    state.set_clock(now);
    // A drag is many small changes; the 25 Hz throttle keeps only some of them.
    for step in 0..8 {
        state.set_clock(now + Duration::from_millis(step * 6));
        state.apply(UiAction::Volume {
            uid: "out-bus".to_string(),
            volume: 0.30 + step as f32 * 0.02,
        });
    }
    state.flush_engine(now + Duration::from_millis(120));
    // 0.44 is the config value; the drag ends on 0.44 (0.30 + 7*0.02).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut gains = Vec::new();
    while Instant::now() < deadline {
        state.tick(Instant::now());
        if let Some((channel, _)) = volumes_of(OUT_BUS) {
            if !channel.is_empty() {
                let expected = 0.44_f32 * 0.44 * 0.44;
                if channel.iter().all(|gain| (gain - expected).abs() < 2e-3) {
                    gains = channel;
                    break;
                }
                gains = channel;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // The fader value is `0.30 + 7 * 0.02 = 0.44`, so the expected gain is the
    // PulseAudio cubic one — the same number `pactl set-sink-volume … 44 %`
    // produces.  A raw 0.44 (the historical "+14 dB" bug) is rejected.
    let expected = 0.44_f32 * 0.44 * 0.44;
    assert!(!gains.is_empty(), "no channelVolumes on {OUT_BUS}");
    for gain in &gains {
        assert!(
            (gain - expected).abs() < 2e-3,
            "channelVolumes {gain} != {expected} (0.44³): the UI volume did not reach the daemon"
        );
    }
    assert!((state.strips[out_index].volume - 0.44).abs() < 1e-6);

    // --- (e) an FX toggle loads the strip's chain ------------------------
    state.apply(UiAction::ToggleFx {
        uid: "out-bus".to_string(),
        effect: EffectKey::Eq,
        active: true,
    });
    assert!(state.strips[out_index].effects[&EffectKey::Eq].active);
    let chain_deadline = Instant::now() + Duration::from_secs(25);
    let mut chain_loaded = false;
    while Instant::now() < chain_deadline {
        state.tick(Instant::now());
        chain_loaded = node_names()
            .iter()
            .any(|name| name.contains("Holaf_FX_out-bus"));
        if chain_loaded {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        chain_loaded,
        "toggling EQ did not load the filter-chain; nodes: {:?}",
        node_names()
    );

    // --- (f) meters: a sine on the bus must move the VU -------------------
    let wav = dir.join("sine.wav");
    write_sine_wav(&wav, 4.0, 0.5);
    let mut playback = Command::new("pw-cat")
        .arg("--playback")
        .arg("--target")
        .arg(OUT_BUS)
        .arg(&wav)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("pw-cat --playback");
    let heard = wait_for(&mut state, Duration::from_secs(10), |state| {
        state.levels_for(out_index)[0] > 0.01
    });
    let level = state.levels_for(out_index);
    // Stop the tone; the envelope must bring the bar back to silence.
    let _ = playback.kill();
    let _ = playback.wait();
    let silent = wait_for(&mut state, Duration::from_secs(10), |state| {
        state.levels_for(out_index)[0] < 0.02
    });
    assert!(
        heard,
        "the meters never moved while a 1 kHz sine played into {OUT_BUS} (level {level:?})"
    );
    assert!(
        silent,
        "the VU did not decay back to silence (level {:?})",
        state.levels_for(out_index)
    );

    // --- (g) shutdown leaves zero debris ---------------------------------
    let status = state.bridge.as_ref().map(|bridge| bridge.status().clone());
    state.shutdown();
    assert!(state.bridge.is_none());
    assert_eq!(state.engine, EngineState::None);
    if let Some(status) = status {
        assert!(!status.connected(), "the bridge still reports connected");
    }
    std::thread::sleep(Duration::from_millis(800));
    let debris: Vec<String> = node_names()
        .into_iter()
        .filter(|name| name.contains("Holaf_"))
        .collect();
    assert!(debris.is_empty(), "shutdown left debris nodes: {debris:?}");
    // …and no capture child survives either.
    let leftovers = run("pgrep", &["-af", "pw-cat --record --target"]).unwrap_or_default();
    assert!(
        !leftovers.contains(OUT_BUS),
        "a capture child survived shutdown: {leftovers}"
    );

    // The config was written by the shutdown path (a temp file, never the repo
    // one) and still holds the two strips.
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(saved["strips"].as_array().unwrap().len(), 2);
}

/// "Disconnected mode", for real: no daemon reachable, the UI stays up.
///
/// Not part of the normal live run (it would fight the other test over the
/// environment), so it is gated on an explicit opt-in:
///
/// ```bash
/// HOLAF_UI_OFFLINE_TEST=1 cargo test -p holaf-ui --test engine_live \
///     the_ui_survives_without_a_daemon -- --nocapture
/// ```
#[test]
fn the_ui_survives_without_a_daemon() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if std::env::var("HOLAF_UI_OFFLINE_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP the_ui_survives_without_a_daemon: set HOLAF_UI_OFFLINE_TEST=1");
        return;
    }
    // Point PipeWire at a runtime dir that does not exist: `connect()` must fail
    // with a reason instead of hanging or panicking.
    let bogus = scratch("offline").join("no-runtime-here");
    std::env::set_var("XDG_RUNTIME_DIR", &bogus);
    std::env::set_var("PIPEWIRE_RUNTIME_DIR", &bogus);
    std::env::set_var("PIPEWIRE_REMOTE", "holaf-no-such-daemon");

    let dir = scratch("offline-config");
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, TEST_CONFIG).unwrap();
    let mut state = AppState::from_config(&config_path);
    state.attach_engine(EngineBridge::spawn_pipewire(holaf_pw::MonoDownmix::default()));

    // The connection failure arrives as an engine event; the UI is not dead.
    assert!(
        wait_for(&mut state, Duration::from_secs(30), |state| state
            .is_disconnected()),
        "the engine never reported a connection failure ({:?})",
        state.engine
    );
    let reason = state.engine_error().expect("a banner reason");
    assert!(!reason.is_empty(), "the banner needs a reason");
    eprintln!("disconnected mode reason: {reason}");

    // Model edits still work; the audio action is refused with a message.
    state.apply(UiAction::Mute {
        uid: "out-bus".to_string(),
        mute: true,
    });
    assert!(state.strips[1].mute, "the model stays editable");
    assert!(
        state
            .status
            .as_deref()
            .is_some_and(|status| status.contains("engine disconnected")),
        "the ignored action must be reported: {:?}",
        state.status
    );
    // Dialogs and config keep working offline.
    state.apply(UiAction::OpenFxSettings {
        uid: "out-bus".to_string(),
        effect: EffectKey::Eq,
    });
    assert!(state.fx_dialog.is_some());

    state.shutdown();
    assert!(state.bridge.is_none());
    assert_eq!(state.engine, EngineState::None);
}

/// The two "daily use" features that need a real daemon: the **application
/// routing** (the daemon now lists streams through `pactl`, so the answer must
/// come back capable — `stub: false`, `source: pactl` — even when nothing is
/// playing) and the **diagnostic report** (which must describe the real graph,
/// not a canned payload).
///
/// ```bash
/// eval "$(./scripts/start-pipewire-headless.sh --export)"
/// cd rust && cargo test -p holaf-ui --test engine_live -- --nocapture
/// ```
#[test]
fn the_app_routing_and_the_diagnostics_see_the_real_engine() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP the_app_routing_and_the_diagnostics_see_the_real_engine: no live daemon");
        return;
    }
    fresh_sandbox_daemon();

    let dir = scratch("apps-diag");
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, TEST_CONFIG).unwrap();
    let mut state = AppState::from_config(&config_path);
    state.attach_engine(EngineBridge::spawn_pipewire(holaf_pw::MonoDownmix::default()));
    assert!(
        wait_for(&mut state, Duration::from_secs(30), |state| {
            state.engine == EngineState::Connected
        }),
        "the engine never connected ({:?})",
        state.engine
    );
    assert!(
        wait_for(&mut state, Duration::from_secs(20), |_| {
            node_id_by_name(OUT_BUS).is_some()
        }),
        "the bus {OUT_BUS} was never published"
    );

    // --- 1. SELECT APPS -------------------------------------------------
    state.apply(UiAction::SelectApps {
        uid: "in-mic".to_string(),
    });
    assert!(
        state.apps_dialog.is_some(),
        "the dialog opens before any answer (never blocks a click)"
    );
    // The real engine's answer arrives asynchronously.  An **empty** list is a
    // perfectly good answer (nothing is playing in the sandbox), so the arrival
    // is detected by the capability the payload advertises, not by its length.
    let got_answer = wait_for(&mut state, Duration::from_secs(20), |state| {
        state.app_list.source.is_some() || state.app_list.stub
    });
    assert!(got_answer, "the engine never answered app_list");

    // Confirm an assignment for a **virtual** input strip (the only kind that
    // takes applications over).
    state.apply(UiAction::SelectAppsDone {
        uid: "in-mic".to_string(),
        names: vec!["firefox".to_string()],
    });
    assert_eq!(state.assigned_apps("in-mic"), ["firefox"]);
    assert!(
        state
            .config
            .as_ref()
            .is_some_and(|config| config.save_pending()),
        "an assignment is persisted"
    );

    // Enforcement runs on the answer: with the stub payload it must produce
    // nothing (and must not disturb the graph), with a real list it moves what
    // is off its bus.
    let moves_before = state.node_names.clone();
    state.enforce_applications();
    assert!(
        wait_for(&mut state, Duration::from_secs(10), |_| {
            node_id_by_name(OUT_BUS).is_some()
        }),
        "enforcement broke the graph"
    );
    eprintln!(
        "app_list: {} app(s), stub={}, source={:?} ({}), bus names {moves_before:?}",
        state.app_list.apps.len(),
        state.app_list.stub,
        state.app_list.source,
        state.app_list.note.as_deref().unwrap_or("-")
    );
    assert!(
        !state.app_list.stub,
        "app management is implemented now: the daemon must not answer the stub"
    );
    assert_eq!(
        state.app_list.source.as_deref(),
        Some("pactl"),
        "the daemon lists streams through pactl"
    );
    // Whatever it listed must carry a sink **name** (the field `plan` compares
    // with `status.strips[].node_name`), never a PulseAudio index.
    for app in &state.app_list.apps {
        assert!(!app.id.is_empty());
        if let Some(sink) = &app.sink {
            assert!(
                !sink.chars().all(|c| c.is_ascii_digit()),
                "{:?} publishes an index instead of a sink name",
                app.name
            );
        }
    }

    // --- 2. Debug window ------------------------------------------------
    state.apply(UiAction::OpenDiagnostics);
    assert!(state.diagnostics.is_some());
    let got_status = wait_for(&mut state, Duration::from_secs(20), |state| {
        state
            .diagnostics
            .as_ref()
            .is_some_and(|diagnostics| diagnostics.report.engine.available)
    });
    assert!(got_status, "the engine never answered status()");
    let report = &state.diagnostics.as_ref().unwrap().report;
    assert!(report.engine.running, "the daemon says it is not running");
    assert!(
        !report
            .findings
            .iter()
            .any(|finding| finding.id == "daemon_unreachable"),
        "the engine answered, it cannot be unreachable"
    );
    // --- 2b. clock / timing block ---------------------------------------
    // The daemon reads the clock settings itself (`pw-metadata -n settings`):
    // the report must carry a real quantum, and when `pw-top` has no batch mode
    // (PipeWire 0.3.65) it must say so instead of claiming zero xruns.
    let timing = &report.engine.timing;
    eprintln!("timing → {}", timing.summary());
    assert!(
        timing.known(),
        "the daemon must publish the clock settings: {:?}",
        timing.settings_error
    );
    assert!(
        timing.rate == 48_000 || timing.rate == 44_100,
        "unexpected rate {}",
        timing.rate
    );
    assert!(timing.quantum >= 16, "quantum {}", timing.quantum);
    assert!(timing.quantum_ms > 0.0);
    if !timing.pw_top_available {
        assert!(
            timing.pw_top_reason.as_deref().is_some_and(|r| !r.is_empty()),
            "no reason for the missing statistics: {timing:?}"
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.id == "pw_top_unavailable"),
            "the report must state that the statistics are missing: {:?}",
            report.findings
        );
    }

    // The report names the real bus of the strip (`Holaf_Bus_UI_In`) — that is
    // what the graph check is built on.
    assert!(
        report
            .engine
            .strips
            .iter()
            .any(|strip| strip.node_name.as_deref() == Some("Holaf_Bus_UI_In")),
        "the status payload carries no bus name: {:?}",
        report.engine.strips
    );
    eprintln!(
        "diagnostics: {} error(s), {} warning(s), {} info — {:?}",
        report.errors(),
        report.warnings(),
        report.infos(),
        report
            .findings
            .iter()
            .map(|finding| finding.id)
            .collect::<Vec<_>>()
    );
    // The pasteable text is complete even when everything is healthy.
    let text = report.text();
    assert!(text.contains("FINDINGS"));
    assert!(text.contains("ENGINE"));
    assert!(!text.contains("daemon_unreachable"));

    state.shutdown();
    // No debris: the whole graph went away with the engine.
    assert!(
        !node_id_by_name(OUT_BUS).is_some(),
        "{OUT_BUS} survived the shutdown"
    );
}

/// **The VU bar is post-fader and a normal level does not pin it.**
///
/// The user's report (“les vu-mètres sont toujours à fond, même quand le son est
/// faible”) had two candidate causes; both are measured here, through the *whole*
/// production chain — `pw-cat` capture → `MeterBoard` → decay envelope → the
/// number the fader widget draws:
///
/// * the old scale (`min(1, rms × 5)`) saturated at −14 dBFS, so a −9 dBFS tone
///   pinned the bar at 1.0 → this test asserts it stays **below** full;
/// * the capture tap must be post-fader → moving the fader to 0.3
///   (`pulse_linear_gain` = 0.027, −31 dB) must drop the bar, not leave it
///   where it was.
#[test]
fn the_vu_bar_is_post_fader_and_not_pinned_by_a_normal_level() {
    const CONFIG: &str = r#"{
        "strips": [
            {
                "uid": "vu-bar",
                "label": "VU Bar",
                "kind": "output",
                "volume": 1.0,
                "device_name": null,
                "effects": {}
            }
        ]
    }"#;
    const BUS: &str = "Holaf_Bus_VU_Bar";

    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!(
            "SKIP engine_live: no headless PipeWire configured \
             (run: eval \"$(./scripts/start-pipewire-headless.sh --export)\")"
        );
        return;
    }
    fresh_sandbox_daemon();

    let dir = scratch("vu");
    let config_path = dir.join("config.json");
    std::fs::write(&config_path, CONFIG).unwrap();
    let mut state = AppState::from_config(&config_path);
    let index = state.strip_index("vu-bar").expect("the strip is loaded");
    state.attach_engine(EngineBridge::spawn_pipewire(holaf_pw::MonoDownmix::default()));
    assert!(
        wait_for(&mut state, Duration::from_secs(20), |state| state.engine
            == EngineState::Connected),
        "the engine never reported Ready ({:?})",
        state.engine
    );
    assert!(
        wait_for_node(BUS, Duration::from_secs(20)),
        "the bus node {BUS} was never published; nodes: {:?}",
        node_names()
    );

    // A −6 dBFS sine: RMS ≈ 0.354 → −9 dBFS.  On the dBFS scale (−60 dBFS → 0,
    // +6 dBFS → 1) that is level ≈ 0.77 — measured 0.727 for a −12 dBFS RMS
    // music-like signal and 0.782 for −9 dBFS RMS, so ordinary material keeps a
    // visible margin under the top of the bar.
    let wav = dir.join("vu-sine.wav");
    write_sine_wav(&wav, 20.0, 0.5);
    let mut playback = Command::new("pw-cat")
        .args(["--playback", "--target", BUS])
        .arg(&wav)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("pw-cat --playback");
    assert!(
        wait_for(&mut state, Duration::from_secs(15), |state| state
            .levels_for(index)[0]
            > 0.3),
        "the VU never moved (level {:?})",
        state.levels_for(index)
    );
    // Let the envelope settle on the real level before reading it.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut loud = 0.0_f32;
    while Instant::now() < deadline {
        state.tick(Instant::now());
        loud = loud.max(state.levels_for(index)[0]);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        (0.6..0.85).contains(&loud),
        "a −9 dBFS tone must be a *high* bar but not a pinned one (level {loud}, \
         expected ≈ 0.77: −60 dBFS → 0, +6 dBFS → 1)"
    );

    // Fader down to 0.3 → `pulse_linear_gain(0.3)` = 0.027 → −31 dB on the
    // monitor.  A pre-fader meter would read exactly the same as before.
    let now = Instant::now();
    state.set_clock(now);
    state.apply(UiAction::Volume {
        uid: "vu-bar".to_string(),
        volume: 0.3,
    });
    state.flush_engine(now + Duration::from_millis(120));
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut quiet = 1.0_f32;
    while Instant::now() < deadline {
        state.tick(Instant::now());
        quiet = quiet.min(state.levels_for(index)[0]);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        quiet < loud - 0.3,
        "the VU is PRE-fader: {loud} at volume 1.0 vs {quiet} at 0.3"
    );
    assert!(quiet > 0.0, "the fader attenuates, it does not mute: {quiet}");

    let _ = playback.kill();
    let _ = playback.wait();
    state.shutdown();
}
