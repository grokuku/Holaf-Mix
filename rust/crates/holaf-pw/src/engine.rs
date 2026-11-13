//! The real PipeWire engine: the object graph + reconciler that implements
//! the [`Backend`] operations against a live daemon.
//!
//! # Threading
//!
//! PipeWire's mainloop/context/core are `!Send` (they use `Rc` internally), so
//! this whole engine lives on a **dedicated worker thread** and is driven by
//! the `PipeWireBackend` handle (`backend.rs`) through an mpsc channel.  Each
//! operation is processed synchronously: the worker runs its mainloop when a
//! round-trip / registry poll is needed **and continuously while it is idle**
//! ([`Engine::pump_mainloop`]), because the filter-chain modules loaded into
//! this context only get scheduled through mainloop traffic.  Every internal
//! wait goes through [`PwSession::sleep_pumping`] for the same reason: a plain
//! `thread::sleep` in the worker freezes a live chain.
//!
//! # Reconciliation model
//!
//! `apply_snapshot` is a full-diff sync of the *desired* strips, run in
//! **ordered phases** so that nothing is ever wired before it exists:
//!
//! ```text
//! P0  first apply only: unload the chains we host, sweep foreign debris
//! P1  teardown strips that disappeared (chain, null-sink, links)
//! P2  publish the desired strips  ← every later phase resolves them here
//! P3  strip nodes (null-sinks) — ALL nodes before ANY link
//! P4  FX chains (each anchored to a node that is really in the registry)
//! P5  routing matrix (links between strips)
//! P6  verification pass: intra-strip chain links + volume/mute (idempotent)
//! P7  default sink
//! ```
//!
//! **A phase never aborts the snapshot.**  Every step that cannot be completed
//! right now (device not plugged in yet, port pair not published, null-sink
//! that never showed up) is recorded as a [`PendingIssue`], reported by
//! `status` and retried by [`Engine::repair_pending`] with a bounded backoff —
//! so the graph heals by itself when the device appears, and a toggle
//! (`fx_set_active`) re-derives the routes as well.
//!
//! The Python engine never needed this: it drove every step through `pw-link`
//! subprocesses with retries *and* it re-applied its whole snapshot on any
//! failure.  Before the phases above existed, one missing device made
//! `apply_snapshot` return an error **in the middle of the graph build**: the
//! strips after it were never reconciled, routes were never created, and —
//! because `running` stayed `false` — the next apply swept the null-sinks it
//! had just created while the hosted chains kept their stale module handles
//! (`chain_gone` forever).  Measured symptom reported by users: with EQ active
//! at launch the output chops continuously, and toggling the EQ afterwards
//! neither fixes nor worsens it.
//!
//! Runtime FX toggles use hot `set-param` when the chain exists; once created
//! a chain is **kept** with neutral BYPASS values when the last effect is
//! disabled (the Python behaviour) — only a full `apply_snapshot` removes a
//! chain that no longer wants any effect.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use pipewire as pw;
use pipewire::properties::PropertiesBox;
use pipewire::proxy::ProxyT;
use pipewire::registry::GlobalObject;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Pod, Value};
use serde_json::{json, Value as JsonValue};

use holaf_core::backend::BackendError;
use holaf_core::graph::{self, ChainFlavour};
use holaf_core::model::{EffectKey, Strip};
use holaf_core::spa;
use holaf_core::fx::PluginSet;

use crate::chainhost::{ChainHost, FxHostMode};
use crate::live::{collect_view, LiveNode, LiveView, PortDir};
use crate::spa_config;

/// Polling knobs (same order of magnitude as the Python audio_engine).
const NODE_POLL_ATTEMPTS: u32 = 40;
const NODE_POLL_DELAY_MS: u64 = 50;
/// Longest single mainloop iteration used while "sleeping" inside the engine
/// (see [`PwSession::sleep_pumping`]).  Small enough that a hosted filter-chain
/// never misses a deadline, large enough not to spin.
const PUMP_SLICE: Duration = Duration::from_millis(2);
const LINK_RETRIES: u32 = 5;
/// How many registry polls [`Engine::link_nodes`] spends waiting for the two
/// *nodes* before giving up.
///
/// Deliberately short: every caller has already waited for its nodes to exist
/// (`create_null_sink`, `create_chain`, the chain-anchor check), so a node that
/// is still absent here is genuinely not there — typically an unplugged device.
/// The long retry budget ([`LINK_RETRIES`]) is spent on the *ports* instead,
/// which really do appear a few milliseconds after a node (a freshly created
/// chain stream publishes its ports late).  Keeping the node wait short matters
/// for the repair loop: a permanently missing device would otherwise burn half a
/// second per pending route on every pass.
const LINK_NODE_ATTEMPTS: u32 = 2;
/// Settle time before re-asserting a strip's volume (WirePlumber restores its
/// own saved per-node volume right after a node appears — see
/// [`Engine::sync_strip_volume`]).
const VOLUME_SETTLE_MS: u64 = 40;
const LINK_RETRY_DELAY_MS: u64 = 100;
const CHAIN_TEARDOWN_ROUNDTRIPS: u32 = 4;

/// Delay before the `attempts`-th (1-based) automatic repair pass.
///
/// Pure so the schedule is unit-tested without a daemon: the first retries are
/// fast (a USB card that enumerates right after the snapshot is picked up within
/// a fifth of a second), then the probe slows down to 5 s so a permanently
/// absent device costs nothing.
fn repair_backoff(attempts: u32) -> Duration {
    let idx = (attempts.saturating_sub(1) as usize).min(REPAIR_BACKOFF.len() - 1);
    REPAIR_BACKOFF[idx]
}

/// Automatic repair schedule for [`Engine::repair_pending`].
///
/// An incomplete graph (device not plugged in yet, port pair not published
/// yet) is retried in the background: fast at first — a USB card that
/// enumerates right after the snapshot is picked up within a fifth of a second
/// — then backing off to a slow probe so a permanently missing device costs
/// nothing.  Every pass is one registry snapshot per pending strip.
const REPAIR_BACKOFF: [Duration; 5] = [
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(2_000),
    Duration::from_millis(5_000),
];

/// A connected PipeWire session owned by the worker thread.
///
/// The `Rc`-flavoured wrappers (pipewire-rs 0.10) are the ones that can be
/// stored together: each one keeps its dependencies (`Loop` → `Context` →
/// `Core`) alive through internal reference counting, so `PwSession` stays a
/// plain struct instead of a self-referential one.
struct PwSession {
    _mainloop: pw::main_loop::MainLoopRc,
    context: pw::context::ContextRc,
    core: pw::core::CoreRc,
    /// Proxies for the objects *we* created remotely (null-sinks, links).
    ///
    /// Since pipewire-rs 0.10 `Node`/`Link` are **owning** proxies: dropping
    /// one calls `pw_proxy_destroy()` and tears the PipeWire object down.
    /// We must therefore hold on to them for as long as the object should
    /// live (i.e. until `destroy_global`/teardown) instead of letting them
    /// fall out of scope right after creation.
    created_nodes: RefCell<Vec<pw::node::Node>>,
    created_links: RefCell<Vec<pw::link::Link>>,
}

/// Owned registry snapshot type (matches the spike).
type OwnedGlobal = GlobalObject<PropertiesBox>;

impl PwSession {
    /// Block the current (worker) thread until the core acknowledges a sync.
    fn roundtrip(&self) {
        let ml = self._mainloop.clone();
        let done = Rc::new(std::cell::Cell::new(false));
        let done_clone = done.clone();
        let loop_clone = ml.clone();
        let pending = match self.core.sync(0) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("pw roundtrip sync failed: {e}");
                return;
            }
        };
        let _l = self
            .core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done_clone.set(true);
                    loop_clone.quit();
                }
            })
            .register();
        while !done.get() {
            ml.run();
        }
    }

    /// Service the mainloop once, blocking up to `timeout` for PipeWire
    /// events, and return the number of dispatched sources.
    ///
    /// # Why this exists (real-time correctness)
    ///
    /// The filter-chain modules we host live **in this process** (see
    /// [`Engine::create_chain`]): `module-filter-chain` exports two client
    /// nodes whose protocol traffic — format/buffer negotiation, node
    /// activation, `set_param` delivery — is dispatched by the **mainloop**
    /// (the RT `process` callbacks run on libpipewire's data-loop thread, but
    /// everything that makes a node *schedulable* needs the mainloop).
    ///
    /// The Python engine never had this problem because it hosts the chain in
    /// a dedicated `pw-cli` process whose only job is running its mainloop.
    /// A daemon that only iterates its mainloop *inside* an IPC operation
    /// leaves the chain unserviced the rest of the time: measured on PipeWire
    /// 0.3.65, a playback stream connecting to `input.Holaf_FX_<uid>` while
    /// the worker sits in `recv()` stays in `connecting` **forever** (the
    /// graph is never started), and when the loop is pumped only by periodic
    /// polls the node misses its deadlines (the audible symptom: a continuous
    /// fast on/off chopping on timing-sensitive devices such as USB cards).
    pub fn pump_mainloop(&self, timeout: Duration) -> i32 {
        self._mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(timeout))
    }

    /// Sleep for `total` while keeping the mainloop serviced.
    ///
    /// Used by every polling/settling wait inside the engine: a plain
    /// `thread::sleep` freezes a live chain for its whole duration (a
    /// `wait_node` retry loop can sleep for hundreds of milliseconds), which
    /// is audible as a dropout on an already-running graph.
    fn sleep_pumping(&self, total: Duration) {
        let deadline = std::time::Instant::now() + total;
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return;
            }
            let slice = (deadline - now).min(PUMP_SLICE);
            self.pump_mainloop(slice);
        }
    }

    /// Fresh registry snapshot (a new registry proxy each time so globals are
    /// replayed — the spike's proven pattern).
    fn snapshot(&self) -> Vec<OwnedGlobal> {
        let registry = match self.core.get_registry() {
            Ok(r) => r,
            Err(e) => {
                log::warn!("snapshot: get_registry failed: {e}");
                return Vec::new();
            }
        };
        let list: Rc<RefCell<Vec<OwnedGlobal>>> = Rc::new(RefCell::new(Vec::new()));
        let l2 = list.clone();
        let _listener = registry
            .add_listener_local()
            .global(move |g| l2.borrow_mut().push(g.to_owned()))
            .register();
        self.roundtrip();
        std::mem::drop(_listener);
        Rc::try_unwrap(list).ok().unwrap_or_default().into_inner()
    }

    fn view(&self) -> LiveView {
        collect_view(&self.snapshot())
    }

    /// Wait until a node with `name` appears (e.g. a freshly-created null-sink
    /// or a chain stream).
    fn wait_node(&self, name: &str, attempts: u32, delay_ms: u64) -> Option<LiveNode> {
        for _ in 0..attempts {
            if let Some(n) = self.view().node_named(name).cloned() {
                return Some(n);
            }
            // Pump rather than sleep: a live chain must keep being serviced
            // while we wait for a registry object to show up.
            self.sleep_pumping(Duration::from_millis(delay_ms));
        }
        None
    }

    /// Create a strip's **bus** null-sink under its planned name (see
    /// [`graph::bus_node_name`]).
    ///
    /// The node also carries `holaf.uid` / `holaf.kind` / `holaf.label`
    /// ([`graph::UID_PROP`], [`graph::KIND_PROP`], [`graph::LABEL_PROP`]).  They
    /// are **informational** — they make `pw-dump` / `pw-cli info <node>`
    /// self-explanatory — and deliberately not what the reconciler keys on:
    /// PipeWire exports only a fixed key subset in registry globals, so a custom
    /// key never reaches the client that would need it (see the `graph` module
    /// docs).
    fn create_null_sink(
        &self,
        name: &str,
        description: &str,
        uid: &str,
        label: &str,
    ) -> Result<(), BackendError> {
        let node = self
            .core
            .create_object::<pw::node::Node>(
                "adapter",
                &pw::properties::properties! {
                    "factory.name" => "support.null-audio-sink",
                    "node.name" => name,
                    "node.description" => description,
                    "media.class" => "Audio/Sink",
                    "audio.channels" => "2",
                    "audio.position" => "[ FL, FR ]",
                    "object.linger" => "1",
                    graph::UID_PROP => uid,
                    graph::KIND_PROP => graph::KIND_BUS,
                    graph::LABEL_PROP => label,
                },
            )
            .map_err(|e| BackendError::internal("create bus", e))?;
        // Keep the owning proxy alive: dropping it would destroy the node.
        self.created_nodes.borrow_mut().push(node);
        if self.wait_node(name, NODE_POLL_ATTEMPTS, NODE_POLL_DELAY_MS).is_none() {
            return Err(BackendError::new(
                "node_not_ready",
                format!("bus '{name}' never appeared in the registry"),
            ));
        }
        log::info!("created bus '{name}' (uid '{uid}')");
        Ok(())
    }

    /// Destroy a registry global.
    ///
    /// Objects *we* created are torn down through [`Core::destroy_object`],
    /// which consumes the owning proxy: that is the lifecycle pipewire-rs
    /// 0.10 documents, and it keeps the local proxy and the remote object in
    /// lock-step (dropping an owning proxy calls `pw_proxy_destroy()`).
    /// Everything else — other clients' objects, debris from a previous run,
    /// WirePlumber links — goes through the registry.
    fn destroy_global(&self, id: u32) {
        let node = {
            let mut nodes = self.created_nodes.borrow_mut();
            let pos = nodes.iter().position(|n| n.upcast_ref().id() == id);
            pos.map(|i| nodes.remove(i))
        };
        if let Some(node) = node {
            if let Err(e) = self.core.destroy_object(node) {
                log::warn!("destroy null-sink (id {id}) failed: {e}");
            }
            return;
        }

        let link = {
            let mut links = self.created_links.borrow_mut();
            let pos = links.iter().position(|l| l.upcast_ref().id() == id);
            pos.map(|i| links.remove(i))
        };
        if let Some(link) = link {
            if let Err(e) = self.core.destroy_object(link) {
                log::warn!("destroy link (id {id}) failed: {e}");
            }
            return;
        }

        if let Ok(registry) = self.core.get_registry() {
            let _ = registry.destroy_global(id);
        }
    }

    /// Bind a live node by name to a typed proxy (for `set_param`); `Ok(None)`
    /// when the node is absent (e.g. a physical device not plugged in).
    fn bind_node_by_name(&self, name: &str) -> Result<Option<pw::node::Node>, BackendError> {
        let snap = self.snapshot();
        let Some(global) = snap
            .iter()
            .find(|g| {
                g.type_ == pw::types::ObjectType::Node
                    && g.props.as_ref().and_then(|p| p.get("node.name")) == Some(name)
            })
        else {
            return Ok(None);
        };
        let registry = self
            .core
            .get_registry()
            .map_err(|e| BackendError::internal("get_registry", e))?;
        registry
            .bind(global)
            .map(Some)
            .map_err(|e| BackendError::internal(&format!("bind node '{name}'"), e))
    }

    /// Bind the live metadata object named `metadata.name == "default"`.
    fn bind_default_metadata(&self) -> Result<Option<pw::metadata::Metadata>, BackendError> {
        let snap = self.snapshot();
        let Some(global) = snap
            .iter()
            .find(|g| {
                g.type_ == pw::types::ObjectType::Metadata
                    && g.props
                        .as_ref()
                        .and_then(|p| p.get("metadata.name"))
                        == Some("default")
            })
        else {
            return Ok(None);
        };
        let registry = self
            .core
            .get_registry()
            .map_err(|e| BackendError::internal("get_registry", e))?;
        registry
            .bind(global)
            .map(Some)
            .map_err(|e| BackendError::internal("bind metadata 'default'", e))
    }
}

