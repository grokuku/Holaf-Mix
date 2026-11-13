//! [`ChainHost`] — *where* a strip's `module-filter-chain` is hosted.
//!
//! The engine does not care how a chain gets loaded, only that (a) it exists,
//! (b) it can be unloaded, and (c) it becomes schedulable.  Two implementations:
//!
//! * [`HostBackend::InProcess`] — `pw_context_load_module` into the daemon's
//!   own context (the behaviour before this refactor);
//! * [`HostBackend::External`] — a dedicated child process
//!   (`holafd --fx-host`, see [`crate::fxhost`]) that does nothing but host
//!   chains and run its mainloop: exactly the `pw-cli` shape the Python engine
//!   (which never chopped) has always used.
//!
//! `External` is the default because it is the architecture with the
//! measurement history behind it: the Python engine (dedicated `pw-cli` host)
//! never chopped, while the in-process Rust engine does on timing-sensitive
//! devices.  Keeping both, selectable, is deliberate — a machine where spawning
//! the host fails must still get a working graph (see [`ChainHost::from_mode`]).
//!
//! # Death, respawn and restore
//!
//! The desired chains (`uid` → module arguments) are the source of truth and
//! live **here, in the parent** — not in the child.  So when the host dies:
//!
//! 1. [`ChainHost::ensure_alive`] notices it (child exit / closed pipe),
//! 2. spawns a fresh host,
//! 3. replays every desired chain into it,
//! 4. and the engine's existing verification pass re-creates the intra-strip
//!    links: the chain's stream nodes died with the old client connection, so
//!    `ensure_chain_links` finds them missing and redoes them (routed links are
//!    handled by the same pass).
//!
//! Because node names derive from the strip uid, everything the rest of the
//! graph points at keeps its name across a respawn: the graph heals with no
//! user action.

use indexmap::IndexMap;
use pipewire as pw;

use holaf_core::backend::BackendError;

use crate::fxhost::FxHostClient;
use crate::sys::LocalModule;

/// Where the filter-chain modules of this engine are hosted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FxHostMode {
    /// A dedicated child process (default; the proven `pw-cli` shape).
    External,
    /// Inside the daemon, in its own context (legacy behaviour).
    InProcess,
}

impl FxHostMode {
    /// Parse the CLI / environment spelling.
    pub fn parse(text: &str) -> Result<FxHostMode, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "external" | "fx-host" | "process" => Ok(FxHostMode::External),
            "inprocess" | "in-process" | "embedded" => Ok(FxHostMode::InProcess),
            other => Err(format!(
                "unknown FX host mode {other:?} (expected 'external' or 'inprocess')"
            )),
        }
    }

    /// Stable name for logs / `status`.
    pub fn name(self) -> &'static str {
        match self {
            FxHostMode::External => "external",
            FxHostMode::InProcess => "inprocess",
        }
    }
}

impl Default for FxHostMode {
    fn default() -> Self {
        FxHostMode::External
    }
}

/// The concrete hosting backend.
enum HostBackend {
    /// Chains loaded into the daemon's own `pw_context`.
    ///
    /// `modules` is declared before `context`: the handles are only meaningful
    /// while the context lives, so they must be considered invalid first.
    InProcess {
        modules: IndexMap<String, LocalModule>,
        context: pw::context::ContextRc,
    },
    /// Chains delegated to a child process.  `None` once the host was stopped
    /// (or could never be started): a later `load` respawns it.
    External(Option<FxHostClient>),
}

fn host_down() -> BackendError {
    BackendError::new("fx_host_down", "no FX host process is running")
}

/// Desired chains + the backend that realizes them.
pub struct ChainHost {
    mode: FxHostMode,
    /// `uid` → `libpipewire-module-filter-chain` argument string: the source of
    /// truth for a respawn.
    desired: IndexMap<String, String>,
    backend: HostBackend,
    /// How many times the external host had to be respawned (diagnostics).
    respawns: u32,
    /// Uids whose chain was restored by a respawn and whose graph wiring
    /// (intra-strip links, volume) has therefore to be re-verified.  Drained by
    /// the engine (`take_restored`) into its pending/repair machinery.
    restored: Vec<String>,
}

impl ChainHost {
    /// Host chains inside the calling process, using `context`.
    pub fn in_process(context: pw::context::ContextRc) -> ChainHost {
        ChainHost {
            mode: FxHostMode::InProcess,
            desired: IndexMap::new(),
            backend: HostBackend::InProcess {
                modules: IndexMap::new(),
                context,
            },
            respawns: 0,
            restored: Vec::new(),
        }
    }

