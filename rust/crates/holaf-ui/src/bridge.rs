//! `EngineBridge` — the UI thread ↔ engine thread seam.
//!
//! # Why a bridge at all
//!
//! The PySide build runs engine calls on a `QThreadPool` (`BackendWorker`) and
//! reports back with a queued Qt signal; the volume/MUTE/MONO setters are
//! fire-and-forget through `self.audio_engine`.  The Rust port keeps the same
//! shape but with plain channels, because `holaf_pw::PipeWireBackend` is
//! already a `Send` handle that marshals every operation to its own worker
//! thread: a **command channel plus an event channel is all the UI needs**, no
//! thread pool.
//!
//! ```text
//!   UI thread (egui, 60 fps)              engine thread
//!   ─────────────────────────             ─────────────
//!   AppState::apply(UiAction)
//!        └─ EngineCommand ──mpsc──────►  recv() → Backend::{set_volume,…}
//!                                         │
//!                                    meter capture (pw-cat children, §4)
//!                                         │
//!   AppState::poll_engine()               ▼
//!        ◄──EngineEvent ───mpsc──────── Applied | Failed | Levels | Devices
//!        ◄──MeterBoard::read(uid)────── Arc<[AtomicU32]> (lock-free levels)
//! ```
//!
//! Design rules kept from the Python app:
//!
//! * **The UI never blocks.** Sends are non-blocking; a dead engine shows up as
//!   [`BridgeError::Disconnected`] (and then as the banner's "disconnected
//!   mode"), never as a freeze.
//! * **A refused command is reported, not swallowed.** `EngineEvent::Failed`
//!   carries the command itself, which is what lets `AppState` roll the
//!   optimistic UI change back to the value the widget had before the click.
//! * **Metering stays outside the daemon** (spec §6): the daemon only *names*
//!   the source to record (`Backend::meter_sources`), and this thread records it
//!   with `pw-cat` (see [`crate::capture`]), publishing into a [`MeterBoard`].
//! * **Clean shutdown.** `Shutdown` stops the capture children, calls
//!   `Backend::stop()` (graph teardown + FX host reaping) and joins the worker;
//!   `Drop` does the same for anyone who forgets ([`AppState::shutdown`] does
//!   not forget — it mirrors `make_shutdown_all`).
//!
//! # Feature `engine-pw`
//!
//! `default = ["engine-pw"]`, so the app drives the real backend out of the box.
//! `--no-default-features` builds the same UI without `holaf-pw` (no libpipewire
//! needed) — the mock engine is then the only one, which is what the headless
//! snapshot uses.
//!
//! # Disconnected mode
//!
//! If `PipeWireBackend::connect()` fails (no daemon, no permissions, no
//! `libpipewire`), the engine thread **stays alive** and answers every command
//! with `Failed(command, reason)`.  The UI shows a red banner with the reason,
//! keeps the strip editing (the model, the config and the dialogs all work), and
//! logs+ignores the audio actions instead of crashing or freezing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use indexmap::IndexMap;
use serde_json::Value;

use holaf_core::model::{EffectKey, Strip};

use crate::apps::AppList;
use crate::diagnostics::CaptureRegistry;
use crate::engine_json::DeviceInfo;
use crate::meters::MeterBoard;

/// One engine operation, i.e. one `holaf_core::backend::Backend` method.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineCommand {
    /// `Backend::set_volume` — 0.0–1.0 nominal, may exceed 1.0.
    SetVolume { uid: String, volume: f32 },
    /// `Backend::set_mute`.
    SetMute { uid: String, mute: bool },
    /// `Backend::set_mono` (stereo → mono downmix).
    SetMono { uid: String, mono: bool },
    /// `Backend::fx_set_active` (hot-reload).
    SetEffect {
        uid: String,
        effect: EffectKey,
        active: bool,
    },
    /// `Backend::fx_set_params` — the full control-port map of one effect
    /// (hot-reload, debounced by the UI).
    SetFxParams {
        uid: String,
        effect: EffectKey,
        params: IndexMap<String, f32>,
    },
    /// `Backend::routing_set`.
    SetRoute {
        source: String,
        target: String,
        active: bool,
    },
    /// `Backend::set_default_sink`.
    SetDefaultSink { uid: String },
    /// Device (re)binding of one strip: `None` makes the output a virtual bus.
    /// The UI's combo goes through a debounced [`EngineCommand::SyncSnapshot`]
    /// instead (like Qt's `_schedule_engine_restart`); this variant is the
    /// surgical form a harness/test can use.
    SetDevice { uid: String, device: Option<String> },
    /// A strip disappeared: the following [`EngineCommand::SyncSnapshot`]
    /// reflects it (kept so a caller can be explicit).
    RemoveStrip { uid: String },
    /// `Backend::apply_snapshot` — sent after a structural change (add, delete,
    /// rename, device change) because the graph plan depends on the whole list.
    SyncSnapshot(Vec<Strip>),
    /// `Backend::device_list` — a *question*, answered by
    /// [`EngineEvent::Devices`].
    DeviceList,
    /// `Backend::app_list` — a *question*, answered by [`EngineEvent::Apps`].
    /// The UI asks every [`crate::apps::ENFORCE_INTERVAL`] so the assigned
    /// applications can be put back on their strip.
    AppList,
    /// `Backend::app_move` — put application `app_id` on sink `sink`.
    AppMove { app_id: String, sink: String },
    /// `Backend::status` — a *question*, answered by [`EngineEvent::Status`]
    /// (the diagnostics report is built from it).
    Status,
    /// Full restart without quitting: stop the graph, reconnect, re-apply the
    /// last snapshot.  Runs on the engine thread (it blocks for as long as
    /// PipeWire needs) and reports through [`EngineEvent::Restarted`].
    RestartEngine,
    /// Tear the engine down (graph + FX host + capture children).
    Shutdown,
}