/// How many links a [`Engine::link_nodes`] call established or found already
/// in place (0/0 = the nodes exist but PipeWire never published a usable port
/// pair, e.g. after many create/destroy cycles on a long-lived daemon).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LinkReport {
    created: usize,
    existing: usize,
}

impl LinkReport {
    /// Links that are live as a result of the call.
    fn total(self) -> usize {
        self.created + self.existing
    }
}

/// What a later repair pass must redo for a [`PendingIssue`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Retry {
    /// Re-run the whole strip reconciliation (node → chain → links → volume).
    Strip { uid: String },
    /// Re-create one route's links.
    Route {
        source_uid: String,
        target_uid: String,
    },
}

/// A piece of graph work that could not be completed right now.
///
/// The reconciler never fails a whole snapshot because one node is missing:
/// it records the issue, keeps building the rest of the graph, reports it in
/// `status` and retries it in the background ([`Engine::repair_pending`]).
#[derive(Debug, Clone)]
struct PendingIssue {
    /// Stable machine-readable key (`strip:<uid>`, `route:<src>-><dst>`).
    key: String,
    /// Human-readable reason, surfaced by `status.pending`.
    message: String,
    retry: Retry,
}

/// The reconciler + runtime state, owned by the worker thread.
pub struct Engine {
    session: PwSession,
    avail: PluginSet,
    /// Desired strips (post-migration), keyed by uid — for `status` and diffs.
    strips: IndexMap<String, Strip>,
    /// `uid → bus node name` of the current snapshot (see
    /// [`graph::NodeNames`]).
    ///
    /// One plan for the whole snapshot, so that the volume push, the FX chain
    /// anchor, the routes and the metering all agree on the same name.  It is
    /// re-derived at every `apply_snapshot` and *kept* (the plan is sticky) so
    /// the reconciler can still find, and tear down, the node of a strip that
    /// just disappeared from the snapshot.
    names: graph::NodeNames,
    /// Where this engine's filter-chain modules live: in a dedicated child
    /// process (`holafd --fx-host`, the default) or inside this process.
    /// See [`ChainHost`] — the engine is deliberately agnostic to the answer.
    host: ChainHost,
    /// Active routes `(source_uid, target_uid)`.
    routes: HashSet<(String, String)>,
    default_sink: Option<String>,
    running: bool,
    /// Graph work that is still missing (see [`PendingIssue`]).
    pending: IndexMap<String, PendingIssue>,
    /// Automatic repair passes performed since `pending` became non-empty.
    repair_attempts: u32,
    /// Earliest instant of the next automatic repair pass.
    next_repair_at: Instant,
    /// Mainloop iterations performed while no order was in flight.
    ///
    /// Diagnostics only (reported by `status`): a daemon that hosts filter
    /// chains must keep this growing while it is idle — a frozen counter means
    /// the mainloop is starved and every hosted chain is starving with it.
    idle_pumps: u64,
}

impl Engine {
    /// Connect to the daemon using the process environment
    /// (`XDG_RUNTIME_DIR` / `PIPEWIRE_RUNTIME_DIR` / `PIPEWIRE_REMOTE` are
    /// honoured by PipeWire, so pointing the backend at the headless daemon is
    /// purely an environment concern).
    pub fn connect() -> Result<Engine, BackendError> {
        Engine::connect_with(FxHostMode::default())
    }