    /// Delegate hosting to a child `holafd --fx-host`.
    pub fn external(client: FxHostClient) -> ChainHost {
        ChainHost {
            mode: FxHostMode::External,
            desired: IndexMap::new(),
            backend: HostBackend::External(Some(client)),
            respawns: 0,
            restored: Vec::new(),
        }
    }

    /// Build the host requested by `mode`, **falling back to in-process** when
    /// the external host cannot be spawned.
    ///
    /// The fallback is loud on purpose: silently degrading to the architecture
    /// users report the chopping with would make the next bug report
    /// undiagnosable.
    pub fn from_mode(mode: FxHostMode, context: pw::context::ContextRc) -> ChainHost {
        match mode {
            FxHostMode::External => match FxHostClient::spawn() {
                Ok(client) => ChainHost::external(client),
                Err(e) => {
                    log::error!(
                        "cannot start the dedicated FX host ({}: {}) — falling back to hosting \
                         the FX chains INSIDE holafd.  Chains may chop on timing-sensitive \
                         devices in this mode.",
                        e.code,
                        e.message
                    );
                    ChainHost::in_process(context)
                }
            },
            FxHostMode::InProcess => ChainHost::in_process(context),
        }
    }

    /// Which backend is really in use (may differ from the requested mode after
    /// a fallback).
    pub fn mode(&self) -> FxHostMode {
        self.mode
    }

    /// Uids whose chain was just restored by a respawn, i.e. whose nodes are
    /// brand new and whose links must be re-verified.
    ///
    /// Draining is the caller's job: the engine turns each one into a pending
    /// item so its repair pass redoes the intra-strip link and the volume (a
    /// chain that comes back without its link is a silent strip).
    pub fn take_restored(&mut self) -> Vec<String> {
        std::mem::take(&mut self.restored)
    }

    /// How many respawns happened so far.
    pub fn respawns(&self) -> u32 {
        self.respawns
    }

    /// Pid of the dedicated host, when there is one running.
    pub fn pid(&self) -> Option<u32> {
        match &self.backend {
            HostBackend::External(Some(c)) => Some(c.pid()),
            HostBackend::External(None) | HostBackend::InProcess { .. } => None,
        }
    }

    /// Desired chain uids, in load order.
    pub fn uids(&self) -> Vec<String> {
        self.desired.keys().cloned().collect()
    }

    /// Is a chain currently hosted for `uid`?
    pub fn contains(&self, uid: &str) -> bool {
        match &self.backend {
            HostBackend::InProcess { modules, .. } => modules.contains_key(uid),
            HostBackend::External(_) => self.desired.contains_key(uid),
        }
    }

    /// Are *any* chains hosted here (drives the worker's idle pumping policy)?
    pub fn is_empty(&self) -> bool {
        match &self.backend {
            HostBackend::InProcess { modules, .. } => modules.is_empty(),
            HostBackend::External(_) => self.desired.is_empty(),
        }
    }

    /// Does **this process** have to keep pumping its mainloop for the chains?
    ///
    /// `false` with an external host, on purpose: the child pumps its own loop
    /// and the daemon's loop only serves the graph + IPC.
    pub fn pumps_own_mainloop(&self) -> bool {
        self.mode == FxHostMode::InProcess && !self.is_empty()
    }

    /// Is the backing host still healthy?  (`External`: child alive and its pipe
    /// open.  `InProcess`: always.)
    pub fn is_alive(&mut self) -> bool {
        match &mut self.backend {
            HostBackend::External(client) => {
                client.as_mut().map(|c| c.is_alive()).unwrap_or(false)
            }
            HostBackend::InProcess { .. } => true,
        }
    }

    /// Respawn the external host if needed, replaying every desired chain.
    ///
    /// Called before any command is issued, so a dead host is replaced without
    /// the engine ever seeing an error.
    pub fn ensure_alive(&mut self) -> Result<(), BackendError> {
        if self.mode != FxHostMode::External || self.is_alive() {
            return Ok(());
        }
        let uids = self.desired.keys().cloned().collect::<Vec<_>>();
        self.respawns = self.respawns.saturating_add(1);
        log::warn!(
            "the dedicated FX host is not running — starting it (attempt #{}) and restoring \
             {} chain(s)",
            self.respawns,
            uids.len()
        );
        let mut client = FxHostClient::spawn()?;
        let mut restored = Vec::with_capacity(uids.len());
        for uid in &uids {
            let Some(config) = self.desired.get(uid) else {
                continue;
            };
            client.load_chain(uid, config, None)?;
            log::info!("FX host: chain '{uid}' restored");
            restored.push(uid.clone());
        }
        self.backend = HostBackend::External(Some(client));
        self.restored.extend(restored);
        Ok(())
    }