/// What the engine thread reports back.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// The backend connected (or, for the mock, the thread started).
    Ready,
    /// Meter values per strip uid, in `0.0–1.0`.  Emitted at [`METER_INTERVAL`]
    /// by the PipeWire engine; the mock emits one per command (deterministic
    /// tests).
    Levels(HashMap<String, [f32; 2]>),
    /// The answer to [`EngineCommand::DeviceList`].
    Devices(Vec<DeviceInfo>),
    /// The answer to [`EngineCommand::AppList`], stub flag included so the
    /// dialog can explain why nothing can be moved.
    Apps(AppList),
    /// The answer to [`EngineCommand::Status`] (the raw backend payload: the
    /// diagnostics module is the only consumer and parses it, not the bridge).
    Status(Value),
    /// A [`EngineCommand::RestartEngine`] finished: `Ok(message)` or the reason.
    Restarted(Result<String, String>),
    /// An operation succeeded; the payload names it (for logging/tests).
    Applied(String),
    /// An operation failed.  Shown in the status notice, never fatal.  This is the
    /// connection-level form (no command to roll back).
    Error(String),
    /// One command was refused.  The UI rolls its optimistic change back and
    /// shows the reason.
    Failed {
        command: EngineCommand,
        reason: String,
    },
}

/// Why a command could not be handed to the engine thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// The engine thread is gone (panic, or a `shutdown` already happened).
    Disconnected,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Disconnected => f.write_str("engine thread is not running"),
        }
    }
}

impl std::error::Error for BridgeError {}

/// Connection facts the UI can read without draining events (the live harness
/// and the tests use it; [`crate::app::AppState`] derives its own state from the
/// event stream).
#[derive(Debug, Default)]
pub struct EngineStatus {
    connected: AtomicBool,
    error: Mutex<Option<String>>,
}

impl EngineStatus {
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|guard| guard.clone())
    }

    fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
        if connected {
            if let Ok(mut guard) = self.error.lock() {
                *guard = None;
            }
        }
    }

    #[cfg_attr(not(feature = "engine-pw"), allow(dead_code))]
    fn set_error(&self, reason: impl Into<String>) {
        self.connected.store(false, Ordering::Relaxed);
        if let Ok(mut guard) = self.error.lock() {
            *guard = Some(reason.into());
        }
    }
}

/// Handle to the engine thread.  Dropping it shuts the thread down.
pub struct EngineBridge {
    commands: Sender<EngineCommand>,
    events: Receiver<EngineEvent>,
    handle: Option<JoinHandle<()>>,
    levels: Arc<Mutex<HashMap<String, [f32; 2]>>>,
    meters: Option<MeterBoard>,
    /// The capture children's own status, shared with the diagnostic report
    /// (`None` for the mock engine: it has no `pw-cat` child at all).
    captures: Option<CaptureRegistry>,
    status: Arc<EngineStatus>,
}

impl std::fmt::Debug for EngineBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineBridge")
            .field("running", &self.handle.is_some())
            .field("meters", &self.meters.is_some())
            .field("connected", &self.status.connected())
            .finish()
    }
}

impl EngineBridge {
    /// Start a **mock** engine thread.
    ///
    /// It applies commands to an in-memory level map and echoes an `Applied`
    /// event, with no timing dependency, so tests are deterministic.  It is
    /// also what the headless snapshot uses: the layout must be reviewable
    /// without an audio server.  It has **no** [`MeterBoard`] (the UI then uses
    /// the `Levels` events), which is asserted by a test.
    pub fn spawn_mock() -> Self {
        Self::spawn_with(
            |command: &EngineCommand, levels: &mut HashMap<String, [f32; 2]>| {
                match command {
                    EngineCommand::SetVolume { uid, volume } => {
                        let v = volume.clamp(0.0, 1.0);
                        levels.insert(uid.clone(), [v * 0.6, v * 0.5]);
                        Ok(format!("set_volume {uid}={volume:.2}"))
                    }
                    EngineCommand::SetMute { uid, mute } => {
                        if *mute {
                            levels.insert(uid.clone(), [0.0, 0.0]);
                        }
                        Ok(format!("set_mute {uid}={mute}"))
                    }
                    EngineCommand::SetMono { uid, mono } => Ok(format!("set_mono {uid}={mono}")),
                    EngineCommand::SetEffect {
                        uid,
                        effect,
                        active,
                    } => Ok(format!("set_effect {uid}.{effect}={active}")),
                    EngineCommand::SetFxParams {
                        uid,
                        effect,
                        params,
                    } => Ok(format!(
                        "set_fx_params {uid}.{effect}={} ports",
                        params.len()
                    )),
                    EngineCommand::SetRoute {
                        source,
                        target,
                        active,
                    } => Ok(format!("set_route {source}->{target}={active}")),
                    EngineCommand::SetDefaultSink { uid } => Ok(format!("set_default_sink {uid}")),
                    EngineCommand::SetDevice { uid, device } => {
                        Ok(format!("set_device {uid}={device:?}"))
                    }
                    EngineCommand::RemoveStrip { uid } => {
                        levels.remove(uid);
                        Ok(format!("remove_strip {uid}"))
                    }
                    EngineCommand::SyncSnapshot(strips) => {
                        Ok(format!("snapshot {} strips", strips.len()))
                    }
                    EngineCommand::DeviceList => {
                        // Deterministic fake devices: enough for the picker to have
                        // something to show in a test headless.
                        levels.clear();
                        Ok("device_list".to_string())
                    }
                    EngineCommand::AppList => Ok("app_list".to_string()),
                    EngineCommand::AppMove { app_id, sink } => {
                        Ok(format!("app_move {app_id}->{sink}"))
                    }
                    EngineCommand::Status => Ok("status".to_string()),
                    EngineCommand::RestartEngine => Ok("restart".to_string()),
                    EngineCommand::Shutdown => Ok("shutdown".to_string()),
                }
            },
        )
    }

