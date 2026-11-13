//! [`PipeWireBackend`] — the real [`Backend`] implementation driving a live
//! PipeWire daemon from the `holaf-pw` engine.
//!
//! # Architecture
//!
//! PipeWire's `Context`/`Core`/`MainLoop` are `!Send` (they use `Rc`
//! internally), while the [`Backend`] trait requires `Send` (the daemon moves
//! `Box<dyn Backend>` to the thread that owns it).  The backend therefore
//! **marshals** every operation to a dedicated worker thread that owns the
//! PipeWire objects:
//!
//! ```text
//! daemon IPC ──holafd backend owner thread──► &mut PipeWireBackend (Send handle)
//!                                                     │ mpsc (op + reply)
//!                                                     ▼
//!                                              worker thread ── Engine (Context/Core/MainLoop + graph)
//! ```
//!
//! Each trait method sends one [`EngineOp`] and blocks on the reply channel
//! (synchronous RPC, bounded by [`REQUEST_TIMEOUT`]).  This mirrors the
//! Python `RLock` semantics and the rewrite spec §4 serialised-order model.
//!
//! The worker thread never blocks on the order channel: between orders it
//! iterates the PipeWire mainloop ([`worker_loop`]).  That is a **real-time
//! requirement**, not a nicety — the filter-chain modules we host live in this
//! process and their client nodes are only schedulable through mainloop
//! traffic, so a blocked mainloop silences (or, when partially served, chops)
//! every FX chain.
//!
//! # TODO: the worker is now redundant (analysis done, refactor not started)
//!
//! Since `holafd` dispatches *all* orders through a single backend owner thread
//! (see `holafd::server`), this second serialisation stage only adds one thread
//! hop and one timer per order.  Removing it needs three coordinated changes,
//! hence a dedicated pass:
//!
//! 1. `Server::bind_lazy(path, factory)` building the backend **on** the owner
//!    thread;
//! 2. drop the `Send` bound from `holaf_core::backend::Backend`;
//! 3. turn this type into a thin synchronous facade over `Engine` (no worker,
//!    no `REQUEST_TIMEOUT`; `Drop` → `engine.stop()`), the connection check
//!    moving into the factory (a failure there must fail the daemon startup
//!    cleanly).
//!
//! Correctness is unaffected today: exactly one order is in flight at any time
//! either way, and the daemon-side timeout (130 s) intentionally sits above
//! [`REQUEST_TIMEOUT`] so this backend's precise error wins.
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

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::thread;
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Value;

use holaf_core::backend::{Backend, BackendError};
use holaf_core::model::{EffectKey, Strip};

use crate::chainhost::FxHostMode;
use crate::engine::{Engine, EngineOp};

/// How long an operation may block on the PipeWire worker before the backend
/// reports `backend_timeout`.  Real graph ops (chain loads, registry polls)
/// can take a few seconds each.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Idle mainloop iteration budget of the worker thread (see [`worker_loop`]).
///
/// While this process hosts at least one filter-chain, the worker never blocks
/// on the command channel: between orders it services the PipeWire mainloop
/// with `iterate(IDLE_PUMP_TIMEOUT)`, which returns **as soon as a PipeWire
/// event arrives** — audio is therefore serviced with the mainloop's own
/// wake-up latency, never with this timeout.  The timeout only bounds how late
/// a *new order* may be noticed when the graph is completely silent (≤5 ms,
/// i.e. the pre-existing behaviour) and costs 200 epoll wake-ups per second
/// (measured 0.7 % CPU with a chain loaded).  With no chain hosted there is
/// nothing to service, so the worker blocks on the channel instead: 0 %.
const IDLE_PUMP_TIMEOUT: Duration = Duration::from_millis(5);

