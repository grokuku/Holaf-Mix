//! [`PipeWireBackend`] — the real [`Backend`] implementation driving a live
//! PipeWire daemon from the `holaf-pw` engine.
//!
//! # Architecture
//!
//! PipeWire's `Context`/`Core`/`MainLoop` are `!Send` (they use `Rc`
//! internally), while the [`Backend`] trait requires `Send` (the daemon holds
//! `Mutex<Box<dyn Backend>>` shared across connection threads).  The backend
//! therefore **marshals** every operation to a dedicated worker thread that
//! owns the PipeWire objects:
//!
//! ```text
//! daemon IPC ──&mut PipeWireBackend (Send handle)
//!                     │ mpsc (op + reply)
//!                     ▼
//!              worker thread ── Engine (Context/Core/MainLoop + graph)
//! ```
//!
//! Each trait method sends one [`EngineOp`] and blocks on the reply channel
//! (synchronous RPC, bounded by [`REQUEST_TIMEOUT`]).  This mirrors the
//! Python `RLock` semantics and the rewrite spec §4 serialised-order model;
//! the mainloop is only iterated while an operation needs a round-trip.
//!
//! # Pointing at a daemon
//!
//! The worker connects through the process environment — PipeWire honours
//! `XDG_RUNTIME_DIR`, `PIPEWIRE_RUNTIME_DIR` and `PIPEWIRE_REMOTE`, so
//! running the daemon against the headless test server is purely:
//!
//! ```bash
//! bash /opt/holaf-headless/start-pipewire-headless.sh
//! export XDG_RUNTIME_DIR=/tmp/pw-runtime PIPEWIRE_RUNTIME_DIR=/tmp/pw-runtime \
//!        DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/pw-runtime/bus
//! holafd --backend pipewire
//! ```

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread;
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Value;

use holaf_core::backend::{Backend, BackendError};
use holaf_core::model::{EffectKey, Strip};

use crate::engine::{Engine, EngineOp};

/// How long an operation may block on the PipeWire worker before the backend
/// reports `backend_timeout`.  Real graph ops (chain loads, registry polls)
/// can take a few seconds each.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Message sent to the worker thread.
enum WorkerMsg {
    /// Execute one engine operation and reply.
    Op {
        op: EngineOp,
        reply: SyncSender<Result<Value, BackendError>>,
    },
    /// Tear the graph down and leave the worker.
    Quit { reply: SyncSender<Result<(), BackendError>> },
}

fn backend_down() -> BackendError {
    BackendError::new("backend_down", "the PipeWire worker thread is gone")
}

fn backend_timeout(op: &str) -> BackendError {
    BackendError::new(
        "backend_timeout",
        format!("operation '{op}' timed out waiting for the PipeWire worker"),
    )
}

/// A `Send` handle to the real PipeWire engine (see module docs).
pub struct PipeWireBackend {
    tx: Sender<WorkerMsg>,
    worker: Option<thread::JoinHandle<()>>,
    connected: bool,
}

impl PipeWireBackend {
    /// Connect to the PipeWire daemon of the process environment and spawn
    /// the worker thread that owns the graph.
    pub fn connect() -> Result<Self, BackendError> {
        let (tx, rx) = mpsc::channel::<WorkerMsg>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), BackendError>>(1);

        let worker = thread::Builder::new()
            .name("holaf-pw".into())
            .spawn(move || {
                let engine = match Engine::connect() {
                    Ok(engine) => {
                        let _ = ready_tx.send(Ok(()));
                        engine
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                worker_loop(rx, engine);
            })
            .map_err(|e| BackendError::internal("spawn pipewire worker", e))?;

        match ready_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(())) => Ok(PipeWireBackend {
                tx,
                worker: Some(worker),
                connected: true,
            }),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(BackendError::new(
                    "pipewire_error",
                    "timed out waiting for the PipeWire worker to connect",
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(BackendError::new(
                    "pipewire_error",
                    "the PipeWire worker exited before acknowledging connection",
                ))
            }
        }
    }

