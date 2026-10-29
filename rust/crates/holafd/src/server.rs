//! The NDJSON Unix-socket server: accept connections, read one request per
//! line, dispatch to the shared [`Backend`] under a mutex, write one response
//! per request.  Requests are processed sequentially (the `Mutex` mirrors the
//! Python engine's `RLock`).  A per-connection writer thread guarantees
//! responses and broadcast events never interleave mid-line on the socket.
//!
//! No async runtime: `std` threads + channels only (rewrite spec §4).

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use holaf_core::migration::strip_from_dict;
use holaf_core::model::{EffectKey, Strip};

use holaf_ipc::{
    decode_line, encode, Event, Request, Response, METHOD_APPLY_SNAPSHOT, METHOD_APP_LIST,
    METHOD_APP_MOVE, METHOD_DEVICE_LIST, METHOD_FX_SET_ACTIVE, METHOD_FX_SET_PARAMS,
    METHOD_ROUTING_LIST, METHOD_ROUTING_SET, METHOD_STATUS, METHOD_STOP, METHOD_STRIP_SET_MONO,
    METHOD_STRIP_SET_MUTE, METHOD_STRIP_SET_VOLUME, METHOD_SYSTEM_SET_DEFAULT_SINK,
};

use crate::backend::{Backend, BackendError};

/// Poll interval of the (non-blocking) accept loop — how soon a shutdown
/// request is honoured.
const ACCEPT_POLL: Duration = Duration::from_millis(15);

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
// Server
// ---------------------------------------------------------------------------

/// A bound Unix-socket NDJSON server dispatching to one shared [`Backend`].
pub struct Server {
    listener: UnixListener,
    socket_path: PathBuf,
    /// The single engine instance.  All requests across all connections are
    /// serialised on this mutex.
    backend: Arc<Mutex<Box<dyn Backend>>>,
    /// Connected clients keyed by their connection id, used for broadcast
    /// events.  Each value is a channel towards that connection's writer
    /// thread.
    clients: Arc<Mutex<HashMap<u64, mpsc::Sender<String>>>>,
    next_client: AtomicU64,
}

impl Server {
    /// Bind the listener on `path`.  A stale socket file left behind by a
    /// previous crashed daemon is removed first; if the path is live the bind
    /// fails with `AddrInUse`.
    pub fn bind(path: impl AsRef<Path>, backend: impl Backend + 'static) -> io::Result<Server> {
        Self::bind_boxed(path, Box::new(backend))
    }

    /// Bind with an already-boxed backend (e.g. `UnimplementedBackend`).
    pub fn bind_boxed(path: impl AsRef<Path>, backend: Box<dyn Backend>) -> io::Result<Server> {
        let path = path.as_ref().to_path_buf();
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                // Remove a stale socket and retry once.
                log::warn!("socket {} already exists — removing stale file", path.display());
                std::fs::remove_file(&path)?;
                UnixListener::bind(&path)?
            }
            Err(e) => return Err(e),
        };
        Ok(Server {
            listener,
            socket_path: path,
            backend: Arc::new(Mutex::new(backend)),
            clients: Arc::new(Mutex::new(HashMap::new())),
            next_client: AtomicU64::new(1),
        })
    }

    /// The socket path this server is listening on.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Shared handle to the backend (for tests / direct dispatch).
    pub fn backend(&self) -> Arc<Mutex<Box<dyn Backend>>> {
        Arc::clone(&self.backend)
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
                return Ok(());
            }
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    let id = self.next_client.fetch_add(1, Ordering::Relaxed);
                    log::info!("client {id} connected");
                    let backend = Arc::clone(&self.backend);
                    let clients = Arc::clone(&self.clients);
                    thread::spawn(move || handle_connection(stream, backend, clients, id));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(ACCEPT_POLL);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Handle a single request synchronously (no socket involved).  Used by
    /// the daemon and by in-process tests.
    pub fn handle_request(&self, request: Request) -> Response {
        let mut guard = lock_backend(&self.backend);
        dispatch(&mut **guard, request)
    }

    /// Broadcast an event to every connected client (best effort).
    ///
    /// The core daemon calls this on state changes; the PipeWire wiring that
    /// produces those events arrives at J2.  Returns the number of clients
    /// that actually received the event.
    pub fn notify(&self, event: &Event) -> usize {
        let line = encode(event);
        let mut dead = Vec::new();
        let mut delivered = 0usize;
        let mut guard = match self.clients.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
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
}

/// Recover a poisoned mutex instead of panicking the connection thread.
fn lock_backend(backend: &Mutex<Box<dyn Backend>>) -> std::sync::MutexGuard<'_, Box<dyn Backend>> {
    match backend.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// One connection: register a writer, read NDJSON lines, dispatch, reply.
fn handle_connection(
    stream: UnixStream,
    backend: Arc<Mutex<Box<dyn Backend>>>,
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

    match clients.lock() {
        Ok(mut g) => g.insert(id, tx.clone()),
        Err(p) => p.into_inner().insert(id, tx.clone()),
    };

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
            Ok(request) => {
                let mut guard = lock_backend(&backend);
                dispatch(&mut **guard, request)
            }
            Err(e) => Response::err(0, "parse_error", format!("invalid JSON: {e}")),
        };
        if tx.send(encode(&response)).is_err() {
            break;
        }
    }

    // Reader done: unregister and close the writer channel.
    if let Ok(mut g) = clients.lock() {
        g.remove(&id);
    }
    drop(tx);
    let _ = writer.join();
    log::info!("client {id} disconnected");
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Turn one [`Request`] into a [`Response`] by locking the engine state,
/// validating the params and forwarding to the backend.
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
}