    /// Load (or replace) the chain of `uid`.
    pub fn load(&mut self, uid: &str, config: &str) -> Result<(), BackendError> {
        self.ensure_alive()?;
        // Record the desire *before* loading: if the load fails half-way, the
        // next respawn/repair still knows what this strip wants.
        self.desired.insert(uid.to_string(), config.to_string());
        match &mut self.backend {
            HostBackend::InProcess { modules, context } => {
                // SAFETY: `context` outlives every handle stored in `modules`
                // (both live in this struct, modules first) and teardown
                // unloads modules before the engine drops its session.
                let loaded = unsafe {
                    crate::sys::load_module(
                        context,
                        "libpipewire-module-filter-chain",
                        Some(config),
                        None,
                    )
                };
                match loaded {
                    Ok(module) => {
                        if let Some(old) = modules.insert(uid.to_string(), module) {
                            // Replacing an existing chain: destroy the previous
                            // module.  SAFETY: deferred destroy, outside any
                            // module callback.
                            unsafe { old.schedule_destroy() };
                        }
                        Ok(())
                    }
                    Err(e) => {
                        self.desired.shift_remove(uid);
                        Err(BackendError::internal("load filter-chain module", e))
                    }
                }
            }
            HostBackend::External(client) => {
                let Some(client) = client.as_mut() else {
                    self.desired.shift_remove(uid);
                    return Err(host_down());
                };
                match client.load_chain(uid, config, None) {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        // The host refused: drop the desire so a later repair
                        // does not keep replaying a config it cannot load.
                        self.desired.shift_remove(uid);
                        Err(e)
                    }
                }
            }
        }
    }

    /// Unload one chain.  Best effort: a dead host is not an error (the chain
    /// went away with it).
    pub fn unload(&mut self, uid: &str) {
        self.desired.shift_remove(uid);
        match &mut self.backend {
            HostBackend::InProcess { modules, .. } => {
                if let Some(module) = modules.shift_remove(uid) {
                    // SAFETY: deferred destroy from outside any module callback.
                    unsafe { module.schedule_destroy() };
                }
            }
            HostBackend::External(client) => {
                if let Some(client) = client.as_mut() {
                    if client.is_alive() {
                        if let Err(e) = client.unload_chain(uid) {
                            log::warn!("FX host: unloading chain '{uid}' failed: {}", e.message);
                        }
                    }
                }
            }
        }
    }

    /// Drop our **handle** on `uid`'s chain because its stream nodes vanished,
    /// while keeping the *desire*.
    ///
    /// The reconciler calls this right before rebuilding the chain.  The
    /// difference with [`ChainHost::unload`] matters: the rebuild can be
    /// deferred (its anchor node is momentarily out of the registry), and a
    /// strip that still wants a chain must keep saying so — wiping the desire
    /// there made `status.fx_host.chains` (and the whole desire state) report
    /// "no chain" for a strip whose chain was about to be rebuilt.
    pub fn mark_stale(&mut self, uid: &str) {
        if let HostBackend::InProcess { modules, .. } = &mut self.backend {
            if let Some(module) = modules.shift_remove(uid) {
                // SAFETY: deferred destroy from outside any module callback.
                unsafe { module.schedule_destroy() };
            }
        }
        // External: the child's module already died with it — there is nothing
        // to destroy, and the desire stays (it is what `ensure_alive` replays).
    }

    /// Forget a chain entirely (handle *and* desire) without asking the host to
    /// destroy anything.
    pub fn forget(&mut self, uid: &str) {
        self.unload(uid);
    }

    /// Unload everything we host.
    pub fn unload_all(&mut self) {
        for uid in self.desired.keys().cloned().collect::<Vec<_>>() {
            self.unload(&uid);
        }
        self.desired.clear();
    }

    /// Hot-reload the flat `Props.params` of an **externally hosted** chain.
    ///
    /// Returns `Ok(false)` when the chain is hosted in-process, i.e. when the
    /// caller must push the parameters itself — it already knows how, through
    /// the daemon-side node proxy, and that is the same path a hot toggle has
    /// always used.
    pub fn set_params(
        &mut self,
        uid: &str,
        props: &[(String, f32)],
    ) -> Result<bool, BackendError> {
        if self.mode == FxHostMode::External {
            // A hot-reload is the most frequent order the daemon issues, so it
            // is also where a dead host must be noticed: without this the
            // chain stayed gone until some *other* path happened to call
            // `load` (measured: the graph kept reporting a dead pid and an
            // "unknown_chain" error forever).
            self.ensure_alive()?;
        }
        match &mut self.backend {
            HostBackend::External(Some(client)) => {
                client.set_params(uid, props)?;
                Ok(true)
            }
            HostBackend::External(None) => Err(host_down()),
            HostBackend::InProcess { .. } => Ok(false),
        }
    }

    /// Unload every chain in-process (module handles are the only way to
    /// destroy them) and/or stop the child process.
    pub fn shutdown(&mut self) {
        self.desired.clear();
        match &mut self.backend {
            HostBackend::InProcess { modules, .. } => {
                for (uid, module) in modules.drain(..) {
                    log::info!("unloaded FX chain for strip '{uid}'");
                    // SAFETY: deferred destroy from outside any module callback.
                    unsafe { module.schedule_destroy() };
                }
            }
            HostBackend::External(client) => {
                if let Some(client) = client.take() {
                    client.shutdown();
                }
            }
        }
    }

    /// One-line description for logs / `status`.
    pub fn describe(&self) -> String {
        match &self.backend {
            HostBackend::InProcess { .. } => {
                format!("{} (holafd pid {})", self.mode.name(), std::process::id())
            }
            HostBackend::External(Some(c)) => format!(
                "{} (pid {}, {} respawn(s))",
                self.mode.name(),
                c.pid(),
                self.respawns
            ),
            HostBackend::External(None) => {
                format!("{} (stopped, {} respawn(s))", self.mode.name(), self.respawns)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bookkeeping-only host: `External(None)` needs no context and no child, so
    /// the chain bookkeeping is unit-testable without PipeWire.
    fn bookkeeping_only() -> ChainHost {
        ChainHost {
            mode: FxHostMode::External,
            desired: IndexMap::new(),
            backend: HostBackend::External(None),
            respawns: 0,
            restored: Vec::new(),
        }
    }

    #[test]
    fn mode_parsing_is_forgiving_but_rejects_typos() {
        assert_eq!(FxHostMode::parse("external").unwrap(), FxHostMode::External);
        assert_eq!(FxHostMode::parse("FX-HOST").unwrap(), FxHostMode::External);
        assert_eq!(
            FxHostMode::parse("inprocess").unwrap(),
            FxHostMode::InProcess
        );
        assert_eq!(
            FxHostMode::parse("in-process").unwrap(),
            FxHostMode::InProcess
        );
        assert!(FxHostMode::parse("elsewhere").is_err());
        assert_eq!(FxHostMode::default(), FxHostMode::External);
        assert_eq!(FxHostMode::External.name(), "external");
    }

    #[test]
    fn desired_chains_are_tracked_without_a_running_host() {
        let mut host = bookkeeping_only();
        assert!(host.is_empty());
        assert!(!host.is_alive(), "no child has been spawned yet");
        assert_eq!(host.pid(), None);
        assert!(!host.pumps_own_mainloop(), "external host pumps its own loop");

        host.desired.insert("a".into(), "cfg".into());
        assert!(!host.is_empty());
        assert!(host.contains("a"));
        assert_eq!(host.uids(), vec!["a".to_string()]);

        host.unload("a");
        assert!(host.desired.is_empty());
        assert!(!host.contains("a"));
    }

    #[test]
    fn set_params_without_a_hosted_chain_never_succeeds_silently() {
        let mut host = bookkeeping_only();
        // `set_params` first tries to recover a dead host, which may or may not
        // find a spawnable `holafd` depending on the environment; what must
        // never happen is a silent `Ok` for a chain nobody hosts.
        match host.set_params("a", &[("eq_L:x".to_string(), 1.0)]) {
            Ok(handled) => assert!(
                !handled,
                "in-process host must report `false` (caller pushes itself)"
            ),
            Err(e) => assert!(
                matches!(
                    e.code,
                    "fx_host_down" | "fx_host_unusable" | "unknown_chain"
                ),
                "unexpected code: {e:?}"
            ),
        }
    }

    #[test]
    fn describe_names_the_backend() {
        let host = bookkeeping_only();
        assert_eq!(host.describe(), "external (stopped, 0 respawn(s))");
    }

    #[test]
    fn shutdown_is_idempotent_without_a_child() {
        let mut host = bookkeeping_only();
        host.desired.insert("a".into(), "cfg".into());
        host.shutdown();
        assert!(host.desired.is_empty());
        host.shutdown();
        assert!(!host.is_alive());
    }
}