    fn call(&self, op: EngineOp) -> Result<Value, BackendError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<Value, BackendError>>(1);
        self.tx
            .send(WorkerMsg::Op { op, reply: reply_tx })
            .map_err(|_| backend_down())?;
        match reply_rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(backend_timeout("engine op")),
            Err(RecvTimeoutError::Disconnected) => Err(backend_down()),
        }
    }

    /// Ask the worker to tear the graph down and exit (used by `stop`/`Drop`).
    fn shutdown(&mut self) {
        if let Some(worker) = self.worker.take() {
            let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<(), BackendError>>(1);
            let _ = self.tx.send(WorkerMsg::Quit { reply: reply_tx });
            // Best-effort: wait a bounded time for the worker to finish, then
            // detach (the process may be exiting anyway).
            let _ = reply_rx.recv_timeout(Duration::from_secs(30));
            let _ = worker.join();
        }
        self.connected = false;
    }
}

fn worker_loop(rx: Receiver<WorkerMsg>, mut engine: Engine) {
    while let Ok(msg) = rx.recv() {
        match msg {
            WorkerMsg::Op { op, reply } => {
                let result = engine.handle(op);
                if reply.send(result).is_err() {
                    // The caller disconnected (daemon shutting down) — stop.
                    break;
                }
            }
            WorkerMsg::Quit { reply } => {
                let result = engine.stop();
                let _ = reply.send(result);
                break;
            }
        }
    }
}

impl Backend for PipeWireBackend {
    fn apply_snapshot(&mut self, strips: &[Strip]) -> Result<Value, BackendError> {
        let op = EngineOp::ApplySnapshot(strips.to_vec());
        self.call(op)
    }

    fn stop(&mut self) -> Result<(), BackendError> {
        // Ask the engine to tear down the graph, then stop the worker.
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<Value, BackendError>>(1);
        let op = EngineOp::Stop;
        self.tx
            .send(WorkerMsg::Op { op, reply: reply_tx })
            .map_err(|_| backend_down())?;
        match reply_rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(r) => r.map(|_| ()),
            Err(RecvTimeoutError::Timeout) => Err(backend_timeout("engine.stop")),
            Err(RecvTimeoutError::Disconnected) => Err(backend_down()),
        }
    }

    fn status(&mut self) -> Result<Value, BackendError> {
        self.call(EngineOp::Status)
    }

    fn set_volume(&mut self, uid: &str, value: f32) -> Result<Value, BackendError> {
        self.call(EngineOp::SetVolume {
            uid: uid.to_string(),
            value,
        })
    }

    fn set_mute(&mut self, uid: &str, muted: bool) -> Result<Value, BackendError> {
        self.call(EngineOp::SetMute {
            uid: uid.to_string(),
            muted,
        })
    }

    fn set_mono(&mut self, uid: &str, mono: bool) -> Result<Value, BackendError> {
        self.call(EngineOp::SetMono {
            uid: uid.to_string(),
            mono,
        })
    }

    fn fx_set_active(
        &mut self,
        uid: &str,
        effect: EffectKey,
        active: bool,
    ) -> Result<Value, BackendError> {
        self.call(EngineOp::FxSetActive {
            uid: uid.to_string(),
            effect,
            active,
        })
    }

    fn fx_set_params(
        &mut self,
        uid: &str,
        effect: EffectKey,
        params: &IndexMap<String, f32>,
    ) -> Result<Value, BackendError> {
        self.call(EngineOp::FxSetParams {
            uid: uid.to_string(),
            effect,
            params: params.clone(),
        })
    }

    fn routing_set(
        &mut self,
        source_uid: &str,
        target_uid: &str,
        active: bool,
    ) -> Result<Value, BackendError> {
        self.call(EngineOp::RoutingSet {
            source_uid: source_uid.to_string(),
            target_uid: target_uid.to_string(),
            active,
        })
    }

    fn routing_list(&mut self) -> Result<Value, BackendError> {
        self.call(EngineOp::RoutingList)
    }

    fn set_default_sink(&mut self, uid: &str) -> Result<Value, BackendError> {
        self.call(EngineOp::SetDefaultSink {
            uid: uid.to_string(),
        })
    }

    fn device_list(&mut self) -> Result<Value, BackendError> {
        self.call(EngineOp::DeviceList)
    }

    fn app_list(&mut self) -> Result<Value, BackendError> {
        self.call(EngineOp::AppList)
    }

    fn app_move(&mut self, app_id: &str, sink_name: &str) -> Result<Value, BackendError> {
        self.call(EngineOp::AppMove {
            app_id: app_id.to_string(),
            sink_name: sink_name.to_string(),
        })
    }
}

impl Drop for PipeWireBackend {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_error_helpers_are_stable() {
        let e = backend_down();
        assert_eq!(e.code, "backend_down");
        let t = backend_timeout("x");
        assert_eq!(t.code, "backend_timeout");
    }
}
