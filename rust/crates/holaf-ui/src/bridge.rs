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
//!   AppState::poll_engine()
//!        ◄──EngineEvent ───mpsc──────── send(Applied | Levels | Error)
//! ```
//!
//! Design rules kept from the Python app:
//!
//! * **The UI never blocks.** Sends are non-blocking; a dead engine shows up as
//!   [`BridgeError::Disconnected`] in the status bar, not as a freeze.
//! * **Levels are polled, not pushed per sample.** `METER_POLL_INTERVAL_MS = 40`
//!   in Python; here the mock engine emits a `Levels` event per command and the
//!   real one will emit at its own cadence.
//! * **Metering itself stays outside the daemon** (spec §6): the daemon only
//!   names the source to record, so a real bridge feeds `Levels` from its own
//!   capture.  That is the one piece left for the engine-integration step.
//!
//! # Feature `engine-pw`
//!
//! Off by default so the crate builds (and its tests run) on a machine without
//! libpipewire.  Enabled, [`EngineBridge::spawn_pipewire`] drives a real
//! [`holaf_pw::PipeWireBackend`] on the engine thread.  It cannot be exercised
//! by the unit tests (it needs a live PipeWire daemon), which is why the mock
//! exists.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use holaf_core::model::{EffectKey, Strip};

/// Which controller property a MIDI learn is armed for
/// (`midi_learn_requested(uid, "volume" | "mute" | "mono")`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MidiProperty {
    Volume,
    Mute,
    Mono,
}

impl MidiProperty {
    pub fn as_str(self) -> &'static str {
        match self {
            MidiProperty::Volume => "volume",
            MidiProperty::Mute => "mute",
            MidiProperty::Mono => "mono",
        }
    }
}

/// One engine operation, i.e. one `holaf_core::backend::Backend` method.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineCommand {
    /// `Backend::set_volume` — 0.0–1.0 nominal, may exceed 1.0.
    SetVolume { uid: String, volume: f32 },
    /// `Backend::set_mute`.
    SetMute { uid: String, mute: bool },
    /// `Backend::set_mono` (stereo → mono downmix).
    SetMono { uid: String, mono: bool },
    /// `Backend::fx_set_active`.
    SetEffect {
        uid: String,
        effect: EffectKey,
        active: bool,
    },
    /// `Backend::routing_set`.
    SetRoute {
        source: String,
        target: String,
        active: bool,
    },
    /// `Backend::set_default_sink`.
    SetDefaultSink { uid: String },
    /// Device (re)binding: `None` makes the output a virtual bus.
    SetDevice {
        uid: String,
        device: Option<String>,
    },
    /// A strip disappeared: the next [`EngineCommand::SyncSnapshot`] reflects it.
    RemoveStrip { uid: String },
    /// `Backend::apply_snapshot` — sent after a structural change (add, delete,
    /// rename, route edit) because the graph plan depends on the whole list.
    SyncSnapshot(Vec<Strip>),
    /// Tear the engine down (a fresh daemon is the point of a restart).
    Shutdown,
}

/// What the engine thread reports back.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// The backend connected (or, for the mock, the thread started).
    Ready,
    /// Meter values per strip uid, in `0.0–1.0`.
    Levels(HashMap<String, [f32; 2]>),
    /// An operation succeeded; the payload names it (for logging/tests).
    Applied(String),
    /// An operation failed.  Shown in the status bar, never fatal.
    Error(String),
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

/// Handle to the engine thread.  Dropping it shuts the thread down.
pub struct EngineBridge {
    commands: Sender<EngineCommand>,
    events: Receiver<EngineEvent>,
    handle: Option<JoinHandle<()>>,
    levels: Arc<Mutex<HashMap<String, [f32; 2]>>>,
}

impl std::fmt::Debug for EngineBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineBridge")
            .field("running", &self.handle.is_some())
            .finish()
    }
}