    /// Connect and pick where the FX chains are hosted ([`FxHostMode`]).
    pub fn connect_with(host_mode: FxHostMode) -> Result<Engine, BackendError> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)
            .map_err(|e| BackendError::new("pipewire_error", format!("mainloop: {e}")))?;
        let context = pw::context::ContextRc::new(&mainloop, None)
            .map_err(|e| BackendError::new("pipewire_error", format!("context: {e}")))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| BackendError::new("pipewire_error", format!("connect: {e}")))?;
        let session = PwSession {
            _mainloop: mainloop,
            context,
            core,
            created_nodes: RefCell::new(Vec::new()),
            created_links: RefCell::new(Vec::new()),
        };
        session.roundtrip();
        let avail = PluginSet::on_disk();
        log::info!(
            "PipeWire engine connected — {} LADSPA plugin(s) available",
            avail.count()
        );
        // The FX host is built *after* the context exists: the in-process
        // flavour needs it, and a failed external spawn falls back to it.
        let host = ChainHost::from_mode(host_mode, session.context.clone());
        log::info!("FX chains are hosted by: {}", host.describe());
        Ok(Engine {
            session,
            avail,
            strips: IndexMap::new(),
            names: graph::NodeNames::default(),
            host,
            routes: HashSet::new(),
            default_sink: None,
            running: false,
            pending: IndexMap::new(),
            repair_attempts: 0,
            next_repair_at: Instant::now(),
            idle_pumps: 0,
        })
    }

    /// Must this process keep servicing its mainloop *while it is idle*?
    ///
    /// Only when it hosts at least one filter-chain module **itself**: such a
    /// module is only schedulable through its host's mainloop traffic, so the
    /// loop must be pumped while one is loaded (and may block on the order
    /// channel otherwise, at zero CPU cost).  With a dedicated FX host the
    /// child pumps its own loop and the daemon has nothing to service when no
    /// order is in flight — which is exactly the point of the split.
    pub fn needs_idle_pumping(&self) -> bool {
        self.host.pumps_own_mainloop()
    }

    /// Service the PipeWire mainloop once, blocking up to `timeout` for events.
    ///
    /// The worker loop calls this whenever no order is pending: the process
    /// **hosts** the filter-chain modules of the strips, so its mainloop must
    /// keep running even when the daemon has nothing to do (see
    /// [`PwSession::pump_mainloop`] for the full rationale and measurements).
    pub fn pump_mainloop(&mut self, timeout: Duration) -> i32 {
        self.idle_pumps = self.idle_pumps.wrapping_add(1);
        self.session.pump_mainloop(timeout)
    }

    /// Execute one backend operation (called by the worker loop).
    pub fn handle(&mut self, op: EngineOp) -> Result<JsonValue, BackendError> {
        match op {
            EngineOp::ApplySnapshot(strips) => self.apply_snapshot(strips),
            EngineOp::Stop => {
                self.stop()?;
                Ok(json!({ "ok": true, "stopped": true }))
            }
            EngineOp::Status => self.status(),
            EngineOp::SetVolume { uid, value } => self.set_volume(&uid, value),
            EngineOp::SetMute { uid, muted } => self.set_mute(&uid, muted),
            EngineOp::SetMono { uid, mono } => self.set_mono(&uid, mono),
            EngineOp::FxSetActive { uid, effect, active } => {
                self.fx_set_active(&uid, effect, active)
            }
            EngineOp::FxSetParams { uid, effect, params } => {
                self.fx_set_params(&uid, effect, &params)
            }
            EngineOp::RoutingSet {
                source_uid,
                target_uid,
                active,
            } => self.routing_set(&source_uid, &target_uid, active),
            EngineOp::RoutingList => Ok(self.routing_list_json()),
            EngineOp::SetDefaultSink { uid } => self.set_default_sink(&uid),
            EngineOp::DeviceList => self.device_list(),
            EngineOp::AppList => Ok(app_list_stub()),
            EngineOp::AppMove { .. } => Ok(app_move_stub()),
            EngineOp::MeterSources => Ok(self.meter_sources_json()),
        }
    }

    /// Stop the engine and tear down the whole graph (Python `shutdown`).
    pub fn stop(&mut self) -> Result<(), BackendError> {
        self.teardown_all();
        // No chain may outlive the graph that pointed at it: with a dedicated
        // FX host this reaps the child process (a later `load` respawns it).
        self.host.shutdown();
        log::info!("FX chains are hosted by: {}", self.host.describe());
        self.running = false;
        self.pending.clear();
        self.repair_attempts = 0;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Pending graph work — record, report, repair
    // ------------------------------------------------------------------

    /// Key of the pending item covering one strip.
    fn strip_key(uid: &str) -> String {
        format!("strip:{uid}")
    }

    /// Key of the pending item covering one route.
    fn route_key(source_uid: &str, target_uid: &str) -> String {
        format!("route:{source_uid}->{target_uid}")
    }

    /// Record (or refresh) an item the reconciler could not complete.
    ///
    /// Logged at `warn` on the first attempt and at `debug` afterwards: a
    /// device that stays unplugged must not flood the log every few seconds,
    /// but the *first* occurrence has to be visible (a silent failure is what
    /// made the original bug undiagnosable).
    fn note_pending(&mut self, key: String, message: impl Into<String>, retry: Retry) {
        let message = message.into();
        let retried = self.repair_attempts > 0;
        if self.pending.is_empty() && !retried {
            self.repair_attempts = 0;
            self.next_repair_at = Instant::now() + repair_backoff(1);
        }
        if retried {
            log::debug!("graph pending [{key}] {message} (retry)");
        } else {
            log::warn!("graph pending [{key}] {message}");
        }
        self.pending.insert(
            key.clone(),
            PendingIssue {
                key,
                message,
                retry,
            },
        );
    }

    /// Forget a pending item (it has just been completed).
    fn clear_pending(&mut self, key: &str) {
        if self.pending.shift_remove(key).is_some() {
            log::info!("graph pending [{key}] resolved");
        }
        if self.pending.is_empty() {
            self.repair_attempts = 0;
        }
    }

    /// Is some part of the graph still missing?
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Time to wait before the next automatic repair pass, `None` when there is
    /// nothing to repair.
    pub fn repair_delay(&self) -> Option<Duration> {
        if self.pending.is_empty() {
            return None;
        }
        Some(self.next_repair_at.saturating_duration_since(Instant::now()))
    }

    /// Is a repair pass due right now?
    pub fn repair_due(&self) -> bool {
        self.repair_delay() == Some(Duration::ZERO)
    }

    /// Does a **separate process** host the FX chains?  (Drives the worker's
    /// maintenance tick: only then can the host die without the daemon
    /// noticing.)
    pub fn hosts_external(&self) -> bool {
        self.host.mode() == FxHostMode::External
    }

    /// Periodic maintenance of the FX host, called by the worker while the
    /// daemon is idle.
    ///
    /// Two jobs, both cheap when nothing happened:
    ///
    /// 1. **notice a dead host** and respawn it ([`ChainHost::ensure_alive`]) —
    ///    otherwise a host that died while the UI was silent would only be
    ///    replaced by the next order that happens to call `load`;
    /// 2. turn every chain the respawn restored into a [`PendingIssue`], so the
    ///    regular repair pass redoes the intra-strip link and the volume: the
    ///    chain's stream nodes are brand new (the old ones died with the old
    ///    client connection), so a restored-but-unlinked chain would be a
    ///    silent strip.
    pub fn maintain(&mut self) {
        let before = self.host.respawns();
        if let Err(e) = self.host.ensure_alive() {
            log::warn!("cannot restart the FX host: {}", e.message);
        } else if self.host.respawns() != before {
            log::warn!("the FX host was restarted — chains have to be re-verified");
        }
        self.drain_host_recoveries();
    }

    /// Turn "this chain was restored by a respawn" into pending graph work.
    fn drain_host_recoveries(&mut self) {
        for uid in self.host.take_restored() {
            if self.strips.contains_key(&uid) {
                self.note_pending(
                    Self::strip_key(&uid),
                    "FX host restarted — re-verifying its chain",
                    Retry::Strip { uid },
                );
            }
        }
    }

    /// Retry every pending item (called by the worker between orders).
    ///
    /// This is what makes the graph *self-healing*: a USB card that enumerates
    /// 300 ms after the app applied its snapshot, or ports that PipeWire
    /// publishes late, are picked up without any further client order — the
    /// user does not have to toggle anything to get a working graph.
    pub fn repair_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        self.repair_attempts = self.repair_attempts.saturating_add(1);
        self.next_repair_at = Instant::now() + repair_backoff(self.repair_attempts);
        // Visible while the graph is briefly incomplete (a device that is still
        // enumerating), then debug: a device that stays unplugged must not fill
        // the log every 5 s.
        if self.repair_attempts <= 3 {
            log::info!(
                "repair pass #{} — {} pending item(s): {}",
                self.repair_attempts,
                self.pending.len(),
                self.pending_summary()
            );
        } else {
            log::debug!(
                "repair pass #{} — {} pending item(s)",
                self.repair_attempts,
                self.pending.len()
            );
        }
        let items: Vec<(String, Retry)> = self
            .pending
            .values()
            .map(|p| (p.key.clone(), p.retry.clone()))
            .collect();
        // Each attempt re-records its own item on failure (with a fresh
        // message), so clear the map first.
        self.pending.clear();
        for (key, retry) in items {
            match retry {
                Retry::Strip { uid } => {
                    if let Some(strip) = self.strips.get(&uid).cloned() {
                        self.sync_strip(&strip);
                    } else {
                        self.pending.shift_remove(&key);
                    }
                }
                Retry::Route {
                    source_uid,
                    target_uid,
                } => {
                    if !self
                        .routes
                        .contains(&(source_uid.clone(), target_uid.clone()))
                    {
                        continue; // route no longer wanted
                    }
                    self.ensure_route(&source_uid, &target_uid);
                }
            }
        }
        if self.pending.is_empty() {
            log::info!("repair: the graph is complete again");
            self.repair_attempts = 0;
        }
    }

    /// `[{key, message}]` — the `status.pending` payload.
    fn pending_json(&self) -> JsonValue {
        JsonValue::Array(
            self.pending
                .values()
                .map(|p| json!({ "key": p.key, "message": p.message }))
                .collect(),
        )
    }

    /// One-line summary for the logs.
    fn pending_summary(&self) -> String {
        self.pending
            .values()
            .map(|p| format!("{} ({})", p.key, p.message))
            .collect::<Vec<_>>()
            .join("; ")
    }

    // ------------------------------------------------------------------
    // apply_snapshot — full-diff reconcile
    // ------------------------------------------------------------------

    fn apply_snapshot(&mut self, strips: Vec<Strip>) -> Result<JsonValue, BackendError> {
        let first_apply = !self.running;
        if first_apply {
            // First apply after connect: start from a clean graph.
            //
            // (a) Chains *we* host are the remnants of an earlier, failed
            //     apply of this same process.  Their module handles would make
            //     every later `push_chain_params` fail with `chain_gone` while
            //     `modules` still claims the chain exists — the state that
            //     made a daemon permanently unusable.  Unload them first, then
            // (b) sweep the debris left by a previous crashed run.
            self.unload_all_chains();
            self.sweep_zombies();
            self.strips.clear();
            self.routes.clear();
            self.pending.clear();
            self.repair_attempts = 0;
        }

        let desired: IndexMap<String, Strip> = strips
            .into_iter()
            .filter(|s| !s.uid.is_empty())
            .map(|s| (s.uid.clone(), s))
            .collect();

        // 1) teardown strips that disappeared.
        let old_uids: Vec<String> = self.strips.keys().cloned().collect();
        for uid in old_uids {
            if !desired.contains_key(&uid) {
                log::info!("apply_snapshot: teardown removed strip '{uid}'");
                self.teardown_strip(&uid);
            }
        }

        // 2) Publish the desired state **before** touching any node.
        //
        // Volume, route and chain resolution all look a strip up in
        // `self.strips`; the previous code published it only at the very end,
        // so during the first apply every lookup missed and no strip's volume
        // was ever applied (measured: the node stayed at `channelVolumes 1.0`
        // until an unrelated *second* apply — the "much louder than the Python
        // engine" report).
        self.strips = desired;
        // The naming plan of this snapshot: sticky (a strip keeps the name it
        // already had while its label slugs to the same base) and collision-free
        // (`Holaf_Bus_Mix_Out`, `Holaf_Bus_Mix_Out_2`, …).  Every later phase
        // derives its names from this single plan.
        let previous_names = std::mem::take(&mut self.names);
        self.names = graph::NodeNames::plan(self.strips.values(), &previous_names);
        // A strip whose label changed publishes a new bus name: the node under
        // the old name is ours and must go (its FX chain is anchored to it).
        self.drop_renamed_buses(&previous_names);
        let uids: Vec<String> = self.strips.keys().cloned().collect();

        // 3) strip nodes first (null-sinks) — no link before its node exists.
        for uid in &uids {
            let strip = self.strips[uid].clone();
            self.sync_strip_node(&strip);
        }

        // 4) then the FX chains (each anchored to a node that really exists).
        for uid in &uids {
            let strip = self.strips[uid].clone();
            self.sync_strip_chain(&strip);
        }

        // 5) then the routing matrix (links between strips — links last).
        let wanted_routes: HashSet<(String, String)> = self
            .strips
            .values()
            .filter(|s| s.is_input())
            .flat_map(|s| {
                s.routes
                    .iter()
                    .filter(|t| self.strips.contains_key(*t))
                    .map(|t| (s.uid.clone(), t.clone()))
            })
            .collect();
        self.reconcile_routes(&wanted_routes);

        // 6) verification pass: the intra-strip chain links and the volume of
        //    every strip.  Both are idempotent, so this pass also repairs what
        //    an earlier (failed) pass left behind.
        for uid in &uids {
            let strip = self.strips[uid].clone();
            self.ensure_chain_links(&strip);
            self.sync_strip_volume(&strip);
        }

        // 7) default sink (is_default on an input strip — mirrors Python).
        let default_uid = self
            .strips
            .values()
            .find(|s| s.is_default)
            .or_else(|| self.strips.values().find(|s| s.is_input()))
            .map(|s| s.uid.clone());
        if let Some(uid) = default_uid {
            let _ = self.set_default_sink(&uid);
        }

        // Nothing named like one of our buses may live outside the plan: that
        // is what guarantees a renamed or removed strip never leaves an orphan
        // bus behind (and what replaces a node created by an older build).
        self.sweep_stale_buses();

        self.running = true;
        // A respawn during this apply restored chains whose links this same
        // pass may already have rebuilt — re-verify them through the repair
        // machinery rather than assuming either way.
        self.drain_host_recoveries();
        let count = self.strips.len();
        if self.pending.is_empty() {
            log::info!("apply_snapshot: {count} strip(s) reconciled");
        } else {
            log::warn!(
                "apply_snapshot: {count} strip(s) reconciled, {} item(s) pending: {}",
                self.pending.len(),
                self.pending_summary()
            );
        }
        Ok(json!({
            "applied": count,
            "running": true,
            "degraded": !self.pending.is_empty(),
            "pending": self.pending_json(),
        }))
    }

    /// Reconcile one strip end to end: node → chain → intra-strip link → volume.
    /// Never fails: whatever cannot be done is recorded as pending.
    fn sync_strip(&mut self, strip: &Strip) {
        self.sync_strip_node(strip);
        let topology_changed = self.sync_strip_chain(strip);
        self.ensure_chain_links(strip);
        self.sync_strip_volume(strip);
        if topology_changed {
            // The strip's endpoints just moved (raw node ↔ `*.Holaf_FX_<uid>`
            // streams), so every route it takes part in must be re-derived.
            // Without this, a route repaired while the chain did not exist yet
            // stays wired to the raw node *in parallel* with the new chain —
            // measured: the input null-sink fed both the device and the chain
            // (doubled path, EQ inaudible).
            let wanted = self.routes.clone();
            self.reconcile_routes(&wanted);
        }
    }

    /// Reconcile the strip's own node: a null-sink for a virtual strip, and for
    /// a physical strip nothing to create — but a *missing device* is recorded
    /// so the graph heals as soon as it shows up.
    ///
    /// # Renaming (label change)
    ///
    /// The bus name follows the strip **label**, so renaming a strip in the UI
    /// changes its public node name.  [`Engine::drop_renamed_buses`] destroys
    /// the node published under the old name (unloading its FX chain first, since
    /// the chain's `target.object` still names it), and this function then
    /// creates the bus under the new one.  Nothing is left behind: the naming
    /// plan is the ownership record, and [`Engine::sweep_stale_buses`] enforces
    /// it at the end of every apply.
    fn sync_strip_node(&mut self, strip: &Strip) {
        let uid = &strip.uid;
        if graph::is_physical(strip) {
            let device = self.names.master_node(strip);
            if self.session.view().node_named(&device).is_none() {
                // Headless / unplugged / not negotiated yet: the device node is
                // owned by its driver, we cannot create it.  Record it so a
                // later apply — or the automatic repair pass — wires the
                // chain, the routes and the volume the moment it appears.
                self.note_pending(
                    Self::strip_key(uid),
                    format!(
                        "device '{device}' is not in the registry — FX chain, routes and 
                         volume deferred until it appears"
                    ),
                    Retry::Strip { uid: uid.clone() },
                );
            }
            return;
        }

        let name = self.bus_name(uid);

        // The bus is already published under the name this snapshot's plan
        // gives it: nothing to do (the common case on every apply).
        //
        // Names are collision-free *within* a plan, so a live node carrying
        // exactly this name is this strip's bus (a rename was already handled
        // by [`Engine::drop_renamed_buses`], any other leftover by
        // [`Engine::sweep_stale_buses`]).
        if self.session.view().node_named(&name).is_some() {
            return;
        }

        let desc = format!("Holaf Mix: {}", strip.label);
        if let Err(e) = self.session.create_null_sink(&name, &desc, uid, &strip.label) {
            log::warn!(
                "strip '{}': bus '{name}' could not be created: {}",
                strip.label,
                e.message
            );
            self.note_pending(
                Self::strip_key(uid),
                e.message,
                Retry::Strip { uid: uid.clone() },
            );
        }
    }

    /// Destroy a node **and every link touching it**.
    ///
    /// A leftover link would reference a destroyed node — WirePlumber then
    /// spends a moment re-deriving the graph, and on PipeWire 0.3.65 the stale
    /// link objects can keep a destroyed node's id alive in the registry.
    fn destroy_node_with_links(&mut self, node_id: u32) {
        let links: Vec<u32> = self
            .session
            .view()
            .links_of_node(node_id)
            .map(|l| l.id)
            .collect();
        for id in links {
            self.session.destroy_global(id);
        }
        self.session.destroy_global(node_id);
        for _ in 0..2 {
            self.session.roundtrip();
        }
    }

    /// Destroy the bus a strip published under a **previous** name.
    ///
    /// Renaming a strip in the UI changes its label, hence its bus name (see
    /// [`graph::NodeNames`]).  Left alone, the old node would keep playing the
    /// same signal under a name nobody can trace back to a strip — and, worse,
    /// the strip would own two sinks.  The node is recognised by the name the
    /// *previous* plan gave it (we are the only creator of `Holaf_Bus_*`), the
    /// FX chain is unloaded first because its `target.object` still names it,
    /// and the new bus is created by the node phase that follows.
    fn drop_renamed_buses(&mut self, previous: &graph::NodeNames) {
        let renamed: Vec<(String, String, String)> = self
            .strips
            .values()
            .filter_map(|strip| {
                let old = previous.get(&strip.uid)?;
                let new = self.names.get(&strip.uid)?;
                (old != new).then(|| (strip.uid.clone(), strip.label.clone(), old.to_string()))
            })
            .collect();
        for (uid, label, old_name) in renamed {
            let Some(id) = self.session.view().node_named(&old_name).map(|n| n.id) else {
                continue; // never published (e.g. the strip was physical)
            };
            log::info!(
                "strip '{label}': bus renamed '{old_name}' → '{}' (label changed)",
                self.bus_name(&uid)
            );
            // The chain is anchored to the old name: drop it, the chain phase
            // rebuilds it against the new bus.
            self.unload_chain(&uid);
            self.destroy_node_with_links(id);
        }
    }

    /// Destroy every bus-looking node that is **not** in the current plan.
    ///
    /// One invariant, checked after every apply: the live set of
    /// `Holaf_Bus_*` / `Holaf_Strip_*` nodes must be exactly what the plan
    /// asks for.  Everything else is debris — a bus left by a renamed strip, a
    /// strip removed from the snapshot, a node created by an older build (or by
    /// the legacy Python engine), a crashed run.  Names are collision-free
    /// within a plan, so this can never destroy a live bus.
    ///
    /// FX chain nodes (`Holaf_FX_*`, `input./output.Holaf_FX_*`) are **not**
    /// touched: they belong to the chain host's handles, which own their
    /// lifecycle.
    fn sweep_stale_buses(&mut self) {
        let planned: HashSet<String> = self
            .strips
            .values()
            .filter(|s| !graph::is_physical(s))
            .map(|s| self.bus_name(&s.uid))
            .collect();
        let stale: Vec<(u32, String)> = {
            let view = self.session.view();
            view.nodes()
                .iter()
                .filter(|n| {
                    n.name.starts_with(graph::BUS_PREFIX)
                        || n.name.starts_with(graph::LEGACY_STRIP_PREFIX)
                })
                .filter(|n| !planned.contains(&n.name))
                .map(|n| (n.id, n.name.clone()))
                .collect()
        };
        for (id, name) in stale {
            log::info!("destroying stale bus '{name}' (not part of the snapshot)");
            self.destroy_node_with_links(id);
        }
    }

    /// The public bus name of a strip (`Holaf_Bus_<Label>`, suffix included),
    /// see [`graph::NodeNames`].
    fn bus_name(&self, uid: &str) -> String {
        if let Some(name) = self.names.get(uid) {
            return name.to_string();
        }
        match self.strips.get(uid) {
            Some(strip) => graph::bus_node_name(&strip.label, 1),
            // Not a strip we know (teardown of a phantom uid): fall back to the
            // historical name so any legacy node is still found.
            None => graph::legacy_strip_node_name(uid),
        }
    }

    /// Reconcile the strip's FX chain (create / hot-reload / unload).
    ///
    /// Returns `true` when the chain **topology** changed (created, unloaded or
    /// a stale handle was dropped), i.e. when the strip's effective route
    /// endpoints may have moved.
    fn sync_strip_chain(&mut self, strip: &Strip) -> bool {
        let uid = &strip.uid;
        let want_chain = graph::wants_fx_chain(strip, &self.avail);

        // A module handle is only authoritative while the nodes it exports are
        // live: a sweep, an external `pw-cli destroy` or a module crash can
        // remove them underneath us.  Detect that *before* using the handle,
        // otherwise the reconciler is stuck in `chain_gone` forever.
        let mut chain_exists = self.host.contains(uid);
        let mut topology_changed = false;
        if chain_exists && !self.chain_streams_live(uid) {
            log::warn!(
                "strip '{}': FX chain handle is stale (its stream nodes are gone) — 
                 dropping it so the chain is rebuilt",
                strip.label
            );
            self.host.mark_stale(uid);
            self.settle(2);
            chain_exists = false;
            topology_changed = true;
        }

        if want_chain && !chain_exists {
            // The chain's anchor node must exist *before* the module loads.
            //
            // `module-filter-chain` hands its playback/capture stream a
            // `target.object`; when that node does not resolve, WirePlumber's
            // policy links the stream to the default sink instead — measured: an
            // output strip's chain ended up playing into another strip's
            // null-sink, i.e. a completely wrong audio path — and the explicit
            // intra-strip link could never be made.  Defer instead.
            let anchor = self.names.master_node(strip);
            if self.session.view().node_named(&anchor).is_none() {
                // Warn once, then debug: the repair pass retries this every few
                // seconds while the device is away.
                if self.repair_attempts > 0 {
                    log::debug!(
                        "strip '{}': FX chain deferred — target node '{anchor}' is not in the registry",
                        strip.label
                    );
                } else {
                    log::warn!(
                        "strip '{}': FX chain deferred — target node '{anchor}' is not in the registry",
                        strip.label
                    );
                }
                self.note_pending(
                    Self::strip_key(uid),
                    format!("waiting for node '{anchor}' before building the FX chain"),
                    Retry::Strip { uid: uid.clone() },
                );
                return topology_changed;
            }
            match self.create_chain(strip) {
                Ok(()) => topology_changed = true,
                Err(e) => {
                    log::warn!(
                        "strip '{}': FX chain creation failed: {}",
                        strip.label,
                        e.message
                    );
                    self.note_pending(
                        Self::strip_key(uid),
                        e.message,
                        Retry::Strip { uid: uid.clone() },
                    );
                }
            }
        } else if want_chain {
            // Exists: re-push control values so a snapshot reflects the
            // current desired params (the chain graph already contains every
            // plugin; only controls change).
            if let Err(e) = self.push_chain_params(strip) {
                log::warn!(
                    "strip '{}': FX control push failed: {}",
                    strip.label,
                    e.message
                );
                self.note_pending(
                    Self::strip_key(uid),
                    e.message,
                    Retry::Strip { uid: uid.clone() },
                );
            }
        } else if chain_exists {
            // "destroy if obsolete": a snapshot with no active effect removes
            // the chain (runtime toggles, by contrast, keep it bypassed).
            log::info!("apply_snapshot: strip '{}' no longer wants FX — unloading chain", uid);
            self.unload_chain(uid);
            topology_changed = true;
        }
        topology_changed
    }

    /// (Re-)create the link *inside* a strip that owns an FX chain:
    ///
    /// ```text
    /// output strip : output.Holaf_FX_<uid>  →  <master node>
    /// input  strip : <master node>          →  input.Holaf_FX_<uid>
    /// ```
    ///
    /// Idempotent, and re-verified at every apply, on every hot toggle and in
    /// every repair pass.  This link used to be created exactly once, inside
    /// `create_chain`: a chain built before its target node was in the registry
    /// stayed unlinked **forever**, because a later apply saw the chain as
    /// existing and only pushed parameters — the measured reason EQ-active-at-
    /// startup stayed broken while toggling the EQ changed nothing.
    fn ensure_chain_links(&mut self, strip: &Strip) -> bool {
        let uid = &strip.uid;
        if !self.host.contains(uid) {
            return true;
        }
        let anchor = self.names.master_node(strip);
        let (source, target, mono) = match ChainFlavour::of(strip) {
            ChainFlavour::Output => (
                graph::fx_output_node_name(uid),
                anchor,
                strip.is_mono,
            ),
            ChainFlavour::Input => (anchor, graph::fx_input_node_name(uid), false),
        };
        match self.link_nodes(&source, &target, mono) {
            Ok(report) if report.total() > 0 => {
                self.clear_pending(&Self::strip_key(uid));
                true
            }
            Ok(_) => {
                self.note_pending(
                    Self::strip_key(uid),
                    format!("no usable port pair for '{source}' → '{target}' (chain not wired)"),
                    Retry::Strip { uid: uid.clone() },
                );
                false
            }
            Err(e) => {
                self.note_pending(
                    Self::strip_key(uid),
                    e.message,
                    Retry::Strip { uid: uid.clone() },
                );
                false
            }
        }
    }

    /// Push the strip's volume / mute to its master node.
    ///
    /// Runs for **every** strip on **every** apply, including the first one: the
    /// master node name is derived from the strip (never from the engine's
    /// state map), which is what makes the first apply as complete as the
    /// second.
    ///
    /// # Why the volume is written twice
    ///
    /// WirePlumber keeps a per-node `restore-stream` state
    /// (`~/.local/state/wireplumber/restore-stream`, keyed by `node.name`) and
    /// **restores** the saved `channelVolumes` when a node appears — asynchronously,
    /// a few milliseconds after the node shows up in the registry.  Our
    /// `set_param` therefore races with it, and the restore can overwrite the
    /// volume we just applied (measured on the sandbox: a stale
    /// `Holaf_Bus_…:channelVolumes=1.0;1.0` came back right after the snapshot
    /// had set 0.085).  Re-asserting once after the node has settled makes the
    /// strip's configured volume win, which is what the Python engine got for
    /// free by calling `pactl` after the node existed.
    fn sync_strip_volume(&mut self, strip: &Strip) {
        let master = self.names.master_node(strip);
        match self.session.bind_node_by_name(&master) {
            Ok(Some(node)) => {
                if let Err(e) = push_volume_mute_pod(&node, strip.volume, strip.mute) {
                    log::warn!("strip '{}': volume push failed: {}", strip.label, e.message);
                }
                // Flush the outbound set_param (delivered on mainloop iterate).
                self.session.roundtrip();
                self.session
                    .sleep_pumping(Duration::from_millis(VOLUME_SETTLE_MS));
                if let Err(e) = push_volume_mute_pod(&node, strip.volume, strip.mute) {
                    log::warn!("strip '{}': volume re-push failed: {}", strip.label, e.message);
                }
                self.session.roundtrip();
            }
            Ok(None) => {
                // Usually the same missing physical device the node phase
                // already reported; do not overwrite that explanation.
                let key = Self::strip_key(&strip.uid);
                if !self.pending.contains_key(&key) {
                    self.note_pending(
                        key,
                        format!("volume not applied: node '{master}' is not in the registry"),
                        Retry::Strip {
                            uid: strip.uid.clone(),
                        },
                    );
                }
            }
            Err(e) => {
                log::warn!("strip '{}': cannot bind volume node: {}", strip.label, e.message);
            }
        }
    }

    /// Reconcile the routing matrix against the wanted route set.
    ///
    /// Never fails: a route whose endpoints are not there yet is recorded as
    /// pending and retried (by the next apply, a toggle, or the automatic
    /// repair pass).
    fn reconcile_routes(&mut self, wanted: &HashSet<(String, String)>) {
        // Destroy links for routes that are no longer wanted.
        let stale: Vec<(String, String)> = self
            .routes
            .iter()
            .filter(|r| !wanted.contains(*r))
            .cloned()
            .collect();
        for (src, dst) in stale {
            self.unroute(&src, &dst);
            self.routes.remove(&(src.clone(), dst.clone()));
            self.clear_pending(&Self::route_key(&src, &dst));
        }
        // Sort so the creation order is deterministic (and the logs stable).
        let mut ordered: Vec<(String, String)> = wanted.iter().cloned().collect();
        ordered.sort();
        for (src, dst) in ordered {
            self.ensure_route(&src, &dst);
        }
    }

    /// (Re-)create one route and make sure its links really exist.
    ///
    /// Returns `true` when at least one link is live.  Every candidate endpoint
    /// pair is torn down first (see [`Engine::unroute`]) rather than trusting
    /// the route-set membership: the effective node of a strip changes when its
    /// FX chain is created/destroyed, so a link created under the previous
    /// topology would otherwise survive.
    fn ensure_route(&mut self, source_uid: &str, target_uid: &str) -> bool {
        self.unroute(source_uid, target_uid);
        let key = Self::route_key(source_uid, target_uid);
        // Book-keeping first: the route is *desired* from now on, whatever the
        // link creation outcome (a pending route must not be forgotten).
        self.routes
            .insert((source_uid.to_string(), target_uid.to_string()));
        match self.route(source_uid, target_uid) {
            Ok(report) if report.total() > 0 => {
                self.clear_pending(&key);
                true
            }
            Ok(_) => {
                self.note_pending(
                    key,
                    format!(
                        "route '{source_uid}' → '{target_uid}': no usable port pair (nodes present, 
                         but PipeWire published no matching ports)"
                    ),
                    Retry::Route {
                        source_uid: source_uid.to_string(),
                        target_uid: target_uid.to_string(),
                    },
                );
                false
            }
            Err(e) => {
                self.note_pending(
                    key,
                    format!("route '{source_uid}' → '{target_uid}': {}", e.message),
                    Retry::Route {
                        source_uid: source_uid.to_string(),
                        target_uid: target_uid.to_string(),
                    },
                );
                false
            }
        }
    }

    // ------------------------------------------------------------------
    // Chain lifecycle
    // ------------------------------------------------------------------

    /// Have `strip`'s filter-chain loaded **by the chain host** (a dedicated
    /// child process by default, see [`ChainHost`]), wait for its two streams,
    /// then let the verification pass wire the intra-strip link.
    fn create_chain(&mut self, strip: &Strip) -> Result<(), BackendError> {
        let uid = &strip.uid;
        let bus = self.bus_name(uid);
        let spec = ChainSpec::for_strip(strip, &bus)?;
        let flavour = ChainFlavour::of(strip);
        let fx_name = spec.fx_name.clone();
        self.host.load(uid, &spec.config)?;

        // Wait for both stream nodes to appear in the daemon registry.  This
        // works identically for both hosts: the nodes are exported to the
        // daemon by whichever process loaded the module.
        let in_name = graph::fx_input_node_name(uid);
        let out_name = graph::fx_output_node_name(uid);
        let in_node = self
            .session
            .wait_node(&in_name, NODE_POLL_ATTEMPTS * 2, NODE_POLL_DELAY_MS);
        let out_node = self
            .session
            .wait_node(&out_name, NODE_POLL_ATTEMPTS * 2, NODE_POLL_DELAY_MS);
        if in_node.is_none() || out_node.is_none() {
            log::warn!(
                "FX chain '{}' streams did not fully appear (in={}, out={}) — unloading",
                fx_name,
                in_node.is_some(),
                out_node.is_none()
            );
            self.host.unload(uid);
            self.settle(CHAIN_TEARDOWN_ROUNDTRIPS);
            return Err(BackendError::new(
                "fx_node_not_ready",
                format!("FX chain '{}' streams never became ready", strip.label),
            ));
        }

        log::info!(
            "created FX chain '{}' ({:?}) — {} / {}",
            fx_name,
            flavour,
            in_name,
            out_name
        );

        // Push the initial flat Props (the module loads with the graph's own
        // embedded controls; re-pushing is a harmless correctness step).
        let _ = self.push_chain_params(strip);

        // The intra-strip link is *not* created here: it belongs to the
        // verification pass ([`Engine::ensure_chain_links`]) so that it is
        // re-checked on every apply / toggle / repair instead of exactly once
        // (a link that could not be made at creation time was lost forever).
        Ok(())
    }

    /// Unload (destroy) every chain the host holds for us.
    fn unload_all_chains(&mut self) {
        let uids = self.host.uids();
        if uids.is_empty() {
            return;
        }
        self.host.unload_all();
        self.settle(CHAIN_TEARDOWN_ROUNDTRIPS);
    }

    /// Unload (destroy) the chain of `uid`, if any.
    fn unload_chain(&mut self, uid: &str) {
        if !self.host.contains(uid) {
            return;
        }
        self.host.unload(uid);
        // Give the host (or our own context) time to finish the teardown before
        // the caller rebuilds or links anything.
        self.settle(CHAIN_TEARDOWN_ROUNDTRIPS);
        log::info!("unloaded FX chain for strip '{uid}'");
    }

    /// Let the context process pending work for `roundtrips` round-trips while
    /// keeping the mainloop serviced (see [`PwSession::sleep_pumping`]).
    fn settle(&self, roundtrips: u32) {
        for _ in 0..roundtrips {
            self.session.roundtrip();
            self.session
                .sleep_pumping(Duration::from_millis(40));
        }
    }

    /// Are both stream nodes of `uid`'s chain still in the registry?
    ///
    /// The handles kept by the [`ChainHost`] are the only record of a chain we
    /// host, but the *nodes* they export can disappear underneath us (a sweep
    /// after a failed apply, an external `pw-cli destroy`, a module crash).
    /// A stale handle makes every later `push_chain_params` fail with
    /// `chain_gone` — measured: after one failed `apply_snapshot`, every later
    /// apply aborted there and the graph stayed dead.  One registry snapshot is
    /// cheap and lets the reconciler rebuild instead.
    fn chain_streams_live(&self, uid: &str) -> bool {
        let view = self.session.view();
        view.node_named(&graph::fx_input_node_name(uid)).is_some()
            && view.node_named(&graph::fx_output_node_name(uid)).is_some()
    }

    /// Push the flat `Props.params` of `strip`'s chain (hot-reload path).
    ///
    /// Two ways to reach the module's `input.Holaf_FX_<uid>` node, and the
    /// chain host decides which one applies:
    ///
    /// * **external host** — the module lives in another process, which owns
    ///   the node proxy; it applies the params itself
    ///   ([`ChainHost::set_params`]).
    /// * **in-process host** — we bind the node from *our* registry view and
    ///   `set_param` on it directly (the behaviour before the FX-host split).
    ///
    /// Either way the values reach the same node through the daemon; the
    /// difference is only which client writes them.
    fn push_chain_params(&mut self, strip: &Strip) -> Result<(), BackendError> {
        let avail = self.avail.clone();
        let uid = &strip.uid;
        let pairs = spa::control_pairs_with(strip, &avail);
        if pairs.is_empty() {
            return Ok(());
        }
        match self.host.set_params(uid, &pairs) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => {
                // The dedicated host no longer knows this chain: its stream
                // nodes are gone, so stop believing we host it and let the
                // reconciler rebuild (same self-healing as the in-process path).
                if e.code == "unknown_chain" || e.code == "chain_gone" {
                    // The chain's nodes are gone: drop the handle so the
                    // reconciler rebuilds, but keep wanting the chain.
                    self.host.mark_stale(uid);
                }
                return Err(e);
            }
        }
        let in_name = graph::fx_input_node_name(uid);
        let node = match self.session.bind_node_by_name(&in_name)? {
            Some(n) => n,
            None => {
                // Chain module gone (daemon restart).  Drop our stale handle so
                // the next activation rebuilds it.
                self.host.mark_stale(uid);
                return Err(BackendError::new(
                    "chain_gone",
                    format!("chain '{in_name}' not in registry"),
                ));
            }
        };
        push_params_pod(&node, &pairs)?;
        // Flush the outbound set_param (delivery happens on mainloop
        // iteration — without this the message would sit in the proxy queue
        // until the next roundtrip).
        self.session.roundtrip();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Volume / mute / mono
    // ------------------------------------------------------------------

    /// Name of the "master" node for volume / FX target of a **known** strip.
    ///
    /// Runtime orders (`set_volume`, `set_mute`) resolve the strip through the
    /// engine state; the *reconciler* never does (it derives the name from the
    /// strip it is holding plus the snapshot's naming plan, see
    /// [`graph::NodeNames::master_node`]) — looking it up here during the first
    /// apply was exactly the volume bug.
    fn strip_master_node_name(&self, uid: &str) -> Option<String> {
        self.strips.get(uid).map(|s| self.names.master_node(s))
    }

    fn set_volume(&mut self, uid: &str, value: f32) -> Result<JsonValue, BackendError> {
        let mute = {
            let strip = self
                .strips
                .get_mut(uid)
                .ok_or_else(|| unknown_strip(uid))?;
            strip.volume = value;
            strip.mute
        };
        let master = self.strip_master_node_name(uid);
        if let Some(name) = master {
            if let Some(node) = self.session.bind_node_by_name(&name)? {
                push_volume_mute_pod(&node, value, mute)?;
                self.session.roundtrip();
            }
        }
        Ok(json!({ "ok": true, "uid": uid, "value": value }))
    }

    fn set_mute(&mut self, uid: &str, muted: bool) -> Result<JsonValue, BackendError> {
        let volume = {
            let strip = self
                .strips
                .get_mut(uid)
                .ok_or_else(|| unknown_strip(uid))?;
            strip.mute = muted;
            strip.volume
        };
        let master = self.strip_master_node_name(uid);
        if let Some(name) = master {
            if let Some(node) = self.session.bind_node_by_name(&name)? {
                push_volume_mute_pod(&node, volume, muted)?;
                self.session.roundtrip();
            }
        }
        Ok(json!({ "ok": true, "uid": uid, "muted": muted }))
    }

    fn set_mono(&mut self, uid: &str, mono: bool) -> Result<JsonValue, BackendError> {
        {
            let strip = self
                .strips
                .get_mut(uid)
                .ok_or_else(|| unknown_strip(uid))?;
            if strip.is_mono == mono {
                return Ok(json!({ "ok": true, "uid": uid, "mono": mono, "changed": false }));
            }
            strip.is_mono = mono;
        }
        // Re-link every route where this strip is the source with the new
        // mono policy (Python set_mono refreshes the outgoing links).
        let targets: Vec<String> = self
            .routes
            .iter()
            .filter(|(s, _)| s == uid)
            .map(|(_, t)| t.clone())
            .collect();
        for target in targets {
            self.ensure_route(uid, &target);
        }
        Ok(json!({ "ok": true, "uid": uid, "mono": mono, "changed": true }))
    }

    // ------------------------------------------------------------------
    // FX toggles / params (hot-reload path)
    // ------------------------------------------------------------------

    fn fx_set_active(
        &mut self,
        uid: &str,
        effect: EffectKey,
        active: bool,
    ) -> Result<JsonValue, BackendError> {
        let strip = self
            .strips
            .get_mut(uid)
            .ok_or_else(|| unknown_strip(uid))?;
        let state = strip.effects.get_mut(&effect).ok_or_else(|| {
            BackendError::new(
                "unknown_effect",
                format!("strip '{uid}' has no effect '{}'", effect.as_str()),
            )
        })?;
        state.active = active;
        let clone = strip.clone();

        let want_chain = graph::wants_fx_chain(&clone, &self.avail);
        let chain_exists = self.host.contains(uid);
        if want_chain && !chain_exists {
            // First activation → build the chain (the intra-strip link is made
            // by `ensure_chain_links` below, which also reports a deferred
            // chain instead of failing the order).
            if let Err(e) = self.create_chain(&clone) {
                self.note_pending(
                    Self::strip_key(uid),
                    e.message,
                    Retry::Strip {
                        uid: uid.to_string(),
                    },
                );
            }
        } else if chain_exists {
            // Chain already exists (kept after a previous disable): hot-reload
            // the flat Props with the new effective controls (bypass for the
            // freshly-disabled effect, real values for the enabled one).
            if let Err(e) = self.push_chain_params(&clone) {
                if e.code == "chain_gone" {
                    // Chain vanished underneath us (`push_chain_params` already
                    // dropped the stale handle): rebuild from scratch.
                    if let Err(e) = self.create_chain(&clone) {
                        self.note_pending(
                            Self::strip_key(uid),
                            e.message,
                            Retry::Strip {
                                uid: uid.to_string(),
                            },
                        );
                    }
                } else {
                    return Err(e);
                }
            }
        }
        // When !want_chain and no chain exists: nothing to do (no audible
        // effect either) — the state is stored for a future activation.

        // Re-derive the topology **unconditionally**, not only when a chain was
        // just created:
        //
        // - a brand-new chain moves the endpoints of every route this strip is
        //   part of (`Holaf_Bus_<Label>` → `output.Holaf_FX_<uid>` / …);
        // - and a toggle is the user's only lever on a graph that was built
        //   wrong at startup (chain created while its target was absent, links
        //   dropped by a sweep, ports that appeared late).  It used to do
        //   *nothing* when the chain already existed (`push_chain_params`
        //   only), which is why "if it is broken it stays broken".
        let wanted = self.routes.clone();
        self.reconcile_routes(&wanted);
        if let Some(strip) = self.strips.get(uid).cloned() {
            self.ensure_chain_links(&strip);
            self.sync_strip_volume(&strip);
        }

        let state_json = serde_json::to_value(&self.strips[uid].effects[&effect])
            .map_err(|e| BackendError::internal("fx serialization", e))?;
        Ok(state_json)
    }

    fn fx_set_params(
        &mut self,
        uid: &str,
        effect: EffectKey,
        params: &IndexMap<String, f32>,
    ) -> Result<JsonValue, BackendError> {
        let strip = self
            .strips
            .get_mut(uid)
            .ok_or_else(|| unknown_strip(uid))?;
        let state = strip.effects.get_mut(&effect).ok_or_else(|| {
            BackendError::new(
                "unknown_effect",
                format!("strip '{uid}' has no effect '{}'", effect.as_str()),
            )
        })?;
        for (port, value) in params {
            state.params.insert(port.clone(), *value);
        }
        let clone = strip.clone();

        // Hot-reload only makes sense on a live chain; without one the params
        // are stored and applied at the next chain creation/activation.
        if self.host.contains(uid) {
            self.push_chain_params(&clone)?;
        }
        let state_json = serde_json::to_value(&self.strips[uid].effects[&effect])
            .map_err(|e| BackendError::internal("fx serialization", e))?;
        Ok(state_json)
    }

    // ------------------------------------------------------------------
    // Routing
    // ------------------------------------------------------------------

    /// Effective *source* node name of a strip: chain output if the strip has
    /// an FX chain, else its master node (device source / null-sink).
    fn effective_source_name(&self, strip: &Strip) -> Option<String> {
        let uid = &strip.uid;
        if self.host.contains(uid) {
            return Some(graph::fx_output_node_name(uid));
        }
        Some(self.names.master_node(strip))
    }

    /// Effective *target* sink node name of a strip: chain capture sink if the
    /// strip has an FX chain, else its master node (device sink / null-sink).
    fn effective_target_name(&self, strip: &Strip) -> Option<String> {
        let uid = &strip.uid;
        if self.host.contains(uid) {
            return Some(graph::fx_input_node_name(uid));
        }
        Some(self.names.master_node(strip))
    }


    /// Create the audio links between a source node and a sink node (FL→FL,
    /// FR→FR; mono → FL duplicated on both, exactly like the Python
    /// `_auto_link_ports(force_mono=True)`), skipping links that already exist
    /// (anti-WirePlumber duplication, like the spike).
    ///
    /// Returns how many links are live afterwards; a `total()` of 0 means the
    /// nodes exist but PipeWire published no usable port pair, and the caller
    /// records the link as pending instead of assuming it worked.
    fn link_nodes(
        &mut self,
        source_name: &str,
        target_name: &str,
        mono: bool,
    ) -> Result<LinkReport, BackendError> {
        let source = self
            .session
            .wait_node(source_name, LINK_NODE_ATTEMPTS, LINK_RETRY_DELAY_MS);
        let target = self
            .session
            .wait_node(target_name, LINK_NODE_ATTEMPTS, LINK_RETRY_DELAY_MS);
        let (Some(source), Some(target)) = (source, target) else {
            return Err(BackendError::new(
                "link_nodes_missing",
                format!("cannot link '{source_name}' → '{target_name}': node not in registry"),
            ));
        };

        for _ in 0..LINK_RETRIES {
            let view = self.session.view();
            let src_fl = view.port_fl(source.id, PortDir::Out);
            let src_fr = view.port_fr(source.id, PortDir::Out);
            let dst_fl = view.port_fl(target.id, PortDir::In);
            let dst_fr = view.port_fr(target.id, PortDir::In);
            drop(view);

            let pairs: Vec<(u32, u32)> = if mono {
                // Mono source: the left channel feeds **both** destination
                // channels — never src_r as well (PipeWire SUMS linked inputs,
                // which would add +6 dB on a correlated signal).
                match (src_fl, dst_fl, dst_fr) {
                    (Some(s), Some(dl), Some(dr)) => vec![(s, dl), (s, dr)],
                    (Some(s), Some(dl), None) => vec![(s, dl)],
                    _ => Vec::new(),
                }
            } else {
                // Stereo: strict FL→FL / FR→FR.  The single-port fallbacks keep
                // a genuinely mono node usable (one link, no duplication).
                match (src_fl, src_fr, dst_fl, dst_fr) {
                    (Some(sl), Some(sr), Some(dl), Some(dr)) => vec![(sl, dl), (sr, dr)],
                    (Some(sl), None, Some(dl), None) => vec![(sl, dl)],
                    (None, Some(sr), None, Some(dr)) => vec![(sr, dr)],
                    _ => Vec::new(),
                }
            };

            if pairs.is_empty() {
                self.session
                    .sleep_pumping(Duration::from_millis(LINK_RETRY_DELAY_MS));
                continue;
            }

            let view = self.session.view();
            let mut created = 0usize;
            let mut existing = 0usize;
            for (out_port, in_port) in &pairs {
                if view.link_exists(*out_port, *in_port) {
                    existing += 1;
                    continue;
                }
                let link = self.session.core.create_object::<pw::link::Link>(
                    "link-factory",
                    &pw::properties::properties! {
                        "link.output.node" => source.id.to_string(),
                        "link.output.port" => out_port.to_string(),
                        "link.input.node" => target.id.to_string(),
                        "link.input.port" => in_port.to_string(),
                        "object.linger" => "1",
                    },
                );
                match link {
                    Err(e) => log::warn!("create link {source_name}→{target_name} failed: {e}"),
                    Ok(link) => {
                        // Keep the owning proxy alive: dropping it would
                        // destroy the link we just created.
                        self.session.created_links.borrow_mut().push(link);
                        created += 1;
                    }
                }
            }
            self.session.roundtrip();
            log::info!(
                "link '{source_name}' → '{target_name}' ({created} created, {existing} already present)"
            );
            return Ok(LinkReport { created, existing });
        }
        // Exhausted the retries without ever seeing a usable port pair: the
        // nodes exist but PipeWire never published their ports (observed after
        // many create/destroy cycles on a long-lived daemon — run the live tests
        // right after a headless restart).
        log::warn!(
            "cannot link '{source_name}' → '{target_name}': no usable port pair after \
             {LINK_RETRIES} attempts — link NOT created (kept pending)"
        );
        Ok(LinkReport::default())
    }

    /// Remove every live link between two node names (used for un-routing).
    fn unlink_nodes(&mut self, source_name: &str, target_name: &str) {
        let view = self.session.view();
        let (Some(source), Some(target)) = (view.node_named(source_name), view.node_named(target_name)) else {
            return;
        };
        let to_destroy: Vec<u32> = view
            .links_of_node(source.id)
            .filter(|l| l.in_node == target.id || l.out_node == target.id)
            .map(|l| l.id)
            .collect();
        drop(view);
        for id in to_destroy {
            self.session.destroy_global(id);
        }
        self.session.roundtrip();
    }

    fn route(&mut self, source_uid: &str, target_uid: &str) -> Result<LinkReport, BackendError> {
        let Some(source_strip) = self.strips.get(source_uid).cloned() else {
            return Err(unknown_strip(source_uid));
        };
        let Some(target_strip) = self.strips.get(target_uid).cloned() else {
            return Err(unknown_strip(target_uid));
        };
        let (Some(src), Some(dst)) = (
            self.effective_source_name(&source_strip),
            self.effective_target_name(&target_strip),
        ) else {
            return Ok(LinkReport::default());
        };
        self.link_nodes(&src, &dst, source_strip.is_mono)
    }

    /// Unlink an existing route (does **not** touch the route-set bookkeeping;
    /// callers decide membership).
    ///
    /// Every candidate endpoint pair is torn down, not just the currently
    /// effective one: a chain created (or unloaded) while the route stays moves
    /// the strip's endpoint to/from `output.Holaf_FX_<uid>` /
    /// `input.Holaf_FX_<uid>`, and the link created under the previous topology
    /// would otherwise survive — the signal would then travel through *both*
    /// paths (measured: doubled level, +5 dB, plus comb filtering).
    fn unroute(&mut self, source_uid: &str, target_uid: &str) {
        let Some(source_strip) = self.strips.get(source_uid).cloned() else {
            return;
        };
        let Some(target_strip) = self.strips.get(target_uid).cloned() else {
            return;
        };
        for src in self.names.route_source_candidates(&source_strip) {
            for dst in self.names.route_target_candidates(&target_strip) {
                self.unlink_nodes(&src, &dst);
            }
        }
    }

    fn routing_set(
        &mut self,
        source_uid: &str,
        target_uid: &str,
        active: bool,
    ) -> Result<JsonValue, BackendError> {
        if active {
            // Always (re-)derive the link: the effective endpoints of a strip
            // follow its FX chain state, so "already in the route set" does not
            // mean "the link is there".
            self.ensure_route(source_uid, target_uid);
        } else {
            self.unroute(source_uid, target_uid);
            self.routes.remove(&(source_uid.to_string(), target_uid.to_string()));
            self.clear_pending(&Self::route_key(source_uid, target_uid));
        }
        Ok(json!({
            "ok": true, "source_uid": source_uid, "target_uid": target_uid, "active": active,
        }))
    }

    fn routing_list_json(&self) -> JsonValue {
        let routes: Vec<JsonValue> = self
            .routes
            .iter()
            .map(|(s, t)| json!({"source_uid": s, "target_uid": t, "active": true}))
            .collect();
        json!({ "routes": routes })
    }

    // ------------------------------------------------------------------
    // Default sink / devices / status
    // ------------------------------------------------------------------

    fn set_default_sink(&mut self, uid: &str) -> Result<JsonValue, BackendError> {
        let strip = self
            .strips
            .get(uid)
            .ok_or_else(|| unknown_strip(uid))?
            .clone();
        let node_name = if graph::is_physical(&strip) {
            strip.device_name.clone()
        } else {
            Some(self.bus_name(uid))
        };
        let Some(node_name) = node_name else {
            return Err(BackendError::new("no_node", format!("strip '{uid}' has no node")));
        };
        let Some(metadata) = self.session.bind_default_metadata()? else {
            return Err(BackendError::new(
                "no_metadata",
                "the daemon has no 'default' metadata object (is WirePlumber running?)",
            ));
        };
        let value = format!(r#"{{ "name": "{node_name}" }}"#);
        metadata.set_property(0, "default.audio.sink", Some("Spa:String:JSON"), Some(&value));
        self.session.roundtrip();
        self.default_sink = Some(uid.to_string());
        log::info!("system default sink → '{}' ({node_name})", strip.label);
        Ok(json!({ "ok": true, "uid": uid, "node_name": node_name }))
    }

    fn device_list(&mut self) -> Result<JsonValue, BackendError> {
        let view = self.session.view();
        let devices: Vec<JsonValue> = view
            .nodes()
            .iter()
            .filter(|n| {
                matches!(n.media_class.as_str(), "Audio/Sink" | "Audio/Source")
                    && !is_holaf_node(n)
            })
            .map(|n| {
                json!({
                    "id": n.id,
                    "name": n.name,
                    "media_class": n.media_class,
                })
            })
            .collect();
        Ok(json!({ "devices": devices }))
    }

    fn status(&mut self) -> Result<JsonValue, BackendError> {
        // Every strip plus its **public handle**: `node_name` is the bus node
        // other programs target (`Holaf_Bus_<Label>`, see
        // [`graph::bus_node_name`]).  Clients (the Qt UI moving an app onto a
        // strip, scripts, the user's own tooling) must use this field rather
        // than rebuilding a name from the uid — the name no longer contains it.
        let strips: Vec<JsonValue> = self
            .strips
            .values()
            .map(|strip| {
                let mut value = serde_json::to_value(strip)
                    .map_err(|e| BackendError::internal("status", e))?;
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("node_name".into(), json!(self.bus_name(&strip.uid)));
                }
                Ok(value)
            })
            .collect::<Result<_, _>>()?;

        // Live Holaf_* node names (external verification / debugging).
        let view = self.session.view();
        let mut holaf_nodes: Vec<&str> = view
            .nodes()
            .iter()
            .filter(|n| is_holaf_node(n))
            .map(|n| n.name.as_str())
            .collect();
        holaf_nodes.sort();

        let chains: Vec<JsonValue> = self
            .strips
            .values()
            .map(|s| {
                let has_chain = self.host.contains(&s.uid);
                json!({ "uid": s.uid, "chain": has_chain })
            })
            .collect();

        let routes = self.routing_list_json();
        let meter_sources = self.meter_sources_payload();
        Ok(json!({
            "running": self.running,
            "strips": strips,
            "routes": routes["routes"],
            "apps": [],
            "default_sink": self.default_sink,
            "holaf_nodes": holaf_nodes,
            "fx_chains": chains,
            "meter_sources": meter_sources,
            // Idle mainloop service (see `idle_pumps`): lets a live test — or
            // a user debugging a crackling chain — prove the daemon is really
            // servicing the modules it hosts.
            "mainloop_pumps": self.idle_pumps,
            // Who hosts the FX chains.  `external` means a dedicated child
            // process (`holafd --fx-host`) is doing nothing but running its
            // PipeWire mainloop — the `pw-cli` shape the Python engine proved.
            // `inprocess` means the chains live in this process (legacy, and
            // the automatic fallback when the child cannot be spawned).
            "fx_host": {
                "mode": self.host.mode().name(),
                "pid": self.host.pid(),
                "respawns": self.host.respawns(),
                "chains": self.host.uids(),
            },
            // Incomplete graph work (missing device / unlinked route / chain
            // that could not be built).  `degraded: true` explains a graph
            // that sounds wrong without anything being logged as an error, and
            // `pending` names exactly what the daemon is still retrying.
            "degraded": !self.pending.is_empty(),
            "pending": self.pending_json(),
        }))
    }

    /// `{uid: source_name}` for the Python VU-meter engine (spec §6).
    ///
    /// The naming rule lives in [`graph::meter_source_name`] (pure, unit
    /// tested): device source for physical inputs, `<node>.monitor` otherwise.
    /// It does not depend on the chain state, but the *existence* of the
    /// underlying sink does — hence callers re-query after a snapshot rebuild.
    fn meter_sources_payload(&self) -> JsonValue {
        let mut sources = serde_json::Map::new();
        for strip in self.strips.values() {
            sources.insert(
                strip.uid.clone(),
                JsonValue::String(self.names.meter_source(strip)),
            );
        }
        sources.into()
    }

    /// Full `meter.sources` reply: the map plus the running flag.
    fn meter_sources_json(&self) -> JsonValue {
        json!({
            "running": self.running,
            "sources": self.meter_sources_payload(),
        })
    }

    // ------------------------------------------------------------------
    // Teardown / cleanup
    // ------------------------------------------------------------------

    /// Remove one strip from the graph and from the engine state.
    ///
    /// The bus is looked up under the name the *current* plan gave it (this runs
    /// before the plan of the new snapshot is computed, so that name is the one
    /// the node was published under), with the historical
    /// `Holaf_Strip_<uid>` as a second net for a node left by an older build.
    /// Missing it here would orphan a bus forever — and only
    /// [`Engine::sweep_stale_buses`] would catch it, at the next apply.
    fn teardown_strip(&mut self, uid: &str) {
        self.unload_chain(uid);
        let planned = self.bus_name(uid);
        let legacy = graph::legacy_strip_node_name(uid);
        let node_and_links = {
            let view = self.session.view();
            let node = view
                .node_named(&planned)
                .or_else(|| view.node_named(&legacy));
            match node {
                Some(node) => {
                    let links: Vec<u32> = view.links_of_node(node.id).map(|l| l.id).collect();
                    Some((node.id, links))
                }
                None => None,
            }
        };
        if let Some((node_id, links)) = node_and_links {
            for id in links {
                self.session.destroy_global(id);
            }
            self.session.destroy_global(node_id);
            self.session.roundtrip();
        }
        self.strips.shift_remove(uid);
        // The plan must forget it too: the uid may be recycled by a brand new
        // strip, which then starts from a fresh name.
        self.names = graph::NodeNames::plan(self.strips.values(), &self.names);
        self.routes.retain(|(s, t)| s != uid && t != uid);
        // Do not keep retrying work that belongs to a strip nobody wants.
        let key = Self::strip_key(uid);
        self.pending.shift_remove(&key);
        let orphan_routes: Vec<String> = self
            .pending
            .values()
            .filter(|p| match &p.retry {
                Retry::Route {
                    source_uid,
                    target_uid,
                } => source_uid == uid || target_uid == uid,
                Retry::Strip { .. } => false,
            })
            .map(|p| p.key.clone())
            .collect();
        for key in orphan_routes {
            self.pending.shift_remove(&key);
        }
    }

    fn teardown_all(&mut self) {
        self.unload_all_chains();
        let view = self.session.view();
        let nodes: Vec<u32> = view
            .nodes()
            .iter()
            .filter(|n| is_holaf_node(n))
            .map(|n| n.id)
            .collect();
        drop(view);
        for id in nodes {
            self.session.destroy_global(id);
        }
        for _ in 0..CHAIN_TEARDOWN_ROUNDTRIPS {
            self.session.roundtrip();
            self.session.sleep_pumping(Duration::from_millis(40));
        }
        self.strips.clear();
        self.names = graph::NodeNames::default();
        self.routes.clear();
        self.pending.clear();
        self.repair_attempts = 0;
        self.default_sink = None;
        log::info!("engine teardown complete");
    }

    /// Destroy every live node of ours (debris from a previous crashed run) —
    /// mirrors Python `_clean_zombie_nodes`.
    ///
    /// Two detections, so a rename can never hide a node: the authoritative
    /// `holaf.uid` marker (see [`graph::UID_PROP`]) and the historical `Holaf_`
    /// name prefix (which also catches nodes created by an older build, or by
    /// the legacy Python engine, before the marker existed).
    fn sweep_zombies(&mut self) {
        let view = self.session.view();
        let ids: Vec<u32> = view
            .nodes()
            .iter()
            .filter(|n| is_holaf_node(n))
            .map(|n| n.id)
            .collect();
        drop(view);
        if ids.is_empty() {
            return;
        }
        log::info!("sweeping {} zombie Holaf node(s)", ids.len());
        for id in ids {
            self.session.destroy_global(id);
        }
        for _ in 0..CHAIN_TEARDOWN_ROUNDTRIPS {
            self.session.roundtrip();
            self.session.sleep_pumping(Duration::from_millis(40));
        }
    }
}

