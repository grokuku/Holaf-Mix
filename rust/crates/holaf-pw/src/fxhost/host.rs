//! The FX host process: the *only* job of this process is to host
//! `libpipewire-module-filter-chain` modules and keep its PipeWire mainloop
//! running.
//!
//! # Why a separate process at all
//!
//! The Python engine hosts its chains in a dedicated `pw-cli` and has never
//! chopped.  The Rust daemon used to host them **in-process**, on the same
//! thread that also serves the IPC socket, reconciles the graph, repairs
//! pending work and logs.  A `module-filter-chain` lives *inside the client
//! process*: its two streams are made schedulable by that process's mainloop
//! and its `process` callbacks run on that process's data loop, so everything
//! else the process does is potential interference (a 1.33 ms quantum at 48 kHz
//! for a 64-sample period leaves very little headroom on a timing-sensitive
//! device such as a USB interface).
//!
//! This process reproduces the proven shape exactly:
//!
//! ```text
//!   pw_context (client config, like pw-cli)
//!        │
//!        ├── filter-chain modules (one per strip with FX)
//!        └── stdin as a PipeWire *IO source*  ← NDJSON commands
//!                 │
//!        pw_main_loop_run()   ← one blocking, purely event-driven loop
//! ```
//!
//! There is no timeout, no polling, no order queue to drain: a command wakes
//! the loop through the same `poll()` that services the streams, and the loop
//! blocks (blocking `poll()`, exactly like `pw-cli`'s `pw_main_loop_run`) the
//! rest of the time.
//!
//! # Real-time priority
//!
//! The host is a plain PipeWire client: [`ContextRc::new`] with `None`
//! properties loads the same default client configuration `pw-cli` loads, so
//! the data-loop threads are promoted by `libpipewire-module-rt` if and only
//! if the machine's client configuration asks for it (rtkit /
//! `client-rt.conf`).  Nothing extra is needed *and nothing extra is done*:
//! the host never calls `sched_setscheduler` itself, which keeps its RT
//! behaviour byte-identical to the `pw-cli` hosting path.  It inherits
//! `PIPEWIRE_*` from `holafd`, so a user who wants to force a specific config
//! can export `PIPEWIRE_CONFIG_NAME` before starting the app.
//!
//! # Lifecycle
//!
//! Spawned by `holafd` on the first chain ([`super::client::FxHostClient`]) as
//! `<holafd> --fx-host` with `stdin`/`stdout` as pipes.  The host exits on:
//!
//! * `{"cmd":"stop"}` — clean `holafd` teardown;
//! * **EOF on stdin** — i.e. when `holafd` dies, however it dies (`SIGKILL`
//!   included).  This is the parent-death signal: the kernel closes the pipe
//!   when the parent goes away, so no `prctl(PR_SET_PDEATHSIG)` (and no extra
//!   dependency) is needed, and it cannot be lost the way a signal can be
//!   (note a `PR_SET_PDEATHSIG` handler is per-*thread* — the thread that
//!   forked — which makes it fragile for a multi-threaded parent);
//! * a failed write to stdout (the parent is gone).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::time::Instant;

use indexmap::IndexMap;
use pipewire as pw;
use pipewire::properties::PropertiesBox;
use pipewire::registry::{GlobalObject, Listener, RegistryRc};
use serde_json::{json, Map};

use holaf_core::backend::BackendError;

use super::proto::{parse_request, FxCommand, FxRequest, FxReply};
use crate::engine::push_params_pod;
use crate::sys::{self, LocalModule};

/// Owned registry entry (id + props) for the objects we may need to bind.
type OwnedGlobal = GlobalObject<PropertiesBox>;

/// stdin as a PipeWire IO source.
///
/// The loop owns no fd here (`add_io` is told not to close it): stdin belongs
/// to the process.  Reading is a plain `read(2)` through `std::io::stdin()`,
/// which never blocks more than the one syscall the readiness notification
/// announced.
struct StdinIo;

impl AsRawFd for StdinIo {
    fn as_raw_fd(&self) -> RawFd {
        // The parent hands us the command pipe as fd 0 (`Stdio::piped()`).
        0
    }
}