    /// Start an engine thread driven by `apply(command, levels) -> Result<name>`.
    ///
    /// The closure runs **on the engine thread** and owns the backend, which is
    /// how the PipeWire implementation keeps its objects where they were created
    /// (`holaf_pw` requires the mainloop to live in the owning thread).  An
    /// `Err` becomes [`EngineEvent::Failed`], so the UI can roll back.
    pub fn spawn_with(
        apply: impl Fn(&EngineCommand, &mut HashMap<String, [f32; 2]>) -> Result<String, String>
            + Send
            + 'static,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel::<EngineCommand>();
        let (event_tx, event_rx) = mpsc::channel::<EngineEvent>();
        let levels = Arc::new(Mutex::new(HashMap::new()));
        let thread_levels = Arc::clone(&levels);
        let status = Arc::new(EngineStatus::default());
        let thread_status = Arc::clone(&status);

        let handle = thread::Builder::new()
            .name("holaf-ui-engine".to_string())
            .spawn(move || {
                thread_status.set_connected(true);
                let _ = event_tx.send(EngineEvent::Ready);
                let mut local_levels: HashMap<String, [f32; 2]> = HashMap::new();
                loop {
                    let command = match command_rx.recv() {
                        Ok(command) => command,
                        Err(_) => break, // the UI side dropped the bridge
                    };
                    let stopping = matches!(&command, EngineCommand::Shutdown);
                    let touched_levels = matches!(
                        &command,
                        EngineCommand::SetVolume { .. }
                            | EngineCommand::SetMute { .. }
                            | EngineCommand::RemoveStrip { .. }
                    );
                    // The "questions" answers are the mock's own: a custom
                    // closure only ever implements *operations*.
                    let answer = match &command {
                        EngineCommand::DeviceList => Some(MockAnswer::Devices),
                        EngineCommand::AppList => Some(MockAnswer::Apps),
                        EngineCommand::Status => Some(MockAnswer::Status),
                        EngineCommand::RestartEngine => Some(MockAnswer::Restarted),
                        _ => None,
                    };
                    match apply(&command, &mut local_levels) {
                        Ok(what) => {
                            if let Some(answer) = answer {
                                answer.emit(&event_tx);
                            }
                            let _ = event_tx.send(EngineEvent::Applied(what));
                        }
                        Err(reason) => {
                            let _ = event_tx.send(EngineEvent::Failed { command, reason });
                        }
                    }
                    // Publish the level map only when it can have changed, and
                    // never while shutting down (the receiver is going away).
                    if touched_levels && !stopping {
                        if let Ok(mut shared) = thread_levels.lock() {
                            *shared = local_levels.clone();
                        }
                        let _ = event_tx.send(EngineEvent::Levels(local_levels.clone()));
                    }
                    if stopping {
                        break;
                    }
                }
                thread_status.set_connected(false);
            })
            .expect("the engine thread must start");

        Self {
            commands: command_tx,
            events: event_rx,
            handle: Some(handle),
            levels,
            meters: None,
            captures: None,
            status,
        }
    }

    /// Queue a command.  Never blocks; fails only when the thread is gone.
    pub fn send(&self, command: EngineCommand) -> Result<(), BridgeError> {
        self.commands
            .send(command)
            .map_err(|_| BridgeError::Disconnected)
    }

    /// Take every event emitted since the last call (non-blocking).
    pub fn drain_events(&self) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }

    /// Last published level map (a snapshot; the thread owns the truth).
    pub fn levels(&self) -> HashMap<String, [f32; 2]> {
        self.levels
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// The lock-free level board, when this bridge is backed by a real engine
    /// (`None` for the mock: its levels arrive as `Levels` events).
    pub fn meter_board(&self) -> Option<&MeterBoard> {
        self.meters.as_ref()
    }

    /// The capture children's status registry, when this bridge runs real
    /// captures (`None` for the mock).  The diagnostics report reads it (see
    /// [`crate::diagnostics::CaptureSummary`]): it is the only place a user can
    /// see *why* a VU bar stays at 0.000.
    pub fn capture_registry(&self) -> Option<&CaptureRegistry> {
        self.captures.as_ref()
    }

    pub fn status(&self) -> &Arc<EngineStatus> {
        &self.status
    }

    pub fn is_connected(&self) -> bool {
        self.status.connected()
    }

    /// Stop the engine thread and wait for it (bounded by the engine's own
    /// shutdown path — a blocked daemon call is the only way this can hang).
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.commands.send(EngineCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.status.set_connected(false);
    }
}