// ---------------------------------------------------------------------------
// Pure/static helpers
// ---------------------------------------------------------------------------

/// Everything needed to load one strip's filter-chain module.
///
/// Built by [`ChainSpec::for_strip`] and consumed by [`Engine::create_chain`]
/// (in-process hosting) as well as by the `holaf-fxcfg` diagnostic binary and
/// the external `--fx-host` process (`holafd` ships it over the NDJSON
/// protocol, so both sides share one single source of truth for the SPA-JSON).
pub struct ChainSpec {
    /// Base node name, `Holaf_FX_<uid>`.
    pub fx_name: String,
    /// Chain capture stream node, `input.Holaf_FX_<uid>`.
    pub input_node: String,
    /// Chain playback stream node, `output.Holaf_FX_<uid>`.
    pub output_node: String,
    /// Module argument string (`filter.graph` + `capture.props` +
    /// `playback.props`), ready for `pw_context_load_module`.
    pub config: String,
}

impl ChainSpec {
    /// Build the chain spec of `strip`, or `fx_graph_empty` when none of the
    /// active effects has an installed plugin (the Python engine behaviour:
    /// no usable plugin → no chain).
    pub fn for_strip(strip: &Strip, bus: &str) -> Result<ChainSpec, BackendError> {
        let graph = spa::build_graph(strip, true);
        if graph.is_empty() {
            return Err(BackendError::new(
                "fx_graph_empty",
                format!(
                    "cannot build FX chain for '{}': no plugin available",
                    strip.label
                ),
            ));
        }
        let is_output = matches!(ChainFlavour::of(strip), ChainFlavour::Output);
        let fx_name = graph::fx_node_name(&strip.uid);
        let description = sanitize_description(&strip.label);
        let target = chain_target_object(strip, bus);
        let config = spa_config::build_filter_chain_config(
            &fx_name,
            &format!("Holaf FX {description}"),
            &graph,
            is_output,
            target.as_deref(),
        );
        Ok(ChainSpec {
            fx_name,
            input_node: graph::fx_input_node_name(&strip.uid),
            output_node: graph::fx_output_node_name(&strip.uid),
            config,
        })
    }
}

