//! The NDJSON Unix-socket server: accept connections, read one request per
//! line, hand it to the **single backend owner thread**, write one response
//! per request.  A per-connection writer thread guarantees responses and
//! broadcast events never interleave mid-line on the socket.
//!
//! No async runtime: `std` threads + channels only (rewrite spec §4).
//!
//! # Concurrency model (spec §4: « ordres → mpsc consommé par un thread »)
//!
//! ```text
//!  client A ─┐  (one reader thread per connection)
//!  client B ─┼─► mpsc::Sender<Command> ───► backend owner thread
//!  client C ─┘         ▲                     owns `Box<dyn Backend>`
//!                      │  Command::Request { request, reply }
//!                      │  reply: mpsc::Sender<Response>
//!   reader thread ◄────┴──── Response ◄──── dispatch() (catch_unwind)
//!        │
//!        └─► per-connection writer thread ─► socket
//!
//!  Server::notify(event) ──► clients registry ──► per-connection writer
//!                            (a *separate* path, never through the order channel)
//! ```
//!
//! * **One owner thread** takes every order, exactly like the Python engine's
//!   `RLock`: total order, no interleaving, no `Mutex` around the engine.
//! * **Connection threads never block each other's socket reads**: a slow
//!   order only delays the responses of *other* orders (that serialisation is
//!   the point), never the accept loop nor a peer's `read()`.
//! * **Events bypass the order channel** on purpose.  A backend that reports
//!   progress from inside an order calls [`Server::notify`], which only locks
//!   the client registry — a lock the owner thread never takes while executing
//!   an order.  Routing events through the order channel would deadlock (the
//!   owner thread would wait on itself).
//! * **A panicking backend does not kill the daemon**: each order is run under
//!   `catch_unwind`; the client gets an `internal_error` response and the owner
//!   thread keeps serving (the backend state may of course be inconsistent
//!   afterwards — that is the backend's problem, and it is logged).
//! * **A dead owner thread is reported**: once the command channel is closed
//!   (or the reply channel disconnects) the client immediately gets
//!   `backend_unavailable` instead of blocking forever; a live-but-slow backend
//!   is bounded by [`CLIENT_RESPONSE_TIMEOUT`] (`backend_timeout`).
//!
//! `engine.stop` is an ordinary order: it queues behind the orders already
//! accepted, which is the correct semantic.  It cannot hang the daemon either —
//! the waiting client is bounded by [`CLIENT_RESPONSE_TIMEOUT`] and the owner
//! thread itself is bounded by the backend's own internal timeouts.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use holaf_core::migration::strip_from_dict;
use holaf_core::model::{EffectKey, Strip};

use holaf_ipc::{
    decode_line, encode, Event, Request, Response, METHOD_APPLY_SNAPSHOT, METHOD_APP_LIST,
    METHOD_APP_MOVE, METHOD_DEVICE_LIST, METHOD_FX_SET_ACTIVE, METHOD_FX_SET_PARAMS,
    METHOD_METER_SOURCES, METHOD_ROUTING_LIST, METHOD_ROUTING_SET, METHOD_STATUS, METHOD_STOP,
    METHOD_STRIP_SET_MONO, METHOD_STRIP_SET_MUTE, METHOD_STRIP_SET_VOLUME,
    METHOD_SYSTEM_SET_DEFAULT_SINK,
};

use crate::backend::{Backend, BackendError};

/// Poll interval of the (non-blocking) accept loop — how soon a shutdown
/// request is honoured.
const ACCEPT_POLL: Duration = Duration::from_millis(15);

/// How long a connection thread waits for its response before reporting
/// `backend_timeout`.  Deliberately **above** the PipeWire backend's own
/// 120 s request timeout (`holaf_pw`), so the backend's more precise error
/// (`backend_timeout` / `backend_down`) wins in practice.
const CLIENT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(130);

/// How long [`Server`] waits for the owner thread to acknowledge a shutdown
/// before detaching it (a slow order may still be running; the process is
/// exiting anyway).
const BACKEND_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Per-method params (spec §4)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ApplySnapshotParams {
    /// Raw strip entries: the daemon runs each through `holaf-core`'s
    /// migrations so legacy configs apply cleanly (old port names, boolean
    /// effects, stale `Model` param).
    #[serde(default)]
    strips: Vec<Value>,
}

#[derive(Deserialize)]
struct SetVolumeParams {
    uid: String,
    value: f32,
}