impl Drop for EngineBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How the mock engine answers a "question" command.
#[derive(Debug, Clone, Copy)]
enum MockAnswer {
    Devices,
    Apps,
    Status,
    Restarted,
}

impl MockAnswer {
    fn emit(self, event_tx: &Sender<EngineEvent>) {
        let event = match self {
            MockAnswer::Devices => EngineEvent::Devices(mock_devices()),
            MockAnswer::Apps => EngineEvent::Apps(mock_apps()),
            MockAnswer::Status => EngineEvent::Status(mock_status()),
            MockAnswer::Restarted => {
                EngineEvent::Restarted(Ok("engine restarted (mock)".to_string()))
            }
        };
        let _ = event_tx.send(event);
    }
}

/// The application list the mock engine answers with (deterministic).
fn mock_apps() -> AppList {
    AppList {
        apps: vec![
            crate::apps::AppInfo {
                id: "42".to_string(),
                name: "firefox".to_string(),
                pid: Some(4242),
                sink: None,
            },
            crate::apps::AppInfo {
                id: "43".to_string(),
                name: "discord".to_string(),
                pid: Some(4343),
                sink: Some("alsa_output.other.analog-stereo".to_string()),
            },
        ],
        stub: false,
        // The mock pretends to be the real daemon, which lists through `pactl`.
        source: Some("pactl".to_string()),
        note: None,
    }
}

/// A **healthy** `status()` payload, so the diagnostics window has something
/// real to render in a test and in a snapshot.
fn mock_status() -> Value {
    serde_json::json!({
        "running": true,
        "default_sink": "Holaf_Bus_Default",
        "degraded": false,
        "pending": [],
        "mainloop_pumps": 128,
        // The clock block the real daemon publishes (`pw-metadata -n settings`).
        // 1024 samples at 48 kHz is the PipeWire default, i.e. a healthy graph —
        // and `pw-top` has no batch mode on PipeWire 0.3.65, which is what the
        // `pw_top_unavailable` information finding says.
        "timing": {
            "source": "pw-metadata settings",
            "rate": 48000, "quantum": 1024, "force_quantum": 0,
            "effective_rate": 48000, "effective_quantum": 1024,
            "quantum_ms": 21.3333, "min_quantum": 16, "max_quantum": 2048,
            "settings_error": null,
            "pw_top": {
                "available": false,
                "reason": "pw-top has no batch mode (needs PipeWire >= 0.3.66)",
                "errors": 0, "rows": [],
            },
        },
        "holaf_nodes": ["Holaf_Bus_Mic", "Holaf_Bus_Casque", "Holaf_Bus_Mix_Out"],
        "meter_sources": {"in-mic": "Holaf_Bus_Mic.monitor"},
        "fx_host": {"mode": "external", "pid": 4242, "respawns": 0, "chains": ["in-mic"]},
        "strips": [
            {"uid": "in-mic", "label": "Mic", "node_name": "Holaf_Bus_Mic",
             "device_name": null, "volume": 0.85,
             "effects": {"gate": {"active": true}, "noise_cancel": {"active": true},
                          "eq": {"active": true}, "compressor": {"active": true}}},
            {"uid": "out-casque", "label": "Casque", "node_name": "Holaf_Bus_Casque",
             "device_name": "alsa_output.pci-0000_00_1f.3.analog-stereo", "volume": 0.75,
             "effects": {"eq": {"active": true}}},
            {"uid": "out-mix", "label": "Mix Out", "node_name": "Holaf_Bus_Mix_Out",
             "device_name": null, "volume": 1.0,
             "effects": {"compressor": {"active": true}}}
        ]
    })
}

/// The device list the mock engine answers with (deterministic).
///
/// The names are the ones the user's machine reports (a built-in HDA card plus
/// a PreSonus Studio 26c over USB), because the device picker's *labels* are what
/// a snapshot has to show: two analog cards both read `analog-stereo` under the
/// old rule, which is the bug this list lets a reviewer see.  One entry has no
/// `description` (the engine publishes one only when the node carries
/// `node.description`/`node.nick`), so both label paths are exercised.
fn mock_devices() -> Vec<DeviceInfo> {
    vec![
        DeviceInfo {
            id: 1,
            name: "alsa_output.pci-0000_00_1f.3.analog-stereo".to_string(),
            media_class: "Audio/Sink".to_string(),
            description: Some("Built-in Audio Analog Stereo".to_string()),
        },
        DeviceInfo {
            id: 2,
            name: "alsa_output.usb-PreSonus_Studio_26c_01424548-00.pro-output-0".to_string(),
            media_class: "Audio/Sink".to_string(),
            description: None,
        },
        DeviceInfo {
            id: 3,
            name: "alsa_input.pci-0000_00_1f.3.analog-stereo".to_string(),
            media_class: "Audio/Source".to_string(),
            description: Some("Built-in Audio Analog Stereo".to_string()),
        },
        DeviceInfo {
            id: 4,
            name: "alsa_input.usb-PreSonus_Studio_26c_01424548-00.analog-stereo".to_string(),
            media_class: "Audio/Source".to_string(),
            description: None,
        },
    ]
}

// ---------------------------------------------------------------------------
// Optional real PipeWire engine (feature `engine-pw`)
// ---------------------------------------------------------------------------