/// What `target.object` a chain's playback/capture should point at: the
/// strip's master node (device for a physical strip, null-sink otherwise) — an
/// output chain's playback stream drives it, an input chain's capture stream
/// reads it.
fn chain_target_object(strip: &Strip, bus: &str) -> Option<String> {
    Some(graph::master_node_name(strip, bus))
}

/// Sanitize a user label into an SPA-safe description (alnum + space only).
fn sanitize_description(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for c in label.chars() {
        if c.is_alphanumeric() || c == ' ' {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    let trimmed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        "strip".to_string()
    } else {
        trimmed
    }
}

fn unknown_strip(uid: &str) -> BackendError {
    BackendError::new("unknown_strip", format!("no strip with uid '{uid}'"))
}

/// Is `node` one of the nodes this engine owns (and may destroy)?
///
/// The `holaf.uid` marker ([`graph::UID_PROP`]) is authoritative — it is the
/// only identity that survives a label change.  The name prefixes are kept as a
/// second net, because nodes created by an older build (or by the legacy Python
/// engine) carry no marker at all and must still be swept.
fn is_holaf_node(node: &LiveNode) -> bool {
    node.is_ours()
        || node.name.contains("Holaf_")
        || node.name.starts_with("input.Holaf_FX_")
        || node.name.starts_with("output.Holaf_FX_")
}

fn app_list_stub() -> JsonValue {
    json!({
        "apps": [],
        "stub": true,
        "note": "sink-input management is not implemented in J2 — no per-application controls yet",
    })
}

fn app_move_stub() -> JsonValue {
    json!({
        "ok": false,
        "stub": true,
        "note": "sink-input management is not implemented in J2 — app.move is a documented stub",
    })
}

/// Serialize the flat `Props.params` struct of the filter-chain to a Pod and
/// `set_param(Props)` on a node proxy (the J1-corrected hot-reload path).
pub(crate) fn push_params_pod(
    node: &pw::node::Node,
    pairs: &[(String, f32)],
) -> Result<(), BackendError> {
    let mut struct_values: Vec<Value> = Vec::with_capacity(pairs.len() * 2);
    for (name, value) in pairs {
        struct_values.push(Value::String(name.clone()));
        struct_values.push(Value::Float(*value));
    }
    let object = Value::Object(pw::spa::pod::Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_Props,
        id: pw::spa::sys::SPA_PARAM_Props,
        properties: vec![pw::spa::pod::Property::new(
            pw::spa::sys::SPA_PROP_params,
            Value::Struct(struct_values),
        )],
    });
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .map_err(|e| BackendError::internal("PodSerializer", e))?
        .0
        .into_inner();
    let pod: &Pod = Pod::from_bytes(&bytes).ok_or_else(|| {
        BackendError::new("pod_build", "built an invalid Props Pod for the filter-chain")
    })?;
    node.set_param(pw::spa::param::ParamType::Props, 0, pod);
    Ok(())
}