#[derive(Deserialize)]
struct SetMuteParams {
    uid: String,
    muted: bool,
}

#[derive(Deserialize)]
struct SetMonoParams {
    uid: String,
    mono: bool,
}

#[derive(Deserialize)]
struct FxSetActiveParams {
    uid: String,
    effect: EffectKey,
    active: bool,
}

#[derive(Deserialize)]
struct FxSetParamsParams {
    uid: String,
    effect: EffectKey,
    params: indexmap::IndexMap<String, f32>,
}

#[derive(Deserialize)]
struct RoutingSetParams {
    source_uid: String,
    target_uid: String,
    active: bool,
}

#[derive(Deserialize)]
struct DefaultSinkParams {
    uid: String,
}

#[derive(Deserialize)]
struct AppMoveParams {
    app_id: String,
    sink_name: String,
}

// ---------------------------------------------------------------------------
// Backend owner thread
// ---------------------------------------------------------------------------

/// One message for the thread that owns the [`Backend`].
enum Command {
    /// Execute one IPC order on the backend and reply on `reply`.
    Request {
        request: Request,
        reply: mpsc::Sender<Response>,
    },
    /// Leave the loop.  The backend is dropped right after, which runs its
    /// teardown (e.g. the PipeWire graph cleanup in `PipeWireBackend::drop`).
    Shutdown { ack: mpsc::Sender<()> },
}

/// Body of the owner thread: serialise every order on `backend`.
fn backend_loop(commands: mpsc::Receiver<Command>, mut backend: Box<dyn Backend>) {
    while let Ok(command) = commands.recv() {
        match command {
            Command::Request { request, reply } => {
                let response = execute(&mut *backend, request);
                if reply.send(response).is_err() {
                    log::debug!("backend: client gave up before the response was collected");
                }
            }
            Command::Shutdown { ack } => {
                log::debug!("backend: shutdown requested, leaving the owner thread");
                let _ = ack.send(());
                break;
            }
        }
    }
    // Dropping the backend here performs its teardown (PipeWire graph, worker
    // thread, …) exactly once, on the thread that owns it.
    drop(backend);
    log::debug!("backend: owner thread exited");
}

/// Run one order, turning a backend panic into an `internal_error` response so
/// a buggy order never takes the daemon down.
fn execute(backend: &mut dyn Backend, request: Request) -> Response {
    let id = request.id;
    let method = request.method.clone();
    match catch_unwind(AssertUnwindSafe(|| dispatch(backend, request))) {
        Ok(response) => response,
        Err(payload) => {
            let detail = panic_message(&*payload);
            log::error!("backend panicked while handling '{method}': {detail}");
            Response::err(
                id,
                "internal_error",
                format!("the backend panicked while handling '{method}': {detail}"),
            )
        }
    }
}

/// Best-effort extraction of a panic payload (`&str` / `String` / other).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Send one order to the owner thread and wait (bounded) for its response.
fn send_order(commands: &mpsc::Sender<Command>, request: Request) -> Response {
    let id = request.id;
    let (reply_tx, reply_rx) = mpsc::channel::<Response>();
    if commands
        .send(Command::Request {
            request,
            reply: reply_tx,
        })
        .is_err()
    {
        return backend_unavailable(id);
    }
    match reply_rx.recv_timeout(CLIENT_RESPONSE_TIMEOUT) {
        Ok(response) => response,
        Err(RecvTimeoutError::Timeout) => Response::err(
            id,
            "backend_timeout",
            format!("no response from the backend within {CLIENT_RESPONSE_TIMEOUT:?}"),
        ),
        Err(RecvTimeoutError::Disconnected) => backend_unavailable(id),
    }
}