/// Everything the FX host needs to execute commands.  Lives on the loop thread
/// only (`Rc`/`RefCell`, no locks).
struct HostState {
    context: pw::context::ContextRc,
    /// Owning registry proxy; keeps the connection (and the core it holds)
    /// alive, and is what we bind node proxies from.
    registry: RegistryRc,
    /// Live `node.name` → global, kept up to date by the registry listener.
    by_name: Rc<RefCell<HashMap<String, OwnedGlobal>>>,
    /// `global id` → `node.name` (for removal events).
    by_id: Rc<RefCell<HashMap<u32, String>>>,
    /// The chains we host, keyed by strip uid.
    modules: IndexMap<String, LocalModule>,
    /// `Node` proxies bound for `set_param` (kept alive so a hot-reload does
    /// not have to re-bind, and so the queued set_param is flushed before the
    /// proxy can go away).
    node_proxies: IndexMap<String, pw::node::Node>,
    /// Set when the host must leave its loop.
    quit: bool,
    /// Accumulator for a partially received command line.
    inbuf: String,
    started: Instant,
    listener: Option<Listener>,
}

impl HostState {
    fn new(context: pw::context::ContextRc, registry: RegistryRc) -> HostState {
        HostState {
            context,
            registry,
            by_name: Rc::new(RefCell::new(HashMap::new())),
            by_id: Rc::new(RefCell::new(HashMap::new())),
            modules: IndexMap::new(),
            node_proxies: IndexMap::new(),
            quit: false,
            inbuf: String::new(),
            started: Instant::now(),
            listener: None,
        }
    }