/// Set volume + mute on a node via the `Props` param (`channelVolumes` array +
/// `mute`), with a softMute fallback when `mute` is rejected by the node.
///
/// # Volume semantics (Python parity)
///
/// `channelVolumes` is a **linear** multiplier array, while a strip's volume is
/// the *percentage* the UI shows and the Python engine applies with
/// `pactl set-sink-volume <node> <int(volume*100)>%` — PulseAudio's cubic curve.
/// The conversion therefore goes through [`holaf_core::model::pulse_linear_gain`]
/// (`0.44` → `0.085`), otherwise every strip plays **+14 dB** louder than with
/// the Python engine (measured bug report: "much louder, VU meters pinned").
///
/// # Why `monitorVolumes` / `monitorMute` are set too
///
/// The monitor tap of a sink (`<node>.monitor`, what the Python VU-meter engine
/// records) has its **own** gain (`Props:monitorVolumes`).  The Python engine
/// sets it explicitly (`pactl set-source-volume <monitor> <pct>%` and
/// `set-source-mute`), so leaving it at unity makes every VU bar read the
/// pre-fader level: "both output strips' meters are pinned while the sliders
/// are low" — the other half of the same bug report.
fn push_volume_mute_pod(node: &pw::node::Node, volume: f32, mute: bool) -> Result<(), BackendError> {
    let gain = holaf_core::model::pulse_linear_gain(volume);
    let object = Value::Object(pw::spa::pod::Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_Props,
        id: pw::spa::sys::SPA_PARAM_Props,
        properties: vec![
            pw::spa::pod::Property::new(
                pw::spa::sys::SPA_PROP_channelVolumes,
                Value::ValueArray(pw::spa::pod::ValueArray::Float(vec![gain; 2])),
            ),
            pw::spa::pod::Property::new(
                pw::spa::sys::SPA_PROP_mute,
                Value::Bool(mute),
            ),
            pw::spa::pod::Property::new(
                pw::spa::sys::SPA_PROP_monitorVolumes,
                Value::ValueArray(pw::spa::pod::ValueArray::Float(vec![gain; 2])),
            ),
            pw::spa::pod::Property::new(
                pw::spa::sys::SPA_PROP_monitorMute,
                Value::Bool(mute),
            ),
        ],
    });
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .map_err(|e| BackendError::internal("PodSerializer (volume)", e))?
        .0
        .into_inner();
    let pod: &Pod = Pod::from_bytes(&bytes).ok_or_else(|| {
        BackendError::new("pod_build", "built an invalid Props Pod for volume")
    })?;
    node.set_param(pw::spa::param::ParamType::Props, 0, pod);
    Ok(())
}

