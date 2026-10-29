//! The real PipeWire engine: the object graph + reconciler that implements
//! the [`Backend`] operations against a live daemon.
//!
//! # Threading
//!
//! PipeWire's mainloop/context/core are `!Send` (they use `Rc` internally), so
//! this whole engine lives on a **dedicated worker thread** and is driven by
//! the `PipeWireBackend` handle (`backend.rs`) through an mpsc channel.  Each
//! operation is processed synchronously: the worker runs its mainloop only
//! when a round-trip / registry poll is needed, mirroring the J1 spike.
//!
//! # Reconciliation model
//!
//! `apply_snapshot` is a full-diff sync of the *desired* strips:
//!
//! - strips that disappeared are torn down (chain unloaded, null-sink
//!   destroyed, links removed);
//! - for each strip: a null-sink node is created for virtual strips (physical
//!   strips address the existing device node by `device_name`), a filter-chain
//!   is loaded **iff** [`graph::wants_fx_chain`] (else an existing one is
//!   unloaded — "destroy if obsolete"), volume/mute are pushed;
//! - routes (input strip `.routes`) are then reconciled link-by-link with an
//!   existence check (never duplicating a WirePlumber-created link).
//!
//! Runtime FX toggles use hot `set-param` when the chain exists; once created
//! a chain is **kept** with neutral BYPASS values when the last effect is
//! disabled (the Python behaviour) — only a full `apply_snapshot` removes a
//! chain that no longer wants any effect.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use indexmap::IndexMap;
use pipewire as pw;
use pipewire::properties::Properties;
use pipewire::registry::GlobalObject;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Pod, Value};
use serde_json::{json, Value as JsonValue};

use holaf_core::backend::BackendError;
use holaf_core::graph::{self, ChainFlavour};
use holaf_core::model::{EffectKey, Strip};
use holaf_core::spa;
use holaf_core::fx::PluginSet;

use crate::live::{collect_view, LiveNode, LiveView, PortDir};
use crate::spa_config;
use crate::sys::{self, LocalModule};

/// Polling knobs (same order of magnitude as the Python audio_engine).
const NODE_POLL_ATTEMPTS: u32 = 40;
const NODE_POLL_DELAY_MS: u64 = 50;
const LINK_RETRIES: u32 = 5;
const LINK_RETRY_DELAY_MS: u64 = 100;
const CHAIN_TEARDOWN_ROUNDTRIPS: u32 = 4;

/// A connected PipeWire session owned by the worker thread.
struct PwSession {
    _mainloop: pw::main_loop::MainLoop,
    context: pw::context::Context,
    core: pw::core::Core,
}