/// How often the worker checks that a **dedicated FX host is still alive**
/// while it has nothing else to do.
///
/// With the chains hosted in a child process the worker has no mainloop to
/// service, so it blocks on its order channel — but it must still notice the
/// child dying (a crash, an OOM kill, a user `kill`), otherwise the graph keeps
/// reporting a chain that no longer exists until some unrelated order happens
/// to call `load`.  Two `waitpid`-cheap wake-ups per second; a respawn is
/// immediate and the repair pass then re-links the restored chains.
const HOST_WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);

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
    /// the worker thread that owns the graph, hosting the FX chains in a
    /// dedicated child process (see [`FxHostMode`]).
    pub fn connect() -> Result<Self, BackendError> {
        PipeWireBackend::connect_with(FxHostMode::default())
    }

    /// Same, choosing who hosts the strips' filter-chains.
    pub fn connect_with(fx_host: FxHostMode) -> Result<Self, BackendError> {
        let (tx, rx) = mpsc::channel::<WorkerMsg>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), BackendError>>(1);

        let worker = thread::Builder::new()
            .name("holaf-pw".into())
            .spawn(move || {
                let engine = match Engine::connect_with(fx_host) {
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

/// Worker-thread loop: serve the PipeWire mainloop **and** the order queue.
///
/// # Why the loop does not block on `recv()`///
/// This thread owns the `Context`/`Core`/`MainLoop`, and the filter-chain
/// modules of the strips are loaded **into this context**
/// ([`Engine::create_chain`] → `pw_context_load_module`).  A module's two
/// client nodes (capture/playback streams) only become schedulable through
/// mainloop traffic: format and buffer negotiation, node activation, and
/// `set_param` delivery are all dispatched from `pw_loop_iterate`, and the
/// RT `process` callbacks afterwards run on libpipewire's data-loop thread —
/// which the mainloop has to arm first.
///
/// A blocking `recv()` here leaves those nodes unserviced for as long as no
/// IPC order arrives.  Measured on the headless PipeWire 0.3.65 sandbox, the
/// consequences are:
///
/// | mainloop service | observed behaviour of a live EQ chain |
/// |---|---|
/// | none (idle `recv()`) | the graph **never starts**: `pw-cat` into `input.Holaf_FX_<uid>` hangs in `connecting` forever, chain nodes stay `suspended`, capture is silent |
/// | only inside IPC orders (a 25 Hz `status` poll) | the chain runs and the DSP is exact (RMS/THD identical to the Python path), but the node is starved between polls — a timing-sensitive device (USB card, 1 ms packets) drops packets → continuous fast on/off chopping |
/// | continuously (`iterate`) | chain runs, `pw-top` ERR stays 0, output bit-identical to the Python engine |
///
/// The Python engine never hit this because it hosts its chains in a dedicated
/// `pw-cli` process whose whole job is running the mainloop.  We keep one
/// thread (the PipeWire objects are `!Send`) but stop blocking it.
fn worker_loop(rx: Receiver<WorkerMsg>, mut engine: Engine) {
    loop {
        // Run every order that is already queued before going back to the
        // mainloop, so a burst of orders never interleaves with audio service.
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    if run_order(&mut engine, msg) == Flow::Stop {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        if engine.needs_idle_pumping() {
            // Keep the hosted filter-chains serviced (see the doc comment).
            // A negative return means the loop could not be iterated (e.g. a
            // signal interrupted it): back off instead of spinning hot.
            if engine.pump_mainloop(IDLE_PUMP_TIMEOUT) < 0 {
                thread::sleep(Duration::from_millis(1));
            }
            if engine.repair_due() {
                // Finish whatever the last snapshot could not build (a device
                // that appeared late, ports published late …) without waiting
                // for another client order.
                engine.repair_pending();
            }
        } else if let Some(delay) = engine.repair_delay() {
            // Nothing of ours runs in this process, but the graph is
            // incomplete: wake up for the next order *or* the repair deadline,
            // whichever comes first (no mainloop pumping needed here).
            match rx.recv_timeout(delay.max(Duration::from_millis(1))) {
                Ok(msg) => {
                    if run_order(&mut engine, msg) == Flow::Stop {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => engine.repair_pending(),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else if engine.hosts_external() {
            // Nothing of ours runs in this process (a child hosts the FX
            // chains) and the graph is complete: sleep on the channel, but wake
            // up periodically to check that the child is still there.
            match rx.recv_timeout(HOST_WATCHDOG_INTERVAL) {
                Ok(msg) => {
                    if run_order(&mut engine, msg) == Flow::Stop {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => engine.maintain(),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            // Nothing of ours runs anywhere and the graph is complete: block on
            // the channel (zero CPU) and wake up as soon as an order arrives.
            match rx.recv() {
                Ok(msg) => {
                    if run_order(&mut engine, msg) == Flow::Stop {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }
}

/// Whether the worker loop must keep running after an order was executed.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// Execute one queued order / shutdown request and report what to do next.
fn run_order(engine: &mut Engine, msg: WorkerMsg) -> Flow {
    match msg {
        WorkerMsg::Op { op, reply } => {
            let result = engine.handle(op);
            if reply.send(result).is_err() {
                // The caller disconnected (daemon shutting down).
                return Flow::Stop;
            }
            Flow::Continue
        }
        WorkerMsg::Quit { reply } => {
            let result = engine.stop();
            let _ = reply.send(result);
            Flow::Stop
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

    fn meter_sources(&mut self) -> Result<Value, BackendError> {
        self.call(EngineOp::MeterSources)
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