/// The engine operations the worker loop executes (mirrors the `Backend`
/// trait surface).
#[derive(Debug, Clone)]
pub enum EngineOp {
    ApplySnapshot(Vec<Strip>),
    Stop,
    Status,
    SetVolume { uid: String, value: f32 },
    SetMute { uid: String, muted: bool },
    SetMono { uid: String, mono: bool },
    FxSetActive { uid: String, effect: EffectKey, active: bool },
    FxSetParams {
        uid: String,
        effect: EffectKey,
        params: IndexMap<String, f32>,
    },
    RoutingSet {
        source_uid: String,
        target_uid: String,
        active: bool,
    },
    RoutingList,
    SetDefaultSink { uid: String },
    DeviceList,
    AppList,
    AppMove { app_id: String, sink_name: String },
    MeterSources,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repair schedule must start fast (a device that appears right after
    /// the snapshot is fixed within ~200 ms) and then back off so a device that
    /// stays unplugged costs nothing.
    #[test]
    fn repair_backoff_starts_fast_and_saturates() {
        assert_eq!(repair_backoff(1), Duration::from_millis(200));
        assert_eq!(repair_backoff(2), Duration::from_millis(400));
        assert_eq!(repair_backoff(3), Duration::from_millis(800));
        // Monotonic, and capped at the last step forever (never grows without
        // bound, never panics on a huge attempt counter).
        let mut previous = Duration::ZERO;
        for attempts in 1..20 {
            let d = repair_backoff(attempts);
            assert!(d >= previous, "backoff decreased at attempt {attempts}");
            assert!(d <= Duration::from_millis(5_000));
            previous = d;
        }
        assert_eq!(repair_backoff(u32::MAX), Duration::from_millis(5_000));
        // Attempt 0 is only reachable defensively (first schedule) and must not
        // underflow.
        assert_eq!(repair_backoff(0), Duration::from_millis(200));
    }

    /// Pending keys are the identity of a repair item: a strip and a route never
    /// collide, and the key of a route names both ends.
    #[test]
    fn pending_keys_are_stable_and_distinct() {
        assert_eq!(Engine::strip_key("abc"), "strip:abc");
        assert_eq!(Engine::route_key("a", "b"), "route:a->b");
        assert_ne!(Engine::route_key("a", "b"), Engine::route_key("b", "a"));
        assert_ne!(Engine::strip_key("a"), Engine::route_key("a", "a"));
    }

    /// A `LinkReport` of 0/0 means "nothing is wired" — the reconciler must
    /// treat that as a failure, not as success (the bug: a route was recorded
    /// as created even when PipeWire had published no port pair).
    #[test]
    fn link_report_counts_existing_links_as_success() {
        assert_eq!(LinkReport::default().total(), 0);
        assert_eq!(LinkReport { created: 0, existing: 2 }.total(), 2);
        assert_eq!(LinkReport { created: 2, existing: 0 }.total(), 2);
        assert_eq!(LinkReport { created: 1, existing: 1 }.total(), 2);
    }

    /// A physical strip with an empty `device_name` must resolve to its bus,
    /// never to an empty node name (the old code produced `""` and every link
    /// attempt on it failed).
    #[test]
    fn chain_anchor_never_resolves_to_an_empty_name() {
        let mut strip = Strip::with_uid("x", "Casque", holaf_core::model::StripType::Output);
        strip.device_name = Some(String::new());
        let bus = graph::bus_node_name(&strip.label, 1);
        assert_eq!(
            chain_target_object(&strip, &bus),
            Some("Holaf_Bus_Casque".to_string())
        );
        strip.device_name = Some("alsa_output.usb".into());
        assert_eq!(
            chain_target_object(&strip, &bus),
            Some("alsa_output.usb".to_string())
        );
    }
}