/// Owned registry snapshot type (matches the spike).
type OwnedGlobal = GlobalObject<Properties>;

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
            thread::sleep(Duration::from_millis(delay_ms));
        }
        None
    }

    fn create_null_sink(&self, name: &str, description: &str) -> Result<(), BackendError> {
        let _node = self
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
                },
            )
            .map_err(|e| BackendError::internal("create null sink", e))?;
        if self.wait_node(name, NODE_POLL_ATTEMPTS, NODE_POLL_DELAY_MS).is_none() {
            return Err(BackendError::new(
                "node_not_ready",
                format!("null-sink '{name}' never appeared in the registry"),
            ));
        }
        log::info!("created null-sink '{name}'");
        Ok(())
    }

    fn destroy_global(&self, id: u32) {
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

/// The reconciler + runtime state, owned by the worker thread.
pub struct Engine {
    session: PwSession,
    avail: PluginSet,
    /// Desired strips (post-migration), keyed by uid — for `status` and diffs.
    strips: IndexMap<String, Strip>,
    /// Locally-hosted filter-chain modules, one per strip uid.
    modules: IndexMap<String, LocalModule>,
    /// Active routes `(source_uid, target_uid)`.
    routes: HashSet<(String, String)>,
    default_sink: Option<String>,
    running: bool,
}

impl Engine {
    /// Connect to the daemon using the process environment
    /// (`XDG_RUNTIME_DIR` / `PIPEWIRE_RUNTIME_DIR` / `PIPEWIRE_REMOTE` are
    /// honoured by PipeWire, so pointing the backend at the headless daemon is
    /// purely an environment concern).
    pub fn connect() -> Result<Engine, BackendError> {
        pw::init();
        let mainloop = pw::main_loop::MainLoop::new(None)
            .map_err(|e| BackendError::new("pipewire_error", format!("mainloop: {e}")))?;
        let context = pw::context::Context::new(&mainloop)
            .map_err(|e| BackendError::new("pipewire_error", format!("context: {e}")))?;
        let core = context
            .connect(None)
            .map_err(|e| BackendError::new("pipewire_error", format!("connect: {e}")))?;
        let session = PwSession {
            _mainloop: mainloop,
            context,
            core,
        };
        session.roundtrip();
        let avail = PluginSet::on_disk();
        log::info!(
            "PipeWire engine connected — {} LADSPA plugin(s) available",
            avail.count()
        );
        Ok(Engine {
            session,
            avail,
            strips: IndexMap::new(),
            modules: IndexMap::new(),
            routes: HashSet::new(),
            default_sink: None,
            running: false,
        })
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
        }
    }

    /// Stop the engine and tear down the whole graph (Python `shutdown`).
    pub fn stop(&mut self) -> Result<(), BackendError> {
        self.teardown_all();
        self.running = false;
        Ok(())
    }

    // ------------------------------------------------------------------
    // apply_snapshot — full-diff reconcile
    // ------------------------------------------------------------------

    fn apply_snapshot(&mut self, strips: Vec<Strip>) -> Result<JsonValue, BackendError> {
        if !self.running {
            // First apply after connect: sweep any Holaf debris left by a
            // previous crashed run so we always start from a clean graph.
            self.sweep_zombies();
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

        // 2) diff each desired strip.
        for strip in desired.values() {
            self.sync_strip(strip)?;
        }

        // 3) diff the routing matrix from the input strips' `.routes`.
        let wanted_routes: HashSet<(String, String)> = desired
            .values()
            .filter(|s| s.is_input())
            .flat_map(|s| {
                s.routes
                    .iter()
                    .filter(|t| desired.contains_key(*t))
                    .map(|t| (s.uid.clone(), t.clone()))
            })
            .collect();
        self.reconcile_routes(&wanted_routes)?;

        // 4) default sink (is_default on an input strip — mirrors Python).
        let default_uid = desired
            .values()
            .find(|s| s.is_default)
            .or_else(|| desired.values().find(|s| s.is_input()))
            .map(|s| s.uid.clone());
        if let Some(uid) = default_uid {
            let _ = self.set_default_sink(&uid);
        }

        self.strips = desired;
        self.running = true;
        let count = self.strips.len();
        log::info!("apply_snapshot: {count} strip(s) reconciled");
        Ok(json!({ "applied": count, "running": true }))
    }

    /// Reconcile one strip: strip-node, chain, volume/mute/mono.
    /// Reconcile one strip: strip-node, chain, volume/mute/mono.
    fn sync_strip(&mut self, strip: &Strip) -> Result<(), BackendError> {
        let uid = &strip.uid;
        let want_chain = graph::wants_fx_chain(strip, &self.avail);

        // ---- strip node ----
        if graph::is_physical(strip) {
            let device = strip.device_name.as_deref().unwrap_or_default();
            if self.session.view().node_named(device).is_none() {
                log::warn!(
                    "strip '{}' is physical but device '{}' is not in the registry — \
                     creating no node (headless/deviceless); volume/FX applied when it appears",
                    strip.label,
                    device
                );
            }
        } else {
            let name = graph::strip_node_name(uid);
            if self.session.view().node_named(&name).is_none() {
                let desc = format!("Holaf Mix: {}", strip.label);
                self.session.create_null_sink(&name, &desc)?;
            }
        }

        // ---- chain ----
        let chain_exists = self.modules.contains_key(uid);
        if want_chain && !chain_exists {
            self.create_chain(strip)?;
        } else if want_chain {
            // Exists: re-push control values so a snapshot reflects the
            // current desired params (the chain graph already contains every
            // plugin; only controls change).
            self.push_chain_params(strip)?;
        } else if chain_exists {
            // "destroy if obsolete": a snapshot with no active effect removes
            // the chain (runtime toggles, by contrast, keep it bypassed).
            log::info!("apply_snapshot: strip '{}' no longer wants FX — unloading chain", uid);
            self.unload_chain(uid);
        }

        // ---- volume / mute / mono ----
        self.push_node_volume_mute(strip)?;

        // Store/update the desired strip (also used by status / fx_*).
        self.strips.insert(uid.clone(), strip.clone());
        Ok(())
    }

    /// Reconcile the routing matrix against the wanted route set.
    fn reconcile_routes(&mut self, wanted: &HashSet<(String, String)>) -> Result<(), BackendError> {
        // Destroy links for routes that are no longer wanted.
        let stale: Vec<(String, String)> = self
            .routes
            .iter()
            .filter(|r| !wanted.contains(*r))
            .cloned()
            .collect();
        for (src, dst) in stale {
            self.unroute(&src, &dst);
            self.routes.remove(&(src, dst));
        }
        // (Re-)create every wanted route.  We *always* unlink + re-link rather
        // than trusting the route-set membership: the effective node of a
        // strip changes when its FX chain is created/destroyed in the same
        // snapshot (an output with a chain is targeted via input.Holaf_FX_*,
        // without one via the raw sink), so a previously-linked route may now
        // point at a stale topology.
        for (src, dst) in wanted {
            self.unroute(src, dst);
            self.route(src, dst)?;
            self.routes.insert((src.clone(), dst.clone()));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Chain lifecycle
    // ------------------------------------------------------------------

    /// Load a filter-chain module for `strip` into our local context, wait for
    /// its two streams, then wire the intra-strip link (chain output → sink /
    /// source → chain input).
    fn create_chain(&mut self, strip: &Strip) -> Result<(), BackendError> {
        let uid = &strip.uid;
        let graph = spa::build_graph(strip, true);
        if graph.is_empty() {
            return Err(BackendError::new(
                "fx_graph_empty",
                format!("cannot build FX chain for '{}': no plugin available", strip.label),
            ));
        }
        let flavour = ChainFlavour::of(strip);
        let is_output = matches!(flavour, ChainFlavour::Output);
        let fx_name = graph::fx_node_name(uid);
        let description = sanitize_description(&strip.label);
        let target = chain_target_object(strip, is_output);
        let cfg = spa_config::build_filter_chain_config(
            &fx_name,
            &format!("Holaf FX {description}"),
            &graph,
            is_output,
            target.as_deref(),
        );

        let module = unsafe {
            // SAFETY: the context lives as long as the worker; the module is
            // tracked in self.modules and explicitly destroyed on teardown.
            sys::load_module(
                &self.session.context,
                "libpipewire-module-filter-chain",
                Some(&cfg),
                None,
            )
            .map_err(|e| BackendError::internal("load filter-chain module", e))?
        };

        // Wait for both stream nodes to appear in the daemon registry.
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
            unsafe {
                module.schedule_destroy();
            }
            for _ in 0..CHAIN_TEARDOWN_ROUNDTRIPS {
                self.session.roundtrip();
                thread::sleep(Duration::from_millis(40));
            }
            return Err(BackendError::new(
                "fx_node_not_ready",
                format!("FX chain '{}' streams never became ready", strip.label),
            ));
        }

        self.modules.insert(uid.clone(), module);
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

        // Intra-strip link:
        //   output chain: chain output → sink node (device or null-sink)
        //   input chain:  source node (device or null-sink) → chain input
        if is_output {
            let target_node = self.output_target_node_name(strip);
            if let Some(target) = target_node {
                self.link_nodes(&out_name, &target, strip.is_mono)?;
            }
        } else {
            let source_node = self.input_source_node_name(strip);
            if let Some(source) = source_node {
                self.link_nodes(&source, &in_name, false)?;
            }
        }
        Ok(())
    }

    /// Unload (destroy) the chain module for `uid`, if any.
    fn unload_chain(&mut self, uid: &str) {
        let Some(module) = self.modules.shift_remove(uid) else {
            return;
        };
        // SAFETY: deferred destroy from outside any module callback; the
        // roundtrips below let the context finish the teardown.
        unsafe {
            module.schedule_destroy();
        }
        for _ in 0..CHAIN_TEARDOWN_ROUNDTRIPS {
            self.session.roundtrip();
            thread::sleep(Duration::from_millis(40));
        }
        log::info!("unloaded FX chain for strip '{uid}'");
    }

    /// `set-param(Props)` with the flat params struct on `input.Holaf_FX_<uid>`.
    fn push_chain_params(&mut self, strip: &Strip) -> Result<(), BackendError> {
        let avail = self.avail.clone();
        let uid = &strip.uid;
        let in_name = graph::fx_input_node_name(uid);
        let node = match self.session.bind_node_by_name(&in_name)? {
            Some(n) => n,
            None => {
                // Chain module gone (daemon restart).  Drop our stale handle so
                // the next activation rebuilds it.
                self.modules.shift_remove(uid);
                return Err(BackendError::new(
                    "chain_gone",
                    format!("chain '{}' not in registry", in_name),
                ));
            }
        };
        let pairs = spa::control_pairs_with(strip, &avail);
        if pairs.is_empty() {
            return Ok(());
        }
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

    /// Name of the "master" node for volume / FX target.
    fn strip_master_node_name(&self, uid: &str) -> Option<String> {
        let strip = self.strips.get(uid)?;
        if graph::is_physical(strip) {
            strip.device_name.clone()
        } else {
            Some(graph::strip_node_name(uid))
        }
    }

    fn push_node_volume_mute(&mut self, strip: &Strip) -> Result<(), BackendError> {
        let Some(master) = self.strip_master_node_name(&strip.uid) else {
            return Ok(());
        };
        let Some(node) = self.session.bind_node_by_name(&master)? else {
            return Ok(());
        };
        push_volume_mute_pod(&node, strip.volume, strip.mute)?;
        self.session.roundtrip();
        Ok(())
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
            self.unroute(uid, &target);
            let _ = self.route(uid, &target);
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
        let chain_exists = self.modules.contains_key(uid);
        if want_chain && !chain_exists {
            // First activation → build the chain and wire it.
            self.create_chain(&clone)?;
        } else if chain_exists {
            // Chain already exists (kept after a previous disable): hot-reload
            // the flat Props with the new effective controls (bypass for the
            // freshly-disabled effect, real values for the enabled one).
            if let Err(e) = self.push_chain_params(&clone) {
                if e.code == "chain_gone" {
                    // Chain vanished underneath us: rebuild from scratch.
                    self.create_chain(&clone)?;
                } else {
                    return Err(e);
                }
            }
        }
        // When !want_chain and no chain exists: nothing to do (no audible
        // effect either) — the state is stored for a future activation.

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
        if self.modules.contains_key(uid) {
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
    /// an FX chain, else the raw source (device source / null-sink monitor).
    fn effective_source_name(&self, strip: &Strip) -> Option<String> {
        let uid = &strip.uid;
        if self.modules.contains_key(uid) {
            return Some(graph::fx_output_node_name(uid));
        }
        if graph::is_physical(strip) {
            strip.device_name.clone()
        } else {
            Some(graph::strip_node_name(uid))
        }
    }

    /// Effective *target* sink node name of a strip: chain capture sink if the
    /// strip has an FX chain, else the raw sink (device sink / null-sink).
    fn effective_target_name(&self, strip: &Strip) -> Option<String> {
        let uid = &strip.uid;
        if self.modules.contains_key(uid) {
            return Some(graph::fx_input_node_name(uid));
        }
        if graph::is_physical(strip) {
            strip.device_name.clone()
        } else {
            Some(graph::strip_node_name(uid))
        }
    }

    fn input_source_node_name(&self, strip: &Strip) -> Option<String> {
        if graph::is_physical(strip) {
            strip.device_name.clone()
        } else {
            Some(graph::strip_node_name(&strip.uid))
        }
    }

    fn output_target_node_name(&self, strip: &Strip) -> Option<String> {
        if graph::is_physical(strip) {
            strip.device_name.clone()
        } else {
            Some(graph::strip_node_name(&strip.uid))
        }
    }

    /// Create the audio links between a source node and a sink node (FL→FL,
    /// FR→FR; mono → duplicated from FL), skipping links that already exist
    /// (anti-WirePlumber duplication, like the spike).
    fn link_nodes(&mut self, source_name: &str, target_name: &str, mono: bool) -> Result<(), BackendError> {
        let source = self
            .session
            .wait_node(source_name, LINK_RETRIES, LINK_RETRY_DELAY_MS);
        let target = self
            .session
            .wait_node(target_name, LINK_RETRIES, LINK_RETRY_DELAY_MS);
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
                match (src_fl, dst_fl, dst_fr) {
                    (Some(s), Some(dl), Some(dr)) => vec![(s, dl), (s, dr)],
                    (Some(s), Some(dl), None) => vec![(s, dl)],
                    _ => Vec::new(),
                }
            } else {
                match (src_fl, src_fr, dst_fl, dst_fr) {
                    (Some(sl), Some(sr), Some(dl), Some(dr)) => vec![(sl, dl), (sr, dr)],
                    (Some(sl), None, Some(dl), None) => vec![(sl, dl)],
                    (None, Some(sr), None, Some(dr)) => vec![(sr, dr)],
                    _ => Vec::new(),
                }
            };

            if pairs.is_empty() {
                thread::sleep(Duration::from_millis(LINK_RETRY_DELAY_MS));
                continue;
            }

            let view = self.session.view();
            let mut created = 0usize;
            for (out_port, in_port) in &pairs {
                if view.link_exists(*out_port, *in_port) {
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
                if let Err(e) = link {
                    log::warn!("create link {source_name}→{target_name} failed: {e}");
                } else {
                    created += 1;
                }
            }
            self.session.roundtrip();
            log::info!(
                "link '{source_name}' → '{target_name}' ({created} created, {} already present)",
                pairs.len() - created
            );
            return Ok(());
        }
        Ok(())
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

    fn route(&mut self, source_uid: &str, target_uid: &str) -> Result<(), BackendError> {
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
            return Ok(());
        };
        self.link_nodes(&src, &dst, source_strip.is_mono)
    }

    /// Unlink an existing route (does **not** touch the route-set bookkeeping;
    /// callers decide membership).
    fn unroute(&mut self, source_uid: &str, target_uid: &str) {
        let Some(source_strip) = self.strips.get(source_uid).cloned() else {
            return;
        };
        let Some(target_strip) = self.strips.get(target_uid).cloned() else {
            return;
        };
        let (Some(src), Some(dst)) = (
            self.effective_source_name(&source_strip),
            self.effective_target_name(&target_strip),
        ) else {
            return;
        };
        self.unlink_nodes(&src, &dst);
    }

    fn routing_set(
        &mut self,
        source_uid: &str,
        target_uid: &str,
        active: bool,
    ) -> Result<JsonValue, BackendError> {
        if active {
            if !self.routes.contains(&(source_uid.to_string(), target_uid.to_string())) {
                self.route(source_uid, target_uid)?;
                self.routes.insert((source_uid.to_string(), target_uid.to_string()));
            }
        } else {
            self.unroute(source_uid, target_uid);
            self.routes.remove(&(source_uid.to_string(), target_uid.to_string()));
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
            Some(graph::strip_node_name(uid))
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
                    && !n.name.starts_with("Holaf_")
                    && !n.name.starts_with("input.Holaf_FX_")
                    && !n.name.starts_with("output.Holaf_FX_")
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
        let strips: Vec<JsonValue> = self
            .strips
            .values()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(|e| BackendError::internal("status", e))?;

        // Live Holaf_* node names (external verification / debugging).
        let view = self.session.view();
        let mut holaf_nodes: Vec<&str> = view
            .nodes()
            .iter()
            .filter(|n| n.name.starts_with("Holaf_") || n.name.contains("Holaf_FX_"))
            .map(|n| n.name.as_str())
            .collect();
        holaf_nodes.sort();

        let chains: Vec<JsonValue> = self
            .strips
            .values()
            .map(|s| {
                let has_chain = self.modules.contains_key(&s.uid);
                json!({ "uid": s.uid, "chain": has_chain })
            })
            .collect();

        let routes = self.routing_list_json();
        Ok(json!({
            "running": self.running,
            "strips": strips,
            "routes": routes["routes"],
            "apps": [],
            "default_sink": self.default_sink,
            "holaf_nodes": holaf_nodes,
            "fx_chains": chains,
        }))
    }

    // ------------------------------------------------------------------
    // Teardown / cleanup
    // ------------------------------------------------------------------

    fn teardown_strip(&mut self, uid: &str) {
        self.unload_chain(uid);
        let name = graph::strip_node_name(uid);
        let node_and_links = {
            let view = self.session.view();
            match view.node_named(&name) {
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
        self.routes.retain(|(s, t)| s != uid && t != uid);
    }

    fn teardown_all(&mut self) {
        for uid in self.modules.keys().cloned().collect::<Vec<_>>() {
            self.unload_chain(&uid);
        }
        let view = self.session.view();
        let nodes: Vec<u32> = view
            .nodes()
            .iter()
            .filter(|n| {
                n.name.starts_with("Holaf_Strip_")
                    || n.name.starts_with("input.Holaf_FX_")
                    || n.name.starts_with("output.Holaf_FX_")
            })
            .map(|n| n.id)
            .collect();
        drop(view);
        for id in nodes {
            self.session.destroy_global(id);
        }
        for _ in 0..CHAIN_TEARDOWN_ROUNDTRIPS {
            self.session.roundtrip();
            thread::sleep(Duration::from_millis(40));
        }
        self.strips.clear();
        self.routes.clear();
        self.default_sink = None;
        log::info!("engine teardown complete");
    }

    /// Destroy every live object whose name contains `Holaf_` (debris from a
    /// previous crashed run) — mirrors Python `_clean_zombie_nodes`.
    fn sweep_zombies(&mut self) {
        let view = self.session.view();
        let ids: Vec<u32> = view
            .nodes()
            .iter()
            .filter(|n| n.name.contains("Holaf_"))
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
            thread::sleep(Duration::from_millis(40));
        }
    }
}

// ---------------------------------------------------------------------------
// Pure/static helpers
// ---------------------------------------------------------------------------

/// What `target.object` a chain's playback/capture should point at.
fn chain_target_object(strip: &Strip, is_output: bool) -> Option<String> {
    if is_output {
        if graph::is_physical(strip) {
            strip.device_name.clone()
        } else {
            Some(graph::strip_node_name(&strip.uid))
        }
    } else if graph::is_physical(strip) {
        strip.device_name.clone()
    } else {
        Some(graph::strip_node_name(&strip.uid))
    }
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
fn push_params_pod(node: &pw::node::Node, pairs: &[(String, f32)]) -> Result<(), BackendError> {
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
fn push_volume_mute_pod(node: &pw::node::Node, volume: f32, mute: bool) -> Result<(), BackendError> {
    let volume = if volume.is_finite() && volume >= 0.0 {
        volume
    } else {
        1.0
    };
    let object = Value::Object(pw::spa::pod::Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_Props,
        id: pw::spa::sys::SPA_PARAM_Props,
        properties: vec![
            pw::spa::pod::Property::new(
                pw::spa::sys::SPA_PROP_channelVolumes,
                Value::ValueArray(pw::spa::pod::ValueArray::Float(vec![volume; 2])),
            ),
            pw::spa::pod::Property::new(
                pw::spa::sys::SPA_PROP_mute,
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
}