    /// Track every node the daemon publishes for us (no round-trip, no snapshot
    /// request: pure event bookkeeping — the loop stays event-driven).
    fn watch_registry(&mut self) {
        let by_name = self.by_name.clone();
        let by_id = self.by_id.clone();
        let listener = self
            .registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(name) = global
                    .props
                    .as_ref()
                    .and_then(|p| p.get("node.name"))
                    .map(str::to_string)
                else {
                    return;
                };
                by_id.borrow_mut().insert(global.id, name.clone());
                by_name.borrow_mut().insert(name, global.to_owned());
            })
            .global_remove({
                let by_id = self.by_id.clone();
                let by_name = self.by_name.clone();
                move |id| {
                    if let Some(name) = by_id.borrow_mut().remove(&id) {
                        by_name.borrow_mut().remove(&name);
                    }
                }
            })
            .register();
        self.listener = Some(listener);
    }

    /// Bind (once, then cached) the `input.Holaf_FX_<uid>` node — the one that
    /// carries the chain's flat `Props`.
    ///
    /// Binding is a *local* operation on the registry proxy: it needs no
    /// round-trip and no `sync`, which is what keeps this process's loop purely
    /// event-driven.  The node may simply not be published yet in the very
    /// millisecond after `load_chain`, hence the explicit `chain_gone`.
    fn ensure_input_node(&mut self, uid: &str) -> Result<(), BackendError> {
        if self.node_proxies.contains_key(uid) {
            return Ok(());
        }
        let name = holaf_core::graph::fx_input_node_name(uid);
        let global = {
            let by_name = self.by_name.borrow();
            match by_name.get(&name) {
                Some(g) => g.to_owned(),
                None => {
                    return Err(BackendError::new(
                        "chain_gone",
                        format!("'{name}' is not in this host's registry view yet"),
                    ))
                }
            }
        };
        match self.registry.bind::<pw::node::Node, _>(&global) {
            Ok(node) => {
                self.node_proxies.insert(uid.to_string(), node);
                Ok(())
            }
            Err(e) => Err(BackendError::new(
                "chain_gone",
                format!("cannot bind '{name}': {e}"),
            )),
        }
    }

    fn unload(&mut self, uid: &str) -> bool {
        self.node_proxies.shift_remove(uid);
        let Some(module) = self.modules.shift_remove(uid) else {
            return false;
        };
        // SAFETY: we are outside any module callback (this runs from the stdin
        // IO source), so a deferred destroy is safe and lets the context finish
        // the teardown on its next iteration.
        unsafe { module.schedule_destroy() };
        true
    }

    /// Execute one command.  Never panics, never blocks on the network: every
    /// operation here is either local (`pw_context_load_module`, proxy method)
    /// or a bookkeeping update.
    fn dispatch(&mut self, req: FxRequest) -> FxReply {
        let id = req.id;
        match req.command {
            FxCommand::LoadChain {
                uid,
                config,
                anchor_node_id,
            } => {
                if self.modules.contains_key(&uid) {
                    // Idempotent: `holafd` may retry a load across a respawn.
                    self.unload(&uid);
                }
                if let Some(anchor) = anchor_node_id {
                    if !self.by_id.borrow().contains_key(&anchor) {
                        log::debug!(
                            "fx-host: anchor node {anchor} not in our registry view yet \
                             (chain '{uid}') — loading anyway"
                        );
                    }
                }
                // SAFETY: the context outlives every module we track (this
                // process only ever tears them down from here or at exit).
                let loaded = unsafe {
                    sys::load_module(
                        &self.context,
                        "libpipewire-module-filter-chain",
                        Some(&config),
                        None,
                    )
                };
                match loaded {
                    Ok(module) => {
                        self.modules.insert(uid.clone(), module);
                        let (input_node, output_node) = chain_node_names(&config);
                        log::info!(
                            "fx-host: loaded chain '{uid}' ({})",
                            input_node.as_deref().unwrap_or("?")
                        );
                        let mut data = Map::new();
                        data.insert(
                            "chain".to_string(),
                            json!({
                                "uid": uid,
                                "input_node": input_node,
                                "output_node": output_node,
                            }),
                        );
                        FxReply::ok(id, data)
                    }
                    Err(e) => {
                        log::error!("fx-host: loading chain '{uid}' failed: {e}");
                        FxReply::err(id, "pw_load_failed", e.to_string())
                    }
                }
            }
            FxCommand::UnloadChain { uid } => {
                let removed = self.unload(&uid);
                let mut data = Map::new();
                data.insert("removed".to_string(), json!(removed));
                FxReply::ok(id, data)
            }
            FxCommand::UnloadAll => {
                let uids: Vec<String> = self.modules.keys().cloned().collect();
                for uid in &uids {
                    self.unload(uid);
                }
                let mut data = Map::new();
                data.insert("unloaded".to_string(), json!(uids));
                FxReply::ok(id, data)
            }
            FxCommand::SetParams { uid, props } => {
                if !self.modules.contains_key(&uid) {
                    return FxReply::err(
                        id,
                        "unknown_chain",
                        format!("no chain '{uid}' hosted here"),
                    );
                }
                if let Err(e) = self.ensure_input_node(&uid) {
                    return FxReply::err(id, &e.code, e.message);
                }
                let Some(node) = self.node_proxies.get(&uid) else {
                    return FxReply::err(
                        id,
                        "chain_gone",
                        format!("input.Holaf_FX_{uid} has no proxy"),
                    );
                };
                let pairs: Vec<(String, f32)> = props.into_iter().collect();
                match push_params_pod(node, &pairs) {
                    // The proxy marshals immediately; the message is flushed by
                    // the connection on the next loop iteration, so there is
                    // nothing to wait for here.
                    Ok(()) => FxReply::ok(id, Map::new()),
                    Err(e) => FxReply::err(id, &e.code, e.message),
                }
            }
            FxCommand::Ping => {
                let mut data = Map::new();
                data.insert("pong".to_string(), json!(true));
                data.insert(
                    "uptime_ms".to_string(),
                    json!(self.started.elapsed().as_millis() as u64),
                );
                FxReply::ok(id, data)
            }
            FxCommand::Status => {
                let mut data = Map::new();
                data.insert(
                    "chains".to_string(),
                    json!(self.modules.keys().collect::<Vec<_>>()),
                );
                data.insert("pid".to_string(), json!(std::process::id()));
                data.insert(
                    "uptime_ms".to_string(),
                    json!(self.started.elapsed().as_millis() as u64),
                );
                FxReply::ok(id, data)
            }
            FxCommand::Stop => {
                let uids: Vec<String> = self.modules.keys().cloned().collect();
                for uid in &uids {
                    self.unload(uid);
                }
                self.quit = true;
                FxReply::done(id)
            }
        }
    }
}

/// Node names `module-filter-chain` derives from its `node.name`:
/// `input.<node.name>` and `output.<node.name>`.
///
/// Read back from the module arguments instead of being recomputed from the
/// uid, so the answer is what the module *really* used (the parent has a single
/// source of truth for the naming, and this is the verification side of it).
fn chain_node_names(config: &str) -> (Option<String>, Option<String>) {
    let Some(name) = config_node_name(config) else {
        return (None, None);
    };
    (Some(format!("input.{name}")), Some(format!("output.{name}")))
}