/// The owner thread is gone: report it instead of hanging the client.
fn backend_unavailable(id: u64) -> Response {
    Response::err(
        id,
        "backend_unavailable",
        "the backend is not available (its owner thread has stopped)",
    )
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// A bound Unix-socket NDJSON server dispatching to one [`Backend`] owned by a
/// dedicated thread.
pub struct Server {
    listener: UnixListener,
    socket_path: PathBuf,
    /// Orders channel towards the backend owner thread.
    commands: mpsc::Sender<Command>,
    /// The owner thread, joined (best effort) when the server is dropped.
    backend_thread: Mutex<Option<JoinHandle<()>>>,
    /// Connected clients keyed by their connection id, used for broadcast
    /// events.  Each value is a channel towards that connection's writer
    /// thread.  **Never** touched by the owner thread while it runs an order,
    /// so a backend can broadcast from inside an order without deadlocking.
    clients: Arc<Mutex<HashMap<u64, mpsc::Sender<String>>>>,
    next_client: AtomicU64,
}

impl Server {
    /// Bind the listener on `path` and start the backend owner thread.  A
    /// stale socket file left behind by a previous crashed daemon is removed
    /// first; if the path is live the bind fails with `AddrInUse`.
    pub fn bind(path: impl AsRef<Path>, backend: impl Backend + 'static) -> io::Result<Server> {
        Self::bind_boxed(path, Box::new(backend))
    }

    /// Bind with an already-boxed backend (e.g. `UnimplementedBackend`).
    ///
    /// The backend is moved to its owner thread here; from then on it is only
    /// reachable through the order channel.
    pub fn bind_boxed(path: impl AsRef<Path>, backend: Box<dyn Backend>) -> io::Result<Server> {
        let path = path.as_ref().to_path_buf();
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                // Remove a stale socket and retry once.
                log::warn!(
                    "socket {} already exists — removing stale file",
                    path.display()
                );
                std::fs::remove_file(&path)?;
                UnixListener::bind(&path)?
            }
            Err(e) => return Err(e),
        };

        let (commands, commands_rx) = mpsc::channel::<Command>();
        let backend_thread = thread::Builder::new()
            .name("holafd-backend".into())
            .spawn(move || backend_loop(commands_rx, backend))?;

        Ok(Server {
            listener,
            socket_path: path,
            commands,
            backend_thread: Mutex::new(Some(backend_thread)),
            clients: Arc::new(Mutex::new(HashMap::new())),
            next_client: AtomicU64::new(1),
        })
    }

    /// The socket path this server is listening on.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Accept-and-dispatch loop until `shutdown` is set.
    ///
    /// Each accepted connection runs on its own thread (reader + writer), so
    /// the daemon is multi-client.  Returns `Ok(())` when asked to stop, or
    /// the first fatal accept error.
    pub fn run(&self, shutdown: Arc<AtomicBool>) -> io::Result<()> {
        self.listener.set_nonblocking(true)?;
        loop {
            if shutdown.load(Ordering::Relaxed) {
                log::debug!("shutdown requested — stopping the accept loop");
                return Ok(());
            }
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    let id = self.next_client.fetch_add(1, Ordering::Relaxed);
                    log::info!("client {id} connected");
                    let commands = self.commands.clone();
                    let clients = Arc::clone(&self.clients);
                    thread::spawn(move || handle_connection(stream, commands, clients, id));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(ACCEPT_POLL);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Handle a single request synchronously (no socket involved): the order
    /// still goes through the owner thread, so it is serialised with every
    /// other one.  Used by the daemon and by in-process tests.
    pub fn handle_request(&self, request: Request) -> Response {
        send_order(&self.commands, request)
    }

    /// Broadcast an event to every connected client (best effort).
    ///
    /// The core daemon calls this on state changes; a backend may also call it
    /// from inside an order — this path takes only the client registry lock,
    /// which the owner thread never holds while executing, so it cannot
    /// deadlock.  Returns the number of clients that actually received the
    /// event.
    pub fn notify(&self, event: &Event) -> usize {
        let line = encode(event);
        let mut dead = Vec::new();
        let mut delivered = 0usize;
        let mut guard = lock(&self.clients);
        for (id, sender) in guard.iter() {
            match sender.send(line.clone()) {
                Ok(()) => delivered += 1,
                Err(_) => dead.push(*id),
            }
        }
        for id in dead {
            guard.remove(&id);
        }
        log::debug!("broadcast event '{}' to {delivered} client(s)", event.event);
        delivered
    }

    /// Ask the owner thread to leave (it drops the backend, running its
    /// teardown) and join it when it responds in time.
    ///
    /// A long-running order may still be in flight; we then detach the thread
    /// instead of blocking the exit (`BACKEND_SHUTDOWN_WAIT`).
    fn stop_backend(&self) {
        let (ack_tx, ack_rx) = mpsc::channel::<()>();
        let mut acknowledged = false;
        if self.commands.send(Command::Shutdown { ack: ack_tx }).is_err() {
            log::debug!("backend owner thread already gone");
        } else {
            match ack_rx.recv_timeout(BACKEND_SHUTDOWN_WAIT) {
                Ok(()) => acknowledged = true,
                Err(_) => log::warn!(
                    "backend owner thread still busy after {BACKEND_SHUTDOWN_WAIT:?} — detaching it"
                ),
            }
        }
        let mut guard = lock(&self.backend_thread);
        if let Some(handle) = guard.take() {
            if acknowledged {
                if handle.join().is_err() {
                    log::debug!("backend owner thread panicked on the way out");
                }
            }
            // Not acknowledged: dropping the handle detaches the thread; the
            // process is exiting and the backend's teardown runs with it.
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop_backend();
        // A clean shutdown must not leave a stale socket behind (a stale file
        // would also be recovered on the next bind, but removing it keeps
        // `ls /tmp` honest and `holafctl` from dialling a dead socket).
        match std::fs::remove_file(&self.socket_path) {
            Ok(()) => log::debug!("removed socket {}", self.socket_path.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => log::warn!(
                "cannot remove socket {}: {e}",
                self.socket_path.display()
            ),
        }
    }
}

/// Recover a poisoned mutex instead of panicking the caller.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// One connection: register a writer, read NDJSON lines, order, reply.
fn handle_connection(
    stream: UnixStream,
    commands: mpsc::Sender<Command>,
    clients: Arc<Mutex<HashMap<u64, mpsc::Sender<String>>>>,
    id: u64,
) {
    // Writer thread owns a clone of the stream and drains the outbound
    // channel, so responses/events never interleave mid-line.
    let writer_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("client {id}: cannot clone stream: {e}");
            return;
        }
    };
    let (tx, rx) = mpsc::channel::<String>();
    let writer = thread::spawn(move || {
        let mut writer = writer_stream;
        for line in rx {
            if let Err(e) = writer.write_all(line.as_bytes()) {
                log::debug!("client {id}: write failed: {e}");
                break;
            }
        }
    });

    lock(&clients).insert(id, tx.clone());

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let response = match decode_line::<Request>(&line) {
            Ok(request) => send_order(&commands, request),
            Err(e) => Response::err(0, "parse_error", format!("invalid JSON: {e}")),
        };
        if tx.send(encode(&response)).is_err() {
            break;
        }
    }

    // Reader done: unregister and close the writer channel.
    lock(&clients).remove(&id);
    drop(tx);
    let _ = writer.join();
    log::info!("client {id} disconnected");
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Turn one [`Request`] into a [`Response`] by validating the params and
/// forwarding to the backend.  Runs on the owner thread only.
fn dispatch(backend: &mut dyn Backend, request: Request) -> Response {
    let Request { id, method, params } = request;
    match method.as_str() {
        METHOD_APPLY_SNAPSHOT => {
            let p = match parse_params::<ApplySnapshotParams>(id, params) {
                Ok(p) => p,
                Err(r) => return r,
            };
            let strips: Vec<Strip> = p.strips.iter().map(strip_from_dict).collect();
            respond(id, backend.apply_snapshot(&strips))
        }
        METHOD_STOP => match backend.stop() {
            Ok(()) => Response::ok(id, json!({ "ok": true })),
            Err(e) => respond_err(id, e),
        },
        METHOD_STATUS => respond(id, backend.status()),
        METHOD_STRIP_SET_VOLUME => {
            let p = match parse_params::<SetVolumeParams>(id, params) {
                Ok(p) => p,
                Err(r) => return r,
            };
            respond(id, backend.set_volume(&p.uid, p.value))
        }
        METHOD_STRIP_SET_MUTE => {
            let p = match parse_params::<SetMuteParams>(id, params) {
                Ok(p) => p,
                Err(r) => return r,
            };
            respond(id, backend.set_mute(&p.uid, p.muted))
        }
        METHOD_STRIP_SET_MONO => {
            let p = match parse_params::<SetMonoParams>(id, params) {
                Ok(p) => p,
                Err(r) => return r,
            };
            respond(id, backend.set_mono(&p.uid, p.mono))
        }
        METHOD_FX_SET_ACTIVE => {
            let p = match parse_params::<FxSetActiveParams>(id, params) {
                Ok(p) => p,
                Err(r) => return r,
            };
            respond(id, backend.fx_set_active(&p.uid, p.effect, p.active))
        }
        METHOD_FX_SET_PARAMS => {
            let p = match parse_params::<FxSetParamsParams>(id, params) {
                Ok(p) => p,
                Err(r) => return r,
            };
            respond(id, backend.fx_set_params(&p.uid, p.effect, &p.params))
        }
        METHOD_ROUTING_SET => {
            let p = match parse_params::<RoutingSetParams>(id, params) {
                Ok(p) => p,
                Err(r) => return r,
            };
            respond(id, backend.routing_set(&p.source_uid, &p.target_uid, p.active))
        }
        METHOD_ROUTING_LIST => respond(id, backend.routing_list()),
        METHOD_SYSTEM_SET_DEFAULT_SINK => {
            let p = match parse_params::<DefaultSinkParams>(id, params) {
                Ok(p) => p,
                Err(r) => return r,
            };
            respond(id, backend.set_default_sink(&p.uid))
        }
        METHOD_DEVICE_LIST => respond(id, backend.device_list()),
        METHOD_APP_LIST => respond(id, backend.app_list()),
        METHOD_METER_SOURCES => respond(id, backend.meter_sources()),
        METHOD_APP_MOVE => {
            let p = match parse_params::<AppMoveParams>(id, params) {
                Ok(p) => p,
                Err(r) => return r,
            };
            respond(id, backend.app_move(&p.app_id, &p.sink_name))
        }
        _ => Response::err(
            id,
            "unknown_method",
            format!("method '{method}' is not implemented"),
        ),
    }
}

/// Deserialize `params` for request `id`, mapping failures to an IPC
/// `invalid_params` error.
///
/// `Err` deliberately carries the full [`Response`] (which already embeds the
/// request id) so every dispatch arm can `return` it directly; the resulting
/// enum size is acceptable for a synchronous protocol handler.
#[allow(clippy::result_large_err)]
fn parse_params<T: DeserializeOwned>(id: u64, params: Value) -> Result<T, Response> {
    serde_json::from_value(params)
        .map_err(|e| Response::err(id, "invalid_params", format!("invalid params: {e}")))
}

fn respond(id: u64, result: Result<Value, BackendError>) -> Response {
    match result {
        Ok(value) => Response::ok(id, value),
        Err(e) => respond_err(id, e),
    }
}

fn respond_err(id: u64, err: BackendError) -> Response {
    Response::err(id, err.code, err.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeBackend;
    use holaf_core::model::{Strip, StripType};
    use holaf_ipc::METHOD_STRIP_SET_MUTE;

    fn test_backend() -> FakeBackend {
        let mut fake = FakeBackend::new();
        fake.apply_snapshot(&[Strip::with_uid("in-1", "Micro", StripType::Input)])
            .unwrap();
        fake
    }

    fn dispatch_one(backend: &mut dyn Backend, method: &str, params: Value) -> Response {
        dispatch(backend, Request::with_params(1, method, params))
    }

    #[test]
    fn dispatch_unknown_method() {
        let mut b = test_backend();
        let resp = dispatch_one(&mut b, "engine.frobnicate", json!({}));
        assert!(!resp.ok);
        let err = resp.error.unwrap();
        assert_eq!(err.code, "unknown_method");
        assert_eq!(resp.id, 1);
    }

    #[test]
    fn dispatch_invalid_params_reports_code() {
        let mut b = test_backend();
        // Missing `value` for strip.set_volume.
        let resp = dispatch_one(&mut b, METHOD_STRIP_SET_VOLUME, json!({"uid": "in-1"}));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "invalid_params");
    }

    #[test]
    fn dispatch_apply_snapshot_migrates_legacy_strips() {
        let mut b = FakeBackend::new();
        // Legacy config fragment: boolean gate + old short EQ band + Model.
        let raw = json!([{
            "uid": "legacy-1",
            "label": "Old",
            "kind": "input",
            "effects": {
                "gate": true,
                "eq": { "active": true, "params": { "50Hz": -6.0 } },
                "noise_cancel": { "active": false, "params": { "Model": 0.0 } }
            }
        }]);
        let resp = dispatch_one(&mut b, METHOD_APPLY_SNAPSHOT, json!({ "strips": raw }));
        assert!(resp.ok, "apply_snapshot failed: {resp:?}");
        assert_eq!(resp.result.unwrap()["applied"], 1);

        let strip = b.strip("legacy-1").unwrap();
        assert!(strip.effects[&EffectKey::Gate].active);
        assert_eq!(
            strip.effects[&EffectKey::Eq].params["50Hz gain (low shelving)"],
            -6.0
        );
        assert!(!strip.effects[&EffectKey::NoiseCancel].params.contains_key("Model"));
        // Migrations filled all remaining defaults.
        assert_eq!(strip.effects[&EffectKey::Eq].params.len(), 15);
    }

    #[test]
    fn dispatch_volume_unknown_uid_maps_backend_error() {
        let mut b = test_backend();
        let resp = dispatch_one(&mut b, METHOD_STRIP_SET_VOLUME, json!({"uid":"x","value":0.5}));
        assert!(!resp.ok);
        let err = resp.error.unwrap();
        assert_eq!(err.code, "unknown_strip");
        assert_eq!(resp.id, 1);
    }

    #[test]
    fn dispatch_effect_key_invalid_value() {
        let mut b = test_backend();
        let resp = dispatch_one(
            &mut b,
            METHOD_FX_SET_ACTIVE,
            json!({"uid": "in-1", "effect": "reverb", "active": true}),
        );
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "invalid_params");
    }

    #[test]
    fn dispatch_end_to_end_sequence() {
        let mut b = test_backend();
        let vol = dispatch_one(&mut b, METHOD_STRIP_SET_VOLUME, json!({"uid":"in-1","value":0.5}));
        assert!(vol.ok);
        assert_eq!(vol.result.unwrap()["value"], 0.5);

        let mute = dispatch_one(&mut b, METHOD_STRIP_SET_MUTE, json!({"uid":"in-1","muted":true}));
        assert!(mute.ok);

        let fx = dispatch_one(
            &mut b,
            METHOD_FX_SET_ACTIVE,
            json!({"uid": "in-1", "effect": "compressor", "active": true}),
        );
        assert!(fx.ok);
        assert_eq!(fx.result.unwrap()["active"], true);

        let status = dispatch_one(&mut b, METHOD_STATUS, json!({}));
        let result = status.result.unwrap();
        let arr = result["strips"].as_array().unwrap();
        let s = &arr[0];
        assert_eq!(s["uid"], "in-1");
        assert_eq!(s["volume"], 0.5);
        assert_eq!(s["mute"], true);
        assert_eq!(s["effects"]["compressor"]["active"], true);
    }

    #[test]
    fn dispatch_meter_sources_maps_every_strip() {
        let mut b = FakeBackend::new();
        let mut mic = Strip::with_uid("in-1", "Micro", StripType::Input);
        mic.device_name = Some("alsa_input.usb-mic.analog-stereo".into());
        let desktop = Strip::with_uid("in-2", "Desktop", StripType::Input);
        let mut out = Strip::with_uid("out-1", "Casque", StripType::Output);
        out.device_name = Some("alsa_output.usb-dac.analog-stereo".into());
        b.apply_snapshot(&[mic, desktop, out]).unwrap();

        let resp = dispatch_one(&mut b, METHOD_METER_SOURCES, json!({}));
        assert!(resp.ok, "meter.sources failed: {resp:?}");
        let result = resp.result.unwrap();
        assert_eq!(result["running"], true);
        let sources = &result["sources"];
        // Physical input → its device source; virtual input → null-sink
        // monitor; physical output → that sink's monitor.
        assert_eq!(sources["in-1"], "alsa_input.usb-mic.analog-stereo");
        assert_eq!(sources["in-2"], "Holaf_Strip_in-2.monitor");
        assert_eq!(sources["out-1"], "alsa_output.usb-dac.analog-stereo.monitor");

        // Toggling an effect does not move the metered source: a virtual
        // strip is always metered on its null-sink monitor (never on the FX
        // chain's passive Audio/Source, which is unroutable/unreadable).
        b.fx_set_active("in-2", EffectKey::Eq, true).unwrap();
        let resp = dispatch_one(&mut b, METHOD_METER_SOURCES, json!({}));
        let sources = resp.result.unwrap()["sources"].clone();
        assert_eq!(sources["in-2"], "Holaf_Strip_in-2.monitor");
    }

    /// A backend panic must be turned into an `internal_error` response, not
    /// kill the owner thread (nor the daemon).
    #[test]
    fn execute_turns_backend_panic_into_internal_error() {
        struct Panicky;
        impl Backend for Panicky {
            fn apply_snapshot(&mut self, _strips: &[Strip]) -> Result<Value, BackendError> {
                panic!("boom in apply_snapshot");
            }
            fn stop(&mut self) -> Result<(), BackendError> {
                Ok(())
            }
            fn status(&mut self) -> Result<Value, BackendError> {
                Ok(json!({ "alive": true }))
            }
            fn set_volume(&mut self, _uid: &str, _value: f32) -> Result<Value, BackendError> {
                unreachable!()
            }
            fn set_mute(&mut self, _uid: &str, _muted: bool) -> Result<Value, BackendError> {
                unreachable!()
            }
            fn set_mono(&mut self, _uid: &str, _mono: bool) -> Result<Value, BackendError> {
                unreachable!()
            }
            fn fx_set_active(
                &mut self,
                _uid: &str,
                _effect: EffectKey,
                _active: bool,
            ) -> Result<Value, BackendError> {
                unreachable!()
            }
            fn fx_set_params(
                &mut self,
                _uid: &str,
                _effect: EffectKey,
                _params: &indexmap::IndexMap<String, f32>,
            ) -> Result<Value, BackendError> {
                unreachable!()
            }
            fn routing_set(
                &mut self,
                _source_uid: &str,
                _target_uid: &str,
                _active: bool,
            ) -> Result<Value, BackendError> {
                unreachable!()
            }
            fn routing_list(&mut self) -> Result<Value, BackendError> {
                Ok(json!({ "routes": [] }))
            }
            fn set_default_sink(&mut self, _uid: &str) -> Result<Value, BackendError> {
                unreachable!()
            }
            fn device_list(&mut self) -> Result<Value, BackendError> {
                Ok(json!({ "devices": [] }))
            }
            fn app_list(&mut self) -> Result<Value, BackendError> {
                Ok(json!({ "apps": [] }))
            }
            fn app_move(&mut self, _app_id: &str, _sink_name: &str) -> Result<Value, BackendError> {
                unreachable!()
            }
            fn meter_sources(&mut self) -> Result<Value, BackendError> {
                Ok(json!({ "running": true, "sources": {} }))
            }
        }

        let mut backend = Panicky;
        let resp = execute(
            &mut backend,
            Request::with_params(7, METHOD_APPLY_SNAPSHOT, json!({ "strips": [] })),
        );
        assert!(!resp.ok);
        assert_eq!(resp.id, 7);
        let err = resp.error.unwrap();
        assert_eq!(err.code, "internal_error");
        assert!(err.message.contains("boom in apply_snapshot"), "{err:?}");

        // The very same backend still answers afterwards (thread survived).
        let resp = execute(&mut backend, Request::with_params(8, METHOD_STATUS, json!({})));
        assert!(resp.ok);
        assert_eq!(resp.result.unwrap()["alive"], true);
    }

    /// A dead owner thread (closed command channel) must yield a clear error
    /// immediately, never a block.
    #[test]
    fn send_order_reports_backend_unavailable_when_owner_is_gone() {
        let (commands, commands_rx) = mpsc::channel::<Command>();
        drop(commands_rx);
        let resp = send_order(&commands, Request::new(3, METHOD_STATUS));
        assert!(!resp.ok);
        assert_eq!(resp.id, 3);
        assert_eq!(resp.error.unwrap().code, "backend_unavailable");
    }

    /// A client that gives up before the reply must not kill the owner thread.
    #[test]
    fn backend_loop_survives_a_client_that_gave_up() {
        let (commands, commands_rx) = mpsc::channel::<Command>();
        let handle = thread::spawn(move || backend_loop(commands_rx, Box::new(FakeBackend::new())));
        {
            let (reply_tx, reply_rx) = mpsc::channel::<Response>();
            commands
                .send(Command::Request {
                    request: Request::new(1, METHOD_STATUS),
                    reply: reply_tx,
                })
                .unwrap();
            drop(reply_rx); // client gone
        }
        // The loop is still alive: a second order is served.
        let resp = send_order(&commands, Request::new(2, METHOD_STATUS));
        assert!(resp.ok, "owner thread died after a dropped reply: {resp:?}");

        let (ack_tx, ack_rx) = mpsc::channel::<()>();
        commands.send(Command::Shutdown { ack: ack_tx }).unwrap();
        ack_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("owner thread acknowledges shutdown");
        drop(commands);
        handle.join().expect("owner thread exits cleanly");
    }
}