#[cfg(feature = "engine-pw")]
mod pw {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Duration, Instant};

    use crate::engine_json;
    use crate::meters::METER_INTERVAL;
    use holaf_core::backend::Backend;

    /// How often the engine thread re-asks for the meter sources, so a device
    /// that appears late (or a strip whose FX chain just became live) starts
    /// metering without waiting for the next snapshot.
    const METER_SOURCE_REFRESH: Duration = Duration::from_secs(2);

    impl EngineBridge {
        /// Drive a real [`holaf_pw::PipeWireBackend`].
        ///
        /// The backend is created **inside the engine thread**: `PipeWireBackend`
        /// owns a `pipewire` mainloop that must live in the thread that will
        /// iterate it (see `holaf-pw`'s threading notes), so constructing it on
        /// the UI thread and moving it would be wrong even though the type is
        /// `Send`.
        ///
        /// The thread also owns the meter capture
        /// ([`crate::capture::MeterCapture`]): one `pw-cat --record` child per
        /// source the engine names, publishing into the [`MeterBoard`] the UI
        /// reads and its own status into the [`CaptureRegistry`] the diagnostic
        /// report reads.
        /// `downmix` is the mono-target policy from `config.json`
        /// (`ui_rust.mono_downmix`, see [`holaf_pw::MonoDownmix`]).  It is sent to
        /// the engine on every (re)connect, so a restart keeps the user's choice;
        /// the default is the Python behaviour (`Left`).
        pub fn spawn_pipewire(downmix: holaf_pw::MonoDownmix) -> Self {
            let (command_tx, command_rx) = mpsc::channel::<EngineCommand>();
            let (event_tx, event_rx) = mpsc::channel::<EngineEvent>();
            let levels = Arc::new(Mutex::new(HashMap::new()));
            let thread_levels = Arc::clone(&levels);
            let board = MeterBoard::new();
            let thread_board = board.clone();
            // The capture status registry crosses the seam on purpose: the
            // engine thread writes it (one entry per `pw-cat` child), the UI
            // thread reads it when the diagnostic report is built.
            let captures = CaptureRegistry::new();
            let thread_captures = captures.clone();
            let status = Arc::new(EngineStatus::default());
            let thread_status = Arc::clone(&status);

            let handle = thread::Builder::new()
                .name("holaf-ui-engine-pw".to_string())
                .spawn(move || {
                    let mut capture = crate::capture::MeterCapture::with_recorder_and_registry(
                        crate::capture::default_recorder(),
                        thread_captures.clone(),
                    );
                    let mut snapshot: Vec<Strip> = Vec::new();
                    let mut uids: Vec<String> = Vec::new();
                    let mut next_source_refresh = Instant::now();
                    // The backend lives in this thread for its whole life: a
                    // `PipeWireBackend` owns the mainloop that iterates here (see
                    // `holaf-pw`'s threading notes).  `None` = disconnected mode
                    // (no daemon): stay alive, refuse operations, until
                    // `RestartEngine` or `Shutdown`.
                    let mut backend = connect_backend(&event_tx, &thread_status, &snapshot);
                    apply_downmix(&mut backend, downmix);
                    if backend.is_some() {
                        refresh_meters(
                            &mut backend,
                            &mut capture,
                            &thread_board,
                            &mut uids,
                        );
                    }

                    loop {
                        let mut structural = false;
                        match command_rx.recv_timeout(METER_INTERVAL) {
                            Ok(EngineCommand::Shutdown) => break,
                            Ok(EngineCommand::RestartEngine) => {
                                // Teardown first (no pw-cat child may outlive the
                                // graph), then a fresh connection and the last
                                // snapshot, so the mixer comes back as the user
                                // left it.
                                capture.stop_all();
                                uids.clear();
                                if let Some(mut previous) = backend.take() {
                                    if let Err(err) = previous.stop() {
                                        log::warn!("engine restart: stop failed: {err}");
                                    }
                                }
                                thread_status.set_connected(false);
                                backend = connect_backend(&event_tx, &thread_status, &snapshot);
                                apply_downmix(&mut backend, downmix);
                                let outcome = match &backend {
                                    Some(_) => Ok(format!(
                                        "engine restarted ({} strips)",
                                        snapshot.len()
                                    )),
                                    None => Err(
                                        "cannot reconnect to PipeWire - the UI stays in                                          disconnected mode"
                                            .to_string(),
                                    ),
                                };
                                let _ = event_tx.send(EngineEvent::Restarted(outcome));
                                structural = true;
                            }
                            Ok(command) => {
                                if let EngineCommand::SyncSnapshot(strips) = &command {
                                    snapshot = strips.clone();
                                }
                                structural = matches!(
                                    &command,
                                    EngineCommand::SyncSnapshot(_)
                                        | EngineCommand::SetDevice { .. }
                                        | EngineCommand::RemoveStrip { .. }
                                );
                                let question = match &command {
                                    EngineCommand::DeviceList => {
                                        backend.as_mut().map(|backend| (command_question(&command), backend.device_list()))
                                    }
                                    EngineCommand::AppList => {
                                        backend.as_mut().map(|backend| (command_question(&command), backend.app_list()))
                                    }
                                    EngineCommand::Status => {
                                        backend.as_mut().map(|backend| (command_question(&command), backend.status()))
                                    }
                                    _ => None,
                                };
                                match dispatch(backend.as_mut(), &command, &snapshot) {
                                    Ok(what) => {
                                        let _ = event_tx.send(EngineEvent::Applied(what));
                                    }
                                    Err(reason) => {
                                        let _ = event_tx.send(EngineEvent::Failed {
                                            command: command.clone(),
                                            reason,
                                        });
                                    }
                                }
                                if let Some((question, answer)) = question {
                                    question.reply(answer, &event_tx);
                                }
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                if Instant::now() >= next_source_refresh {
                                    if backend.is_some() {
                                        refresh_meters(
                                            &mut backend,
                                            &mut capture,
                                            &thread_board,
                                            &mut uids,
                                        );
                                    }
                                    next_source_refresh = Instant::now() + METER_SOURCE_REFRESH;
                                }
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                        if structural && backend.is_some() {
                            // The plan (and therefore the meter sources) just
                            // changed.
                            refresh_meters(&mut backend, &mut capture, &thread_board, &mut uids);
                            next_source_refresh = Instant::now() + METER_SOURCE_REFRESH;
                        }

                        // Publish the current levels (25 Hz) from the board.
                        let mut published = HashMap::with_capacity(uids.len());
                        for uid in &uids {
                            if let Some(level) = thread_board.read(uid) {
                                published.insert(uid.clone(), level);
                            }
                        }
                        if let Ok(mut shared) = thread_levels.lock() {
                            *shared = published.clone();
                        }
                        let _ = event_tx.send(EngineEvent::Levels(published));
                    }

                    // --- clean shutdown -------------------------------
                    // Order matters: stop recording first (no pw-cat child may
                    // outlive the graph), then tear the graph down.
                    capture.stop_all();
                    if let Some(mut backend) = backend.take() {
                        if let Err(err) = backend.stop() {
                            log::warn!("engine shutdown: {err}");
                        }
                    }
                    thread_status.set_connected(false);
                    log::info!("engine: stopped");
                })
                .expect("the engine thread must start");

            Self {
                commands: command_tx,
                events: event_rx,
                handle: Some(handle),
                levels,
                meters: Some(board),
                captures: Some(captures),
                status,
            }
        }
    }

    /// Tell a freshly connected backend which mono downmix the user chose.
    ///
    /// Best effort: an engine that refuses the policy keeps its default (the
    /// Python behaviour), which is the safe direction — the alternative would be
    /// a mixer that stops working because of a cosmetic setting.
    fn apply_downmix(
        backend: &mut Option<holaf_pw::PipeWireBackend>,
        downmix: holaf_pw::MonoDownmix,
    ) {
        let Some(backend) = backend.as_mut() else {
            return;
        };
        match backend.set_mono_downmix(downmix) {
            Ok(_) => log::info!("engine: mono downmix '{}'", downmix.as_str()),
            Err(err) => log::warn!(
                "engine: cannot set mono downmix '{}': {err} (keeping the default)",
                downmix.as_str()
            ),
        }
    }

    /// Connect a fresh backend, reporting the outcome on the event channel.
    ///
    /// `snapshot` is re-applied so a restart brings the mixer back (an empty
    /// snapshot is a no-op: a first run with no config must not tear down a
    /// graph the daemon serves for another UI).
    fn connect_backend(
        event_tx: &Sender<EngineEvent>,
        status: &EngineStatus,
        snapshot: &[Strip],
    ) -> Option<holaf_pw::PipeWireBackend> {
        match holaf_pw::PipeWireBackend::connect() {
            Ok(mut backend) => {
                status.set_connected(true);
                let _ = event_tx.send(EngineEvent::Ready);
                log::info!("engine: PipeWire backend connected");
                if !snapshot.is_empty() {
                    if let Err(err) = backend.apply_snapshot(snapshot) {
                        log::warn!("engine: re-applying the snapshot failed: {err}");
                        let _ = event_tx.send(EngineEvent::Error(format!(
                            "apply_snapshot after connect: {err}"
                        )));
                    }
                }
                Some(backend)
            }
            Err(err) => {
                // Disconnected mode: stay alive, refuse every operation with
                // the reason, exit on Shutdown.  The UI keeps working (config,
                // dialogs, layout) and shows the banner; "Restart audio engine"
                // is the way out without quitting.
                let reason = format!("cannot connect to PipeWire: {err}");
                log::warn!("engine: {reason}");
                status.set_error(reason.clone());
                let _ = event_tx.send(EngineEvent::Error(reason));
                None
            }
        }
    }

    /// Which "question" a command asks, so its answer is sent as the matching
    /// event rather than as a plain `Applied`.
    #[derive(Debug, Clone, Copy)]
    enum Question {
        Devices,
        Apps,
        Status,
        Other,
    }

    impl Question {
        fn reply(
            self,
            answer: Result<Value, holaf_core::backend::BackendError>,
            event_tx: &Sender<EngineEvent>,
        ) {
            match (self, answer) {
                (Question::Devices, Ok(value)) => {
                    let _ = event_tx.send(EngineEvent::Devices(engine_json::devices(&value)));
                }
                (Question::Apps, Ok(value)) => {
                    let _ = event_tx.send(EngineEvent::Apps(AppList::parse(&value)));
                }
                (Question::Status, Ok(value)) => {
                    let _ = event_tx.send(EngineEvent::Status(value));
                }
                (Question::Other, Ok(_)) => {}
                (_, Err(err)) => {
                    // A refused question is connection-level: the dialog shows
                    // "unavailable" with this reason.
                    let _ = event_tx.send(EngineEvent::Error(err.to_string()));
                }
            }
        }
    }

    fn command_question(command: &EngineCommand) -> Question {
        match command {
            EngineCommand::DeviceList => Question::Devices,
            EngineCommand::AppList => Question::Apps,
            EngineCommand::Status => Question::Status,
            _ => Question::Other,
        }
    }

    /// Ask the engine which sources to record and start/stop captures to match.
    ///
    /// `backend` is `None` in disconnected mode: the captures are all stopped.
    fn refresh_meters(
        backend: &mut Option<holaf_pw::PipeWireBackend>,
        capture: &mut crate::capture::MeterCapture,
        board: &MeterBoard,
        uids: &mut Vec<String>,
    ) {
        let Some(backend) = backend.as_mut() else {
            capture.stop_all();
            uids.clear();
            return;
        };
        let sources = match backend.meter_sources() {
            Ok(value) => engine_json::meter_sources(&value),
            Err(err) => {
                log::debug!("metering: meter_sources failed: {err}");
                capture.stop_all();
                uids.clear();
                return;
            }
        };
        if sources.is_empty() {
            if !capture.is_empty() {
                log::debug!("metering: the engine reports no source");
            }
            capture.stop_all();
            uids.clear();
            return;
        }
        // Sorted, so the slot assignment (and the logs) are deterministic.
        let mut wanted: Vec<String> = sources.keys().cloned().collect();
        wanted.sort();
        let slots = board.assign(&wanted);
        // `sync` takes the uid→source map; `wanted` only drives the slot order.
        capture.sync(&sources, &slots, board);
        *uids = wanted;
    }

    /// Map one [`EngineCommand`] onto the `Backend` trait.
    ///
    /// `backend` is `None` in disconnected mode: every operation is refused
    /// with the connection reason (the UI rolls its optimistic change back and
    /// shows the reason), which is what makes "no daemon" a banner and not a
    /// freeze.
    fn dispatch(
        backend: Option<&mut holaf_pw::PipeWireBackend>,
        command: &EngineCommand,
        snapshot: &[Strip],
    ) -> Result<String, String> {
        let Some(backend) = backend else {
            return Err(format!(
                "the audio engine is not connected - {command:?} was not applied"
            ));
        };
        let describe = |what: &str, uid: &str| format!("{what} {uid}");
        let value = match command {
            EngineCommand::SetVolume { uid, volume } => backend
                .set_volume(uid, *volume)
                .map(|_| describe("set_volume", uid)),
            EngineCommand::SetMute { uid, mute } => backend
                .set_mute(uid, *mute)
                .map(|_| describe("set_mute", uid)),
            EngineCommand::SetMono { uid, mono } => backend
                .set_mono(uid, *mono)
                .map(|_| describe("set_mono", uid)),
            EngineCommand::SetEffect {
                uid,
                effect,
                active,
            } => backend
                .fx_set_active(uid, *effect, *active)
                .map(|_| format!("set_effect {uid}.{effect}={active}")),
            EngineCommand::SetFxParams {
                uid,
                effect,
                params,
            } => backend
                .fx_set_params(uid, *effect, params)
                .map(|_| format!("set_fx_params {uid}.{effect}={} ports", params.len())),
            EngineCommand::SetRoute {
                source,
                target,
                active,
            } => backend
                .routing_set(source, target, *active)
                .map(|_| format!("set_route {source}->{target}={active}")),
            EngineCommand::SetDefaultSink { uid } => backend
                .set_default_sink(uid)
                .map(|_| describe("set_default_sink", uid)),
            EngineCommand::SetDevice { uid, device } => {
                // Rebinding a strip's device is a structural change: the graph
                // plan is rebuilt from the snapshot, with the device patched in.
                let mut strips = snapshot.to_vec();
                let mut found = false;
                for strip in &mut strips {
                    if &strip.uid == uid {
                        strip.device_name = device.clone();
                        found = true;
                    }
                }
                if !found {
                    return Err(format!("cannot rebind unknown strip {uid}"));
                }
                backend
                    .apply_snapshot(&strips)
                    .map(|_| format!("set_device {uid}={device:?}"))
            }
            EngineCommand::RemoveStrip { uid } => backend
                .apply_snapshot(snapshot)
                .map(|_| describe("remove_strip", uid)),
            EngineCommand::SyncSnapshot(strips) => backend
                .apply_snapshot(strips)
                .map(|_| format!("snapshot {} strips", strips.len())),
            EngineCommand::AppMove { app_id, sink } => {
                backend.app_move(app_id, sink).and_then(|value| {
                    // The daemon answers `ok: false` with a `note` when the move
                    // did not land (an unknown stream index, a sink that
                    // vanished, a session manager that refused the link).
                    // Reporting that as a failure is the honest thing to do —
                    // the UI then says so instead of pretending the app moved.
                    if value.get("ok").and_then(Value::as_bool) == Some(false) {
                        let note = value
                            .get("note")
                            .and_then(Value::as_str)
                            .unwrap_or("the engine refused the move");
                        Err(holaf_core::backend::BackendError::new(
                            "app_move_refused",
                            note.to_string(),
                        ))
                    } else {
                        Ok(format!("app_move {app_id}->{sink}"))
                    }
                })
            }
            // Answered separately (they return a payload, not an operation).
            EngineCommand::DeviceList | EngineCommand::AppList | EngineCommand::Status => {
                Ok("question".to_string())
            }
            EngineCommand::RestartEngine => Ok("restart".to_string()),
            EngineCommand::Shutdown => Ok("shutdown".to_string()),
        };
        value.map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_fresh_bridge_reports_ready() {
        let bridge = EngineBridge::spawn_mock();
        // The thread emits `Ready` asynchronously: poll with a bounded wait.
        let mut events = Vec::new();
        for _ in 0..200 {
            events.extend(bridge.drain_events());
            if !events.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(events, vec![EngineEvent::Ready]);
        assert!(bridge.is_connected());
        let status = Arc::clone(bridge.status());
        bridge.shutdown();
        assert!(!status.connected());
    }

    #[test]
    fn commands_are_applied_by_the_engine_thread() {
        let bridge = EngineBridge::spawn_mock();
        bridge
            .send(EngineCommand::SetVolume {
                uid: "in-mic".to_string(),
                volume: 0.5,
            })
            .expect("send");
        // Wait for the echo (the thread is async): the event channel is the
        // synchronisation point, so poll with a bounded wait.
        let mut events = Vec::new();
        for _ in 0..200 {
            events.extend(bridge.drain_events());
            if events.iter().any(|e| matches!(e, EngineEvent::Levels(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            events.iter().any(|e| matches!(
                e,
                EngineEvent::Applied(what) if what.contains("set_volume")
            )),
            "{events:?}"
        );
        let levels = events
            .iter()
            .find_map(|e| match e {
                EngineEvent::Levels(map) => Some(map.clone()),
                _ => None,
            })
            .expect("a levels event");
        assert_eq!(levels["in-mic"], [0.3, 0.25]);
        assert_eq!(bridge.levels()["in-mic"], [0.3, 0.25]);
        bridge.shutdown();
    }

    #[test]
    fn mute_zeroes_the_meters() {
        let bridge = EngineBridge::spawn_mock();
        bridge
            .send(EngineCommand::SetMute {
                uid: "in-web".to_string(),
                mute: true,
            })
            .unwrap();
        for _ in 0..200 {
            let events = bridge.drain_events();
            if events.iter().any(|e| matches!(e, EngineEvent::Levels(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(bridge.levels()["in-web"], [0.0, 0.0]);
        bridge.shutdown();
    }

    #[test]
    fn the_mock_answers_a_device_list_request() {
        let bridge = EngineBridge::spawn_mock();
        assert!(
            bridge.meter_board().is_none(),
            "the mock has no level board (it reports levels through events)"
        );
        bridge.send(EngineCommand::DeviceList).unwrap();
        let mut devices = Vec::new();
        for _ in 0..200 {
            for event in bridge.drain_events() {
                if let EngineEvent::Devices(list) = event {
                    devices = list;
                }
            }
            if !devices.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(devices.len(), 4, "the mock lists two sinks and two sources");
        assert_eq!(devices.iter().filter(|device| device.is_sink()).count(), 2);
        assert_eq!(devices.iter().filter(|device| device.is_source()).count(), 2);
        // Two sinks with two *different* labels: the whole point of the list.
        let labels: Vec<String> = devices
            .iter()
            .filter(|device| device.is_sink())
            .map(|device| device.label())
            .collect();
        assert_eq!(
            labels,
            vec![
                "Built-in Audio Analog Stereo".to_string(),
                "PreSonus Studio 26c · pro-output-0".to_string(),
            ]
        );
        bridge.shutdown();
    }

    #[test]
    fn sending_after_shutdown_is_reported_not_panicked() {
        let bridge = EngineBridge::spawn_mock();
        let commands = bridge.commands.clone();
        bridge.shutdown();
        // The receiver is gone: a send must return an error, not panic.
        let result = commands.send(EngineCommand::SetMute {
            uid: "x".to_string(),
            mute: false,
        });
        assert!(result.is_err());
    }

    #[test]
    fn dropping_the_bridge_stops_the_thread() {
        let bridge = EngineBridge::spawn_mock();
        let commands = bridge.commands.clone();
        drop(bridge);
        // Dropping joined the thread, so its receiver is closed.
        assert!(commands.send(EngineCommand::Shutdown).is_err());
    }

    #[test]
    fn a_custom_engine_closure_can_fail_without_killing_the_thread() {
        let bridge = EngineBridge::spawn_with(|command, _levels| match command {
            EngineCommand::SetDefaultSink { uid } if uid == "nope" => Err("no such sink".into()),
            _ => Ok("ok".to_string()),
        });
        let refused = EngineCommand::SetDefaultSink {
            uid: "nope".to_string(),
        };
        bridge.send(refused.clone()).unwrap();
        let mut events = Vec::new();
        for _ in 0..200 {
            events.extend(bridge.drain_events());
            if events
                .iter()
                .any(|e| matches!(e, EngineEvent::Failed { .. }))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Failed { command, reason }
                    if command == &refused && reason.contains("no such sink")
            )),
            "{events:?}"
        );
        // The thread survived: a second command still gets an answer.
        bridge
            .send(EngineCommand::SetMute {
                uid: "u".to_string(),
                mute: true,
            })
            .unwrap();
        assert!(bridge.send(EngineCommand::Shutdown).is_ok());
    }

    #[test]
    fn bridge_error_displays_a_useful_message() {
        assert!(BridgeError::Disconnected
            .to_string()
            .contains("not running"));
    }

    #[test]
    fn the_status_follows_the_thread_lifecycle() {
        let bridge = EngineBridge::spawn_mock();
        for _ in 0..200 {
            bridge.drain_events();
            if bridge.is_connected() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(bridge.is_connected());
        assert_eq!(bridge.status().error(), None);
        let status = Arc::clone(bridge.status());
        bridge.shutdown();
        assert!(!status.connected());
    }
}