impl EngineBridge {
    /// Start a **mock** engine thread.
    ///
    /// It applies commands to an in-memory level map and echoes an `Applied`
    /// event, with no timing dependency, so tests are deterministic.  It is
    /// also what the snapshot binary uses: the layout must be reviewable
    /// without an audio server.
    pub fn spawn_mock() -> Self {
        Self::spawn_with(|command: &EngineCommand, levels: &mut HashMap<String, [f32; 2]>| {
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
                EngineCommand::SyncSnapshot(strips) => Ok(format!("snapshot {} strips", strips.len())),
                EngineCommand::Shutdown => Ok("shutdown".to_string()),
            }
        })
    }

    /// Start an engine thread driven by `apply(command, levels) -> Result<name>`.
    ///
    /// The closure runs **on the engine thread** and owns the backend, which is
    /// how the PipeWire implementation will keep its objects where they were
    /// created (`holaf_pw` requires the mainloop to live in the owning thread).
    pub fn spawn_with(
        apply: impl Fn(
                &EngineCommand,
                &mut HashMap<String, [f32; 2]>,
            ) -> Result<String, String>
            + Send
            + 'static,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel::<EngineCommand>();
        let (event_tx, event_rx) = mpsc::channel::<EngineEvent>();
        let levels = Arc::new(Mutex::new(HashMap::new()));
        let thread_levels = Arc::clone(&levels);

        let handle = thread::Builder::new()
            .name("holaf-ui-engine".to_string())
            .spawn(move || {
                let _ = event_tx.send(EngineEvent::Ready);
                let mut local_levels: HashMap<String, [f32; 2]> = HashMap::new();
                loop {
                    let command = match command_rx.recv() {
                        Ok(command) => command,
                        Err(_) => break, // the UI side dropped the bridge
                    };
                    let stopping = matches!(command, EngineCommand::Shutdown);
                    let touched_levels = matches!(
                        command,
                        EngineCommand::SetVolume { .. }
                            | EngineCommand::SetMute { .. }
                            | EngineCommand::RemoveStrip { .. }
                    );
                    match apply(&command, &mut local_levels) {
                        Ok(what) => {
                            let _ = event_tx.send(EngineEvent::Applied(what));
                        }
                        Err(reason) => {
                            let _ = event_tx.send(EngineEvent::Error(reason));
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
            })
            .expect("the engine thread must start");

        Self {
            commands: command_tx,
            events: event_rx,
            handle: Some(handle),
            levels,
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
    }
}

impl Drop for EngineBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Optional real PipeWire engine (feature `engine-pw`)
// ---------------------------------------------------------------------------

/// The real PipeWire engine.  The `impl EngineBridge` block inside adds
/// [`EngineBridge::spawn_pipewire`] when the feature is on; nothing else needs
/// re-exporting.
#[cfg(feature = "engine-pw")]
mod pw {
    use super::*;
    use holaf_core::backend::Backend;

    impl EngineBridge {
        /// Drive a real [`holaf_pw::PipeWireBackend`].
        ///
        /// The backend is created **inside the engine thread**: `PipeWireBackend`
        /// owns a `pipewire` mainloop that must live in the thread that will
        /// iterate it (see `holaf-pw`'s threading notes), so constructing it on
        /// the UI thread and moving it would be wrong even though the type is
        /// `Send`.
        ///
        /// Metering is *not* provided here: `Backend::meter_sources()` only
        /// names the PulseAudio sources to record, and capturing them is the
        /// remaining piece of the engine-integration step (see
        /// `docs/ui-rust-design.md`).
        pub fn spawn_pipewire() -> Self {
            let (command_tx, command_rx) = mpsc::channel::<EngineCommand>();
            let (event_tx, event_rx) = mpsc::channel::<EngineEvent>();
            let levels = Arc::new(Mutex::new(HashMap::new()));

            let handle = thread::Builder::new()
                .name("holaf-ui-engine-pw".to_string())
                .spawn(move || {
                    let mut backend = match holaf_pw::PipeWireBackend::connect() {
                        Ok(backend) => {
                            let _ = event_tx.send(EngineEvent::Ready);
                            backend
                        }
                        Err(err) => {
                            let _ = event_tx.send(EngineEvent::Error(format!(
                                "cannot connect to PipeWire: {err}"
                            )));
                            return;
                        }
                    };
                    // The strips the last `SyncSnapshot` carried; `routing_set`
                    // and friends need the ids only, but a full re-apply after a
                    // structural change needs the list.
                    let mut snapshot: Vec<Strip> = Vec::new();
                    while let Ok(command) = command_rx.recv() {
                        if matches!(command, EngineCommand::Shutdown) {
                            break;
                        }
                        if let EngineCommand::SyncSnapshot(strips) = &command {
                            snapshot = strips.clone();
                        }
                        let result = dispatch(&mut backend, &command, &snapshot);
                        match result {
                            Ok(what) => {
                                let _ = event_tx.send(EngineEvent::Applied(what));
                            }
                            Err(reason) => {
                                let _ = event_tx.send(EngineEvent::Error(reason));
                            }
                        }
                    }
                    if let Err(err) = backend.stop() {
                        log::warn!("engine shutdown: {err}");
                    }
                })
                .expect("the engine thread must start");

            Self {
                commands: command_tx,
                events: event_rx,
                handle: Some(handle),
                levels,
            }
        }
    }

    /// Map one [`EngineCommand`] onto the `Backend` trait.
    fn dispatch(
        backend: &mut holaf_pw::PipeWireBackend,
        command: &EngineCommand,
        snapshot: &[Strip],
    ) -> Result<String, String> {
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
            EngineCommand::Shutdown => Ok("shutdown".to_string()),
        };
        value.map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(events, vec![EngineEvent::Ready]);
        bridge.shutdown();
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
            std::thread::sleep(std::time::Duration::from_millis(5));
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
            if events
                .iter()
                .any(|e| matches!(e, EngineEvent::Levels(_)))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(bridge.levels()["in-web"], [0.0, 0.0]);
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
        bridge
            .send(EngineCommand::SetDefaultSink {
                uid: "nope".to_string(),
            })
            .unwrap();
        let mut events = Vec::new();
        for _ in 0..200 {
            events.extend(bridge.drain_events());
            if events
                .iter()
                .any(|e| matches!(e, EngineEvent::Error(_)))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EngineEvent::Error(reason) if reason.contains("no such sink"))),
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
    fn midi_property_names_match_the_python_strings() {
        assert_eq!(MidiProperty::Volume.as_str(), "volume");
        assert_eq!(MidiProperty::Mute.as_str(), "mute");
        assert_eq!(MidiProperty::Mono.as_str(), "mono");
    }

    #[test]
    fn bridge_error_displays_a_useful_message() {
        assert!(BridgeError::Disconnected
            .to_string()
            .contains("not running"));
    }
}