/// Extract `node.name = "<value>"` from a filter-chain module argument string.
fn config_node_name(config: &str) -> Option<String> {
    let key = "node.name";
    let start = config.find(key)? + key.len();
    let rest = &config[start..];
    let open = rest.find('"')? + 1;
    let rest = &rest[open..];
    let close = rest.find('"')?;
    let value = &rest[..close];
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Read whatever is available on stdin and execute every complete line.
///
/// Returns `false` when the parent is gone (EOF) or when a reply could not be
/// written, i.e. when the host must stop.
fn service_stdin(state: &Rc<RefCell<HostState>>) -> bool {
    let mut chunk = [0u8; 8192];
    let n = match std::io::stdin().read(&mut chunk) {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return true,
        Err(e) => {
            log::error!("fx-host: stdin read failed: {e}");
            return false;
        }
    };
    if n == 0 {
        // EOF: `holafd` exited (or closed the pipe).  Nothing left to serve.
        log::info!("fx-host: stdin closed — exiting");
        return false;
    }
    let mut out = std::io::stdout();
    state
        .borrow_mut()
        .inbuf
        .push_str(&String::from_utf8_lossy(&chunk[..n]));
    loop {
        let pos = match state.borrow().inbuf.find('\n') {
            Some(p) => p,
            None => break,
        };
        let line: String = state.borrow_mut().inbuf.drain(..=pos).collect();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let reply = match parse_request(line) {
            Ok(req) => state.borrow_mut().dispatch(req),
            Err(reply) => reply,
        };
        let quit = state.borrow().quit;
        if out
            .write_all(reply.to_line().as_bytes())
            .and_then(|()| out.flush())
            .is_err()
        {
            log::warn!("fx-host: cannot write reply — parent is gone");
            return false;
        }
        if quit {
            return false;
        }
    }
    true
}

/// Entry point of `holafd --fx-host`.  Returns the process exit code.
///
/// Blocks until the parent closes the command pipe or sends `stop`.
pub fn run_host() -> i32 {
    pw::init();
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => {
            log::error!("fx-host: cannot create mainloop: {e}");
            return 1;
        }
    };
    // Same client configuration as `pw-cli`: one context, one connection.
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => {
            log::error!("fx-host: cannot create context: {e}");
            return 1;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => {
            log::error!("fx-host: cannot connect: {e}");
            return 1;
        }
    };
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            log::error!("fx-host: cannot get registry: {e}");
            return 1;
        }
    };

    let state = Rc::new(RefCell::new(HostState::new(context, registry)));
    state.borrow_mut().watch_registry();
    log::info!("fx-host: pid {} ready", std::process::id());

    // stdin becomes a PipeWire IO source: a command wakes the loop the very
    // same way an audio event does — no polling thread, no timeout, no queue to
    // drain on the hot path.
    let cb_state = state.clone();
    let quit_loop = mainloop.clone();
    let _io = mainloop
        .loop_()
        .add_io(StdinIo, pw::spa::support::system::IoFlags::IN, move |_io| {
            if !service_stdin(&cb_state) {
                quit_loop.quit();
            }
        });

    // `run()` == `pw_main_loop_run()`: a blocking, purely event-driven loop,
    // exactly what a dedicated `pw-cli` does for the Python engine.
    mainloop.run();
    log::info!("fx-host: mainloop stopped");
    // Drop the modules before the context goes away (destroying a module from
    // outside any callback is the safe form).
    for uid in state.borrow().modules.keys().cloned().collect::<Vec<_>>() {
        state.borrow_mut().unload(&uid);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_node_name_from_module_args() {
        let cfg = "{ node.name = \"Holaf_FX_main\" node.description = \"Holaf FX Main\" \
                    filter.graph = { nodes = [ ] } }";
        assert_eq!(config_node_name(cfg).as_deref(), Some("Holaf_FX_main"));
        let (i, o) = chain_node_names(cfg);
        assert_eq!(i.as_deref(), Some("input.Holaf_FX_main"));
        assert_eq!(o.as_deref(), Some("output.Holaf_FX_main"));
    }

    #[test]
    fn missing_node_name_is_not_fatal() {
        assert_eq!(config_node_name("{ filter.graph = { } }"), None);
        assert_eq!(chain_node_names("{ }"), (None, None));
        assert_eq!(config_node_name("{ node.name = \"\" }"), None);
    }
}
