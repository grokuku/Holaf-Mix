//! The diagnostic report — the Rust twin of `src/diagnostics/collector.py`.
//!
//! Two separable halves, and only the first one lives here:
//!
//! 1. **Collection + rules** (this module, no `egui`, no daemon): the backend's
//!    `status()` payload, the device list, the meter sources and the strip list
//!    are folded into a [`Report`], and [`Report::detect`] runs the automatic
//!    checks that produce [`Finding`]s (`{id, severity, message, hint}`, the
//!    Python ids kept where they still apply).  Everything is a pure function
//!    of its inputs, so a degraded graph is a unit test, not a live repro.
//! 2. **Presentation** ([`crate::dialogs::draw_diagnostics`]): the tabbed window
//!    (Summary / Engine / Graph / System) with *Refresh* and *Copy report*.
//!
//! # What is reproduced, and what cannot be
//!
//! The Python collector shells out to `pw-dump` / `pw-top` itself and reads the
//! Qt log ring buffer; the Rust UI does neither.  It talks to the backend
//! through `Backend::status()`, which publishes the *engine's own* view of the
//! graph (`running`, `strips[].node_name`, `holaf_nodes`, `fx_host`, `pending`,
//! `degraded`, `mainloop_pumps`, `meter_sources`) **and** the clock/timing block
//! (`timing`: `pw-metadata -n settings` for the rate/quantum and, when the tool
//! supports batch mode, per-node `pw-top` statistics).  So `small_quantum`,
//! `pwt_errors` and `pw_top_unavailable` are computed here from real numbers —
//! and when a number is genuinely unavailable the daemon says **why**, which is
//! what `pw_top_unavailable` prints (on PipeWire < 0.3.66: "pw-top has no batch
//! mode").  Nothing is invented.
//!
//! Implemented ids: `daemon_unreachable`, `engine_not_running`, `graph_degraded`,
//! `graph_pending`, `fx_host_down`, `fx_host_respawns`, `fx_host_inprocess`,
//! `mainloop_pumps_stalled`, `mainloop_pumps_zero`, `mainloop_pumps_external`,
//! `legacy_bus_names`, `strip_bus_unnamed`, `strip_bus_missing`, `device_missing`,
//! `device_not_listed`, `metering_no_source`, `multiple_daemons`, and the
//! clock/processing ones: `small_quantum`, `pwt_errors`, `pw_top_unavailable`
//! (fed by `status.timing` — see [`TimingFacts`]).
//!
//! Two rules only fire where they mean something: `strip_bus_missing` skips
//! **physical** strips (they have no null-sink bus — their master node is the
//! device), and the `mainloop_pumps_*` family only judges the **in-process** FX
//! mode (with `fx_host.mode=external` the daemon hosts no chain, so a 0 counter
//! is normal).

use std::collections::HashMap;
use std::path::Path;

use holaf_core::model::Strip;
use serde_json::Value;

use crate::engine_json::DeviceInfo;

/// Version of this crate (the "app version" line of the report).
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// An empty `meter_sources` map, so [`Inputs::empty`] needs no allocation.
static EMPTY_SOURCES: std::sync::LazyLock<HashMap<String, String>> =
    std::sync::LazyLock::new(HashMap::new);

/// Prefix of the bus names the pre-J5 daemon used (`Holaf_Strip_<uid>`).
pub const LEGACY_STRIP_PREFIX: &str = "Holaf_Strip_";

/// Report severity, ordered (used for sorting, like the Python
/// `_SEVERITY_RANK`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    /// Lower-case name (JSON / tests).
    pub fn name(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }

    /// Short upper-case label, as the text report prints it.
    pub fn tag(self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Warning => "WARNING",
            Severity::Info => "INFO",
        }
    }
}

/// One automatic finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub message: String,
    pub hint: Option<String>,
}

impl Finding {
    fn new(id: &'static str, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            id,
            severity,
            message: message.into(),
            hint: None,
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// One line of the text report (`[ERROR  ] id — message`).
    pub fn line(&self) -> String {
        let mut line = format!(
            "  [{:<7}] {} — {}",
            self.severity.tag(),
            self.id,
            self.message
        );
        if let Some(hint) = &self.hint {
            line.push_str(&format!("\n            → {hint}"));
        }
        line
    }
}

/// One item of the engine's "still retrying" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingItem {
    pub key: String,
    pub message: String,
}

/// Who hosts the FX chains (`status.fx_host`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FxHost {
    /// `external` (a dedicated child) or `inprocess` (legacy).
    pub mode: String,
    pub pid: Option<i64>,
    /// Only known when the payload carries it; `external && pid.is_some()`
    /// otherwise (the daemon reaps a dead host lazily, so a stale pid would
    /// otherwise read as "alive").
    pub alive: bool,
    pub respawns: u64,
    pub chains: Vec<String>,
}

impl Default for FxHost {
    fn default() -> Self {
        Self {
            mode: "unknown".to_string(),
            pid: None,
            alive: false,
            respawns: 0,
            chains: Vec::new(),
        }
    }
}

/// One strip as the engine sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct StripFacts {
    pub uid: String,
    pub label: String,
    /// `Holaf_Bus_<Label>` — absent on a daemon that predates J5.
    pub node_name: Option<String>,
    pub device_name: Option<String>,
    /// Effects that are *active* (what should have a chain).
    pub active_effects: Vec<String>,
    pub volume: f32,
}

/// One row of the engine's `timing.pw_top` block (a `pw-top -b` sample).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingRow {
    pub name: Option<String>,
    pub quantum: u32,
    pub rate: u32,
    pub err: u64,
}

impl TimingRow {
    /// `Holaf_Bus_Mix_Out (ERR 3)` — how the `pwt_errors` finding names it.
    pub fn describe_err(&self) -> String {
        format!(
            "{} (ERR {})",
            self.name.as_deref().unwrap_or("unknown node"),
            self.err
        )
    }
}

/// The clock/timing section (`status.timing`, published by the daemon from
/// `pw-metadata -n settings` + `pw-top -b`).
///
/// `available` is about the **settings** (rate/quantum): the daemon always
/// reports them, unless `pw-metadata` could not be read at all.  The `pw-top`
/// statistics are a separate question — `pw_top_available = false` always
/// carries a reason, which is exactly what the `pw_top_unavailable` finding
/// prints.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimingFacts {
    /// Where the numbers came from (`pw-metadata settings`).
    pub source: Option<String>,
    /// `clock.rate` / effective quantum / one quantum in milliseconds.
    pub rate: u32,
    pub quantum: u32,
    pub quantum_ms: f64,
    /// Why the settings could not be read (`None` = they were).
    pub settings_error: Option<String>,
    /// Were the per-node `pw-top` statistics collected?
    pub pw_top_available: bool,
    pub pw_top_reason: Option<String>,
    pub pw_top_errors: u64,
    pub pw_top_rows: Vec<TimingRow>,
}

impl TimingFacts {
    /// Do we know a quantum at all?
    pub fn known(&self) -> bool {
        self.settings_error.is_none() && self.quantum > 0
    }

    /// One line for the Engine tab:
    /// `quantum 1024 @ 48 kHz (21.3 ms) — pw-top: unavailable (reason)`.
    pub fn summary(&self) -> String {
        let mut line = if self.known() {
            format!(
                "quantum {} @ {} kHz ({:.1} ms)",
                self.quantum,
                self.rate as f64 / 1000.0,
                self.quantum_ms
            )
        } else {
            format!(
                "quantum unknown ({})",
                self.settings_error.as_deref().unwrap_or("no data")
            )
        };
        line.push_str(&format!(
            " — pw-top: {}",
            if self.pw_top_available {
                format!(
                    "{} error(s) over {} node(s)",
                    self.pw_top_errors,
                    self.pw_top_rows.len()
                )
            } else {
                format!(
                    "unavailable ({})",
                    self.pw_top_reason.as_deref().unwrap_or("no reason given")
                )
            }
        ));
        line
    }

    /// Live quantum of a node by `node.name`, when `pw-top` provided it.
    pub fn quantum_of(&self, name: &str) -> Option<u32> {
        self.pw_top_rows
            .iter()
            .find(|row| row.name.as_deref() == Some(name))
            .map(|row| row.quantum)
            .filter(|quantum| *quantum > 0)
    }
}

/// Below this a graph overruns easily on a busy CPU — the `small_quantum`
/// threshold (Python `MIN_HEALTHY_QUANTUM`).
pub const MIN_HEALTHY_QUANTUM: u32 = 256;

/// `pw-top` error count from which the finding is an ERROR (Python
/// `PWT_ERROR_THRESHOLD`).
pub const PWT_ERROR_THRESHOLD: u64 = 100;

/// The engine section of the report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineFacts {
    /// Did `status()` answer at all?
    pub available: bool,
    pub reason: Option<String>,
    pub running: bool,
    pub degraded: bool,
    pub default_sink: Option<String>,
    pub pending: Vec<PendingItem>,
    pub fx_host: FxHost,
    pub mainloop_pumps: Option<u64>,
    pub strips: Vec<StripFacts>,
    /// `uid → source` the engine told the UI to record.
    pub meter_sources: usize,
    /// `Holaf_*` node names the engine saw in the graph.
    pub holaf_nodes: Vec<String>,
    /// Clock settings + `pw-top` statistics (`status.timing`).
    pub timing: TimingFacts,
}

/// The system section: what the process itself knows (no `pw-dump` here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemFacts {
    pub app_version: String,
    pub app_pid: u32,
    pub pipewire_version: Option<String>,
    pub os: String,
    pub config_path: Option<String>,
    /// Live `holafd` processes (from `/proc`), for `multiple_daemons`.
    pub daemon_pids: Vec<u32>,
}

/// One collected report.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// Unix seconds (`format_timestamp` renders it).
    pub generated_at: u64,
    pub engine: EngineFacts,
    /// Devices the engine listed (names only) and the ones a strip references
    /// but that are absent.
    pub devices: Vec<String>,
    pub missing_devices: Vec<(String, String, String)>,
    /// Uids the meter capture has no source for.
    pub meter_missing: Vec<String>,
    /// What the VU bars are drawing right now: `(uid, label, [left, right])`,
    /// in strip order, as `AppState::levels_for` returns them (0.0–1.0).
    ///
    /// This is the section the user missed in the first report (“L=0.030
    /// R=0.029” per strip in the Python one): without it, “les VU sont à fond”
    /// cannot be told apart from “the levels never arrive”.  It is the *drawn*
    /// value — envelope included — so it is exactly what is on screen.
    pub meter_levels: Vec<(String, String, [f32; 2])>,
    pub system: SystemFacts,
    pub findings: Vec<Finding>,
}

/// The tabs of the diagnostics window (the Python `DiagnosticsDialog` tabs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagTab {
    Summary,
    Engine,
    Graph,
    System,
    /// The last [`crate::logbuf::MAX_LINES`] log lines (the Qt "Journal" tab).
    /// Last on purpose: it is the tab a user opens *after* the others.
    Log,
}

impl DiagTab {
    pub const ALL: [DiagTab; 5] = [
        DiagTab::Summary,
        DiagTab::Engine,
        DiagTab::Graph,
        DiagTab::System,
        DiagTab::Log,
    ];

    pub fn title(self) -> &'static str {
        match self {
            DiagTab::Summary => "Summary",
            DiagTab::Engine => "Engine",
            DiagTab::Graph => "Graph",
            DiagTab::System => "System",
            DiagTab::Log => "Journal",
        }
    }
}

/// The open diagnostics window: which tab, and the report it shows.
#[derive(Debug, Clone)]
pub struct DiagnosticsState {
    pub tab: DiagTab,
    pub report: Report,
}

impl DiagnosticsState {
    pub fn new(report: Report) -> Self {
        Self {
            tab: DiagTab::Summary,
            report,
        }
    }
}

/// Everything [`Report::collect`] needs, so the caller (and the tests) never
/// depend on the ambient machine.
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    /// `Backend::status()` payload, `None` when the engine could not be asked.
    pub status: Option<&'a Value>,
    /// Why `status` is `None`.
    pub status_error: Option<&'a str>,
    pub devices: &'a [DeviceInfo],
    /// `uid → source` the engine published (its length drives the metering rule).
    pub meter_sources: &'a HashMap<String, String>,
    /// The UI's strip list (labels, and the fallback when the engine is silent).
    pub strips: &'a [Strip],
    /// `(uid, label, declared device)` of every unresolved binding.
    pub missing_devices: &'a [(String, String, String)],
    /// Uids the meter capture has no source for.
    pub meter_missing: &'a [String],
    /// `(uid, label, [left, right])` the VU bars currently draw (see
    /// [`Report::meter_levels`]).
    pub meter_levels: &'a [(String, String, [f32; 2])],
    pub config_path: Option<&'a Path>,
    /// Unix seconds; `0` means "now" (a test fixes it instead).
    pub now_unix: u64,
    /// `None` = read `/proc` (production); `Some` = use these (tests).
    pub daemon_pids: Option<Vec<u32>>,
}

impl<'a> Inputs<'a> {
    /// The minimal report: no engine, no devices, no strips.
    pub fn empty(strips: &'a [Strip]) -> Self {
        Self {
            status: None,
            status_error: None,
            devices: &[],
            meter_sources: &EMPTY_SOURCES,
            strips,
            missing_devices: &[],
            meter_missing: &[],
            meter_levels: &[],
            config_path: None,
            now_unix: 0,
            daemon_pids: Some(Vec::new()),
        }
    }
}

impl Report {
    /// Collect a report from what the UI already has in memory (see [`Inputs`]).
    pub fn collect(inputs: &Inputs<'_>) -> Self {
        let engine = engine_facts(inputs.status, inputs.status_error, inputs.strips);
        let mut report = Self {
            generated_at: inputs.now_unix,
            engine,
            devices: inputs
                .devices
                .iter()
                .map(|device| device.name.clone())
                .collect(),
            missing_devices: inputs.missing_devices.to_vec(),
            meter_missing: inputs.meter_missing.to_vec(),
            meter_levels: inputs.meter_levels.to_vec(),
            system: SystemFacts {
                app_version: APP_VERSION.to_string(),
                app_pid: std::process::id(),
                pipewire_version: pipewire_version(),
                os: os_description(),
                config_path: inputs.config_path.map(|path| path.display().to_string()),
                daemon_pids: match &inputs.daemon_pids {
                    Some(pids) => pids.clone(),
                    None => daemon_pids(),
                },
            },
            findings: Vec::new(),
        };
        // `meter_sources` is only used for the "engine running, strips, no
        // source" rule; keep the count, not the map (the report is text).  An
        // explicit map wins over the count parsed out of the status payload.
        if !inputs.meter_sources.is_empty() {
            report.engine.meter_sources = inputs.meter_sources.len();
        }
        report.findings = report.detect();
        report
    }

    /// Run every automatic rule; the findings come back sorted by severity then
    /// id so the text report is diffable (same order as the Python collector).
    pub fn detect(&self) -> Vec<Finding> {
        let engine = &self.engine;
        let mut findings: Vec<Finding> = Vec::new();

        if !engine.available {
            findings.push(
                Finding::new(
                    "daemon_unreachable",
                    Severity::Error,
                    format!(
                        "the engine could not be queried: {}",
                        engine.reason.as_deref().unwrap_or("unknown reason")
                    ),
                )
                .with_hint(
                    "check that holafd is running and that this UI uses the right socket \
                     (`holafctl status`), then press Refresh",
                ),
            );
            // Nothing else can be judged without the payload.
            findings.sort_by(findings_order);
            return findings;
        }

        if !engine.running {
            findings.push(
                Finding::new(
                    "engine_not_running",
                    Severity::Warning,
                    "the engine reports running=false: the audio graph is not built",
                )
                .with_hint(
                    "re-apply the configuration (add/remove a strip, or restart the engine \
                     from the context menu)",
                ),
            );
        }

        // --- graph health --------------------------------------------------
        if engine.degraded {
            findings.push(
                Finding::new(
                    "graph_degraded",
                    Severity::Error,
                    format!(
                        "the engine reports a DEGRADED graph — {}",
                        summarise_pending(&engine.pending)
                    ),
                )
                .with_hint(
                    "a device/route/chain could not be built: re-plug the device, then \
                     re-apply the configuration — this is the first thing to fix",
                ),
            );
        } else if !engine.pending.is_empty() {
            findings.push(
                Finding::new(
                    "graph_pending",
                    Severity::Warning,
                    format!(
                        "the engine is still retrying incomplete graph work: {}",
                        summarise_pending(&engine.pending)
                    ),
                )
                .with_hint(
                    "usually a device that is not enumerated yet; re-check in a few seconds",
                ),
            );
        }

        // --- FX host -------------------------------------------------------
        let fx_host = &engine.fx_host;
        if fx_host.mode.eq_ignore_ascii_case("inprocess") {
            findings.push(
                Finding::new(
                    "fx_host_inprocess",
                    Severity::Warning,
                    "the FX chains are hosted in the daemon process (mode=inprocess) instead of \
                     a dedicated --fx-host child",
                )
                .with_hint(
                    "the in-process mode is the legacy path and the known cause of crackling \
                     chains: restart holafd so it can spawn its FX host",
                ),
            );
        } else if fx_host.mode.eq_ignore_ascii_case("external") && !fx_host.alive {
            findings.push(
                Finding::new(
                    "fx_host_down",
                    Severity::Error,
                    format!(
                        "the FX host process is gone (pid {}) — chains are not running",
                        fx_host
                            .pid
                            .map(|pid| pid.to_string())
                            .unwrap_or_else(|| "?".to_string())
                    ),
                )
                .with_hint(
                    "the daemon respawns it on demand; if respawns keeps climbing, read the logs",
                ),
            );
        }
        if fx_host.respawns > 0 {
            let severity = if fx_host.respawns >= 3 {
                Severity::Error
            } else {
                Severity::Warning
            };
            findings.push(
                Finding::new(
                    "fx_host_respawns",
                    severity,
                    format!(
                        "the FX host was respawned {} time(s) — it is crashing",
                        fx_host.respawns
                    ),
                )
                .with_hint("look for a LADSPA plugin error or a memlock limit in the logs"),
            );
        }

        // --- mainloop service ----------------------------------------------
        // The counter is only meaningful for the **in-process** FX path: the
        // daemon bumps `mainloop_pumps` when it services its own PipeWire
        // mainloop, which it only does to host filter-chain modules. With
        // `fx_host.mode=external` (the default, and the only sane mode) the
        // dedicated `--fx-host` child owns the chains and pumps its own
        // mainloop, so `mainloop_pumps=0` is the *normal* state — reporting it
        // as a stall was a false positive that hid the real findings.
        if engine.running && engine.mainloop_pumps == Some(0) {
            if fx_host.mode.eq_ignore_ascii_case("inprocess") {
                if fx_host.chains.is_empty() {
                    findings.push(Finding::new(
                        "mainloop_pumps_zero",
                        Severity::Info,
                        "mainloop_pumps=0 in the in-process FX mode and no FX chain hosted: \
                         the daemon has not serviced an idle pass yet (or this build does not \
                         maintain the counter)",
                    ).with_hint(
                        "not fatal; if in-process FX chains crackle, treat it as a dead \
                         mainloop",
                    ));
                } else {
                    findings.push(
                        Finding::new(
                            "mainloop_pumps_stalled",
                            Severity::Warning,
                            format!(
                                "mainloop_pumps=0 while {} FX chain(s) are hosted in the \
                                 daemon (mode=inprocess): the daemon is not servicing its \
                                 PipeWire mainloop",
                                fx_host.chains.len()
                            ),
                        )
                        .with_hint("chains starve and the audio chops: restart the daemon"),
                    );
                }
            } else if fx_host.mode.eq_ignore_ascii_case("external") && !fx_host.chains.is_empty()
            {
                // Explains a counter that looks alarming but is not.
                findings.push(
                    Finding::new(
                        "mainloop_pumps_external",
                        Severity::Info,
                        format!(
                            "mainloop_pumps=0 is expected with fx_host.mode=external: the {} \
                             FX chain(s) are pumped by the dedicated FX host process (pid {}), \
                             not by the daemon",
                            fx_host.chains.len(),
                            fx_host
                                .pid
                                .map(|pid| pid.to_string())
                                .unwrap_or_else(|| "?".to_string())
                        ),
                    )
                    .with_hint(
                        "no action needed; this counter only tracks the in-process FX path",
                    ),
                );
            }
        }

        // --- node naming ----------------------------------------------------
        let legacy: Vec<String> = engine
            .strips
            .iter()
            .filter_map(|strip| strip.node_name.clone())
            .chain(engine.holaf_nodes.iter().cloned())
            .filter(|name| name.starts_with(LEGACY_STRIP_PREFIX))
            .collect();
        if !legacy.is_empty() {
            let names = unique_sorted(legacy);
            findings.push(
                Finding::new(
                    "legacy_bus_names",
                    Severity::Warning,
                    format!(
                        "legacy `Holaf_Strip_<uid>` bus names on a label-based daemon: {}",
                        names.join(", ")
                    ),
                )
                .with_hint(
                    "bus names follow the label (`Holaf_Bus_<Label>`) since J5 — the holafd \
                     binary in use is stale: rebuild and restart it",
                ),
            );
        }
        if !engine.strips.is_empty() && engine.strips.iter().all(|strip| strip.node_name.is_none())
        {
            findings.push(
                Finding::new(
                    "strip_bus_unnamed",
                    Severity::Warning,
                    format!(
                        "the engine does not publish `node_name` for any of its {} strips",
                        engine.strips.len()
                    ),
                )
                .with_hint(
                    "that field came with the label-based bus names: the holafd binary is stale",
                ),
            );
        }

        // --- strips vs the live graph ---------------------------------------
        if !engine.holaf_nodes.is_empty() {
            for strip in &engine.strips {
                // A **physical** strip (bound to a device) has no null-sink bus:
                // the engine publishes a `node_name` for every strip because it
                // is the *public handle* (`Holaf_Bus_<Label>`), but a physical
                // strip's master node is the DEVICE itself — no `Holaf_Bus_*`
                // node is ever created for it. Checking that bus in the graph is
                // a guaranteed false positive; what must exist for such a strip
                // is the device, which `device_missing` below covers.
                if is_physical(strip) {
                    continue;
                }
                if let Some(node_name) = &strip.node_name {
                    if !engine.holaf_nodes.iter().any(|node| node == node_name) {
                        findings.push(
                            Finding::new(
                                "strip_bus_missing",
                                Severity::Error,
                                format!(
                                    "virtual strip '{}' declares bus '{}' but that node is \
                                     absent from the graph",
                                    strip.label, node_name
                                ),
                            )
                            .with_hint(
                                "the bus was not created (daemon error, or the strip left the \
                                 graph): re-apply the configuration",
                            ),
                        );
                    }
                }
            }
        }

        // --- devices ---------------------------------------------------------
        if !self.devices.is_empty() {
            for (uid, label, declared) in &self.missing_devices {
                findings.push(
                    Finding::new(
                        "device_missing",
                        Severity::Error,
                        format!(
                            "strip '{label}' ({uid}) references device '{declared}' which the \
                             engine does not list"
                        ),
                    )
                    .with_hint("the device is unplugged/renamed: re-select it on the strip"),
                );
            }
        }
        // A device the config names while the engine lists *other* devices is
        // the same problem seen from the strip side; `device_not_listed` covers
        // the case where the strip list and the device list disagree without
        // any strip being repairable (e.g. a config edited by hand).
        for strip in &engine.strips {
            let Some(device) = &strip.device_name else {
                continue;
            };
            if self.devices.is_empty()
                || self.devices.iter().any(|name| name == device)
                || self
                    .missing_devices
                    .iter()
                    .any(|(uid, _, _)| uid == &strip.uid)
            {
                continue;
            }
            findings.push(
                Finding::new(
                    "device_not_listed",
                    Severity::Warning,
                    format!(
                        "strip '{}' is bound to '{}', which is not in the engine's device list",
                        strip.label, device
                    ),
                )
                .with_hint(
                    "the list may be momentarily incomplete; refresh, then re-select the device",
                ),
            );
        }

        // --- metering ---------------------------------------------------------
        if engine.running && !engine.strips.is_empty() && engine.meter_sources == 0 {
            findings.push(
                Finding::new(
                    "metering_no_source",
                    Severity::Warning,
                    "the engine is running and strips exist, but no metering source is registered",
                )
                .with_hint(
                    "meter sources follow the FX-chain lifecycle: wait 2 s (the engine re-syncs) \
                     then refresh",
                ),
            );
        }
        // **Every bar glued at the top** is not a mix: it is either a signal far
        // over full scale (a bus summing several strips, a doubled route) or a
        // capture read with the wrong sample format.  Both used to look like “the
        // VUs are always full”; the METERING section tells them apart (a level of
        // exactly 1.000 means +6 dBFS or more).
        let drawn: Vec<&(String, String, [f32; 2])> = self
            .meter_levels
            .iter()
            .filter(|(uid, _, _)| !self.meter_missing.iter().any(|missing| missing == uid))
            .collect();
        if drawn.len() >= 2
            && drawn
                .iter()
                .all(|(_, _, [l, r])| *l >= 0.999 && *r >= 0.999)
        {
            findings.push(
                Finding::new(
                    "metering_pinned",
                    Severity::Warning,
                    format!(
                        "all {} VU bars are at full scale (level 1.000 = at least +{:.0} dBFS)",
                        drawn.len(),
                        crate::meters::OVERLOAD_HEADROOM_DB
                    ),
                )
                .with_hint(
                    "either the buses carry far more than full scale (sum of several strips, or a \
                     doubled route) or a capture is being read with the wrong sample format — \
                     the log says “impossible block” in that case",
                ),
            );
        }

        // --- clock / processing errors (the crackle signature) ----------------
        //
        // Port of the Python §6 rules, fed by `status.timing` instead of a
        // `pw-dump`/`pw-top` call from the UI: the *daemon* runs the tools (it
        // already owns the PipeWire connection), and only publishes what it
        // really got.  `small_quantum` fires on a quantum we know (settings or a
        // live `pw-top` row) below `MIN_HEALTHY_QUANTUM`.
        let timing = &engine.timing;
        let mut small: Vec<String> = Vec::new();
        if timing.known() && timing.quantum < MIN_HEALTHY_QUANTUM {
            small.push(format!(
                "effective quantum {} @ {} kHz ({:.1} ms)",
                timing.quantum,
                timing.rate as f64 / 1000.0,
                timing.quantum_ms
            ));
        }
        // Live per-node values, when `pw-top` was available: a node may run at a
        // quantum the settings do not show (a client asking for low latency).
        for node in &engine.holaf_nodes {
            if let Some(quantum) = timing.quantum_of(node) {
                if quantum < MIN_HEALTHY_QUANTUM {
                    small.push(format!("{node} (quantum {quantum})"));
                }
            }
        }
        if !small.is_empty() {
            findings.push(
                Finding::new(
                    "small_quantum",
                    Severity::Warning,
                    format!("quantum below {MIN_HEALTHY_QUANTUM}: {}", small.join(", ")),
                )
                .with_hint(
                    "a very small quantum overruns easily on a busy CPU (crackling): raise \
                     clock.quantum (pw-metadata -n settings 0 clock.force-quantum 0) or lower \
                     the sample rate",
                ),
            );
        }

        if timing.pw_top_available && timing.pw_top_errors > 0 {
            let severity = if timing.pw_top_errors >= PWT_ERROR_THRESHOLD {
                Severity::Error
            } else {
                Severity::Warning
            };
            let mut worst: Vec<&TimingRow> = timing
                .pw_top_rows
                .iter()
                .filter(|row| row.err > 0)
                .collect();
            worst.sort_by_key(|row| std::cmp::Reverse(row.err));
            let detail = worst
                .iter()
                .take(3)
                .map(|row| row.describe_err())
                .collect::<Vec<_>>()
                .join(", ");
            findings.push(
                Finding::new(
                    "pwt_errors",
                    severity,
                    format!(
                        "{} processing error(s) reported by pw-top: {detail}",
                        timing.pw_top_errors
                    ),
                )
                .with_hint(
                    "xruns: the graph misses its deadline (CPU load, quantum, a hungry LADSPA \
                     chain)",
                ),
            );
        }

        // Only when the statistics are *really* missing, and with the daemon's
        // own reason (an old PipeWire has no `pw-top -b` at all).
        if let Some(reason) = &timing.pw_top_reason {
            if !timing.pw_top_available {
                findings.push(
                    Finding::new(
                        "pw_top_unavailable",
                        Severity::Info,
                        format!("live processing statistics unavailable: {reason}"),
                    )
                    .with_hint(
                        "not fatal — the quantum and the sample rate still come from \
                         pw-metadata settings",
                    ),
                );
            }
        }

        // --- daemons ----------------------------------------------------------
        if self.system.daemon_pids.len() > 1 {
            // A container or a long session can leave dozens of orphans behind;
            // naming the first five and counting the rest keeps the finding
            // readable (the full list is what `ps` is for).
            let pids = &self.system.daemon_pids;
            let shown = pids
                .iter()
                .take(5)
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let more = pids.len().saturating_sub(5);
            findings.push(
                Finding::new(
                    "multiple_daemons",
                    Severity::Warning,
                    if more > 0 {
                        format!(
                            "{} holafd daemons are alive ({shown}, … +{more} more)",
                            pids.len()
                        )
                    } else {
                        format!("{} holafd daemons are alive ({shown})", pids.len())
                    },
                )
                .with_hint("orphans from a previous session: the UI may talk to the wrong one — kill the extra pids"),
            );
        }

        findings.sort_by(findings_order);
        findings
    }

    pub fn errors(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == Severity::Error)
            .count()
    }

    pub fn warnings(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == Severity::Warning)
            .count()
    }

    pub fn infos(&self) -> usize {
        self.findings.len() - self.errors() - self.warnings()
    }

    /// The pasteable text report: findings first (that is what a bug report
    /// needs), then the details.
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        lines.push("=".repeat(78));
        lines.push("Holaf-Mix — diagnostic report".to_string());
        lines.push(format!("generated {}", format_timestamp(self.generated_at)));
        lines.push(format!(
            "app {} · ui egui {} · pid {}",
            self.system.app_version, APP_VERSION, self.system.app_pid
        ));
        lines.push("=".repeat(78));
        lines.push(String::new());

        lines.push(format!(
            "FINDINGS — {} error(s), {} warning(s), {} info",
            self.errors(),
            self.warnings(),
            self.infos()
        ));
        if self.findings.is_empty() {
            lines.push("  (none — everything looks healthy)".to_string());
        }
        for finding in &self.findings {
            lines.push(finding.line());
        }
        lines.push(String::new());

        lines.push("ENGINE".to_string());
        if !self.engine.available {
            lines.push(format!(
                "  unavailable: {}",
                self.engine.reason.as_deref().unwrap_or("unknown reason")
            ));
        } else {
            lines.push(format!(
                "  running        : {}",
                fmt_bool(self.engine.running)
            ));
            lines.push(format!(
                "  degraded       : {}",
                fmt_bool(self.engine.degraded)
            ));
            lines.push(format!(
                "  default sink   : {}",
                self.engine.default_sink.as_deref().unwrap_or("?")
            ));
            lines.push(format!(
                "  fx host        : mode={} pid={} respawns={} chains={}",
                self.engine.fx_host.mode,
                self.engine
                    .fx_host
                    .pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                self.engine.fx_host.respawns,
                self.engine.fx_host.chains.len()
            ));
            if let Some(pumps) = self.engine.mainloop_pumps {
                lines.push(format!("  mainloop pumps : {pumps}"));
            }
            // Clock + processing statistics: the numbers behind `small_quantum`
            // / `pwt_errors`, and the reason when they could not be read.
            lines.push(format!(
                "  timing         : {}",
                self.engine.timing.summary()
            ));
            for row in self
                .engine
                .timing
                .pw_top_rows
                .iter()
                .filter(|row| row.err > 0)
            {
                lines.push(format!("    - {}", row.describe_err()));
            }
            if !self.engine.pending.is_empty() {
                lines.push("  pending:".to_string());
                for item in &self.engine.pending {
                    lines.push(format!("    - {}: {}", item.key, item.message));
                }
            }
            lines.push(format!("  strips ({}):", self.engine.strips.len()));
            for strip in &self.engine.strips {
                lines.push(format!(
                    "    - {:<24} bus={:<28} device={:<40} vol={:.3} fx={}",
                    strip.label,
                    strip.node_name.as_deref().unwrap_or("?"),
                    strip.device_name.as_deref().unwrap_or("-"),
                    strip.volume,
                    if strip.active_effects.is_empty() {
                        "-".to_string()
                    } else {
                        strip.active_effects.join(",")
                    }
                ));
            }
        }
        lines.push(String::new());

        lines.push("GRAPH".to_string());
        lines.push(format!("  metering sources: {}", self.engine.meter_sources));
        if !self.meter_missing.is_empty() {
            lines.push(format!(
                "  no meter source : {}",
                self.meter_missing.join(", ")
            ));
        }
        if self.engine.holaf_nodes.is_empty() {
            lines.push("  Holaf_* nodes   : (none reported)".to_string());
        } else {
            lines.push(format!(
                "  Holaf_* nodes ({}):",
                self.engine.holaf_nodes.len()
            ));
            for node in &self.engine.holaf_nodes {
                lines.push(format!("    - {node}"));
            }
        }
        lines.push(String::new());

        lines.push("METERING".to_string());
        if self.meter_levels.is_empty() {
            lines.push("  (no strip is drawing a level)".to_string());
        } else {
            lines.push("  levels as the VU bars draw them (0.00-1.00):".to_string());
            for (uid, label, [left, right]) in &self.meter_levels {
                let source = if self.meter_missing.iter().any(|missing| missing == uid) {
                    "  (no capture source)"
                } else {
                    ""
                };
                lines.push(format!(
                    "    - {:<24} L={left:.3} R={right:.3}{source}",
                    label
                ));
            }
            // The dBFS the bar means: what a report reader needs to tell “this
            // strip is loud” from “this strip is at the top of the scale”.
            lines.push(format!(
                "  scale: {:.0} dBFS -> 0.00, 0 dBFS -> {:.2}, +{:.0} dBFS (overload) -> 1.00",
                crate::meters::LEVEL_FLOOR_DB,
                crate::meters::level_from_amplitude(1.0),
                crate::meters::OVERLOAD_HEADROOM_DB
            ));
        }
        lines.push(String::new());

        lines.push("DEVICES".to_string());
        if self.devices.is_empty() {
            lines.push("  (the engine listed none)".to_string());
        } else {
            for device in &self.devices {
                let missing = self
                    .missing_devices
                    .iter()
                    .find(|(_, _, declared)| declared == device);
                let suffix = if missing.is_some() {
                    " (referenced)"
                } else {
                    ""
                };
                lines.push(format!("    - {device}{suffix}"));
            }
        }
        for (uid, label, declared) in &self.missing_devices {
            lines.push(format!("  ! {label} ({uid}) → {declared} NOT FOUND"));
        }
        lines.push(String::new());

        lines.push("SYSTEM".to_string());
        lines.push(format!("  app version    : {}", self.system.app_version));
        lines.push(format!(
            "  PipeWire       : {}",
            self.system.pipewire_version.as_deref().unwrap_or("?")
        ));
        lines.push(format!("  OS             : {}", self.system.os));
        lines.push(format!(
            "  config         : {}",
            self.system.config_path.as_deref().unwrap_or("?")
        ));
        lines.push(format!(
            "  holafd pids    : {}",
            if self.system.daemon_pids.is_empty() {
                "none".to_string()
            } else {
                self.system
                    .daemon_pids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        lines.push(String::new());
        lines.join("\n")
    }
}

fn findings_order(a: &Finding, b: &Finding) -> std::cmp::Ordering {
    a.severity.cmp(&b.severity).then_with(|| a.id.cmp(b.id))
}

fn unique_sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

/// Whether the strip is bound to a physical device.
///
/// Mirrors [`holaf_core::graph::is_physical`], but on the engine's *facts* (a
/// [`StripFacts`], not a persisted [`Strip`]): a physical strip's master node is
/// the device, so it has **no** `Holaf_Bus_<Label>` node to look for.
fn is_physical(strip: &StripFacts) -> bool {
    strip
        .device_name
        .as_deref()
        .map(|device| !device.is_empty())
        .unwrap_or(false)
}

fn fmt_bool(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

/// First three pending items, then `… +N more` (the Python `_summarise_pending`).
pub fn summarise_pending(pending: &[PendingItem]) -> String {
    if pending.is_empty() {
        return "no details reported".to_string();
    }
    let mut parts: Vec<String> = pending
        .iter()
        .take(3)
        .map(|item| {
            let message = item
                .message
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let message = if message.chars().count() > 140 {
                format!("{}…", message.chars().take(137).collect::<String>())
            } else {
                message
            };
            format!("{} ({})", item.key, message)
        })
        .collect();
    let remaining = pending.len().saturating_sub(parts.len());
    if remaining > 0 {
        parts.push(format!("… +{remaining} more (see the ENGINE section)"));
    }
    parts.join("; ")
}

/// `status()` payload → [`EngineFacts`].
fn engine_facts(status: Option<&Value>, error: Option<&str>, strips: &[Strip]) -> EngineFacts {
    let Some(status) = status else {
        return EngineFacts {
            available: false,
            reason: Some(error.unwrap_or("no status was collected").to_string()),
            ..EngineFacts::default()
        };
    };
    let fx_host_value = status.get("fx_host");
    let mode = fx_host_value
        .and_then(|value| value.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let pid = fx_host_value
        .and_then(|value| value.get("pid"))
        .and_then(Value::as_i64);
    let alive = fx_host_value
        .and_then(|value| value.get("alive"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| !mode.eq_ignore_ascii_case("external") || pid.is_some());
    let fx_host = FxHost {
        mode,
        pid,
        alive,
        respawns: fx_host_value
            .and_then(|value| value.get("respawns"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        chains: fx_host_value
            .and_then(|value| value.get("chains"))
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    };

    let engine_strips: Vec<StripFacts> = status
        .get("strips")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let uid = entry.get("uid").and_then(Value::as_str)?.to_string();
                    let label = entry
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or(&uid)
                        .to_string();
                    // The UI's own strip list is the reference for the *label*
                    // (a rename may not have reached the engine yet) but the
                    // engine is authoritative for its node name.
                    let label = strips
                        .iter()
                        .find(|strip| strip.uid == uid)
                        .map(|strip| strip.label.clone())
                        .unwrap_or(label);
                    let active_effects = entry
                        .get("effects")
                        .and_then(Value::as_object)
                        .map(|effects| {
                            effects
                                .iter()
                                .filter(|(_, state)| {
                                    state
                                        .get("active")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false)
                                })
                                .map(|(key, _)| key.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(StripFacts {
                        uid,
                        label,
                        node_name: entry
                            .get("node_name")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        device_name: entry
                            .get("device_name")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        active_effects,
                        volume: entry.get("volume").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    EngineFacts {
        available: true,
        reason: None,
        running: status
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        degraded: status
            .get("degraded")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        default_sink: status
            .get("default_sink")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        pending: status
            .get("pending")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let key = entry.get("key").and_then(Value::as_str)?;
                        Some(PendingItem {
                            key: key.to_string(),
                            message: entry
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        fx_host,
        mainloop_pumps: status.get("mainloop_pumps").and_then(Value::as_u64),
        meter_sources: status
            .get("meter_sources")
            .and_then(Value::as_object)
            .map(|map| map.len())
            .unwrap_or(0),
        strips: engine_strips,
        holaf_nodes: status
            .get("holaf_nodes")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        timing: timing_facts(status.get("timing")),
    }
}

/// `status.timing` → [`TimingFacts`].  Absent payload = nothing known (an older
/// daemon), which the findings treat as "not reported", never as "quantum 0".
fn timing_facts(value: Option<&Value>) -> TimingFacts {
    let Some(timing) = value else {
        return TimingFacts::default();
    };
    let pw_top = timing.get("pw_top");
    TimingFacts {
        source: timing
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string),
        rate: timing
            .get("effective_rate")
            .or_else(|| timing.get("rate"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        quantum: timing
            .get("effective_quantum")
            .or_else(|| timing.get("quantum"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        quantum_ms: timing
            .get("quantum_ms")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        settings_error: timing
            .get("settings_error")
            .and_then(Value::as_str)
            .map(str::to_string),
        pw_top_available: pw_top
            .and_then(|top| top.get("available"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        pw_top_reason: pw_top
            .and_then(|top| top.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
        pw_top_errors: pw_top
            .and_then(|top| top.get("errors"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        pw_top_rows: pw_top
            .and_then(|top| top.get("rows"))
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .map(|row| TimingRow {
                        name: row.get("name").and_then(Value::as_str).map(str::to_string),
                        quantum: row.get("quantum").and_then(Value::as_u64).unwrap_or(0) as u32,
                        rate: row.get("rate").and_then(Value::as_u64).unwrap_or(0) as u32,
                        err: row.get("err").and_then(Value::as_u64).unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// `pkg-config --modversion libpipewire-0.3`, when the tool exists.
///
/// Spawning a process from a "pure" report is not free, so it is done once per
/// report (the dialog refreshes on demand only) and a failure is `None`, never
/// an error: the version line is a convenience.
pub fn pipewire_version() -> Option<String> {
    let output = std::process::Command::new("pkg-config")
        .args(["--modversion", "libpipewire-0.3"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// `uname -srm`, else the target triple this binary was built for.
pub fn os_description() -> String {
    let output = std::process::Command::new("uname").arg("-srm").output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !text.is_empty() {
                return text;
            }
            std::env::consts::OS.to_string()
        }
        _ => format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
    }
}

/// Pids of the live `holafd` processes (`/proc/*/comm`), sorted.
///
/// The Python collector asks `psutil`; here `/proc` is enough (`comm` is the
/// executable name, truncated to 15 chars, which `holafd` fits in).  A `/proc`
/// that cannot be read yields an empty list — never an error.
pub fn daemon_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if comm.trim() == "holafd" {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids
}

/// `1700000000` → `2023-11-14 22:13:20 UTC`.
///
/// A tiny civil-from-days conversion (Howard Hinnant's algorithm) instead of a
/// date crate: the report needs one line, not a dependency.
pub fn format_timestamp(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let seconds_of_day = unix % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

/// Days since the Unix epoch → `(year, month, day)` (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Current Unix time (0 if the clock is before the epoch — never panics).
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holaf_core::model::{Strip, StripType};
    use serde_json::json;

    fn healthy_status() -> Value {
        json!({
            "running": true,
            "default_sink": "Holaf_Bus_Mic",
            "degraded": false,
            "pending": [],
            "mainloop_pumps": 42,
            "fx_host": {"mode": "external", "pid": 4242, "respawns": 0 },
            "holaf_nodes": ["Holaf_Bus_Mic", "Holaf_Bus_Casque"],
            "meter_sources": {"in-mic": "Holaf_Bus_Mic.monitor"},
            "strips": [
                {"uid": "in-mic", "label": "Mic", "node_name": "Holaf_Bus_Mic",
                 "device_name": null, "volume": 0.85,
                 "effects": {"gate": {"active": true}, "eq": {"active": false}}},
                {"uid": "out-casque", "label": "Casque", "node_name": "Holaf_Bus_Casque",
                 "device_name": "alsa_output.usb.analog-stereo", "volume": 0.75,
                 "effects": {}}
            ]
        })
    }

    fn strips() -> Vec<Strip> {
        vec![
            Strip::with_uid("in-mic", "Mic", StripType::Input),
            Strip::with_uid("out-casque", "Casque", StripType::Output),
        ]
    }

    /// A report built from `status` with one metering source and no live
    /// `holafd` (the machine's `/proc` must never leak into a unit test).
    fn collect(status: Option<&Value>, error: Option<&str>) -> Report {
        let strips = strips();
        let sources: HashMap<String, String> =
            [("in-mic".to_string(), "Holaf_Bus_Mic.monitor".to_string())]
                .into_iter()
                .collect();
        Report::collect(&Inputs {
            status,
            status_error: error,
            strips: &strips,
            meter_sources: &sources,
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        })
    }

    #[test]
    fn a_healthy_engine_produces_no_error_finding() {
        let status = healthy_status();
        let report = collect(Some(&status), None);
        assert!(report.engine.available);
        assert!(report.engine.running);
        assert!(!report.engine.degraded);
        assert_eq!(report.errors(), 0, "{:?}", report.findings);
        assert_eq!(report.warnings(), 0, "{:?}", report.findings);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert!(report.text().contains("(none — everything looks healthy)"));
    }

    /// **The METERING section** — the one the first report was missing, which
    /// made “the VUs are full” indistinguishable from “the levels never arrive”.
    /// It prints the level each bar draws, the strips without a capture source,
    /// and the dBFS the scale means.
    #[test]
    fn the_report_shows_what_every_vu_bar_draws() {
        let status = healthy_status();
        let strips = strips();
        let levels = vec![
            ("in-mic".to_string(), "Mic".to_string(), [0.301_f32, 0.299]),
            ("out-casque".to_string(), "Casque".to_string(), [0.0, 0.0]),
        ];
        let report = Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            meter_levels: &levels,
            meter_missing: &["out-casque".to_string()],
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        });
        let text = report.text();
        assert!(text.contains("\nMETERING"), "{text}");
        assert!(
            text.contains("Mic") && text.contains("L=0.301 R=0.299"),
            "{text}"
        );
        // A strip with no capture source says so instead of showing a fake level.
        assert!(
            text.contains("Casque") && text.contains("(no capture source)"),
            "{text}"
        );
        // The scale is spelled out, so a reader can tell 0.73 from “full”.
        assert!(text.contains("0 dBFS -> 0.91"), "{text}");

        // A report with no strips drawing anything says so (never an empty
        // heading that reads like a truncated report).
        let empty = Report::collect(&Inputs::empty(&[]));
        assert!(
            empty.text().contains("(no strip is drawing a level)"),
            "{}",
            empty.text()
        );
    }

    /// **Every bar glued at the top** is a diagnosis, not a mix: it means at
    /// least +6 dBFS everywhere (a summed bus, a doubled route) or a capture read
    /// with the wrong sample format.  The report must say it instead of leaving
    /// the user with “les VU sont toujours à fond”.
    #[test]
    fn every_bar_at_full_scale_is_reported_as_a_finding() {
        let status = healthy_status();
        let strips = strips();
        let pinned = vec![
            ("in-mic".to_string(), "Mic".to_string(), [1.0_f32, 1.0]),
            ("out-casque".to_string(), "Casque".to_string(), [1.0, 1.0]),
        ];
        let report = Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            meter_levels: &pinned,
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        });
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "metering_pinned")
            .expect("metering_pinned");
        assert_eq!(finding.severity, Severity::Warning);
        assert!(finding.message.contains("+6 dBFS"), "{}", finding.message);

        // A normal mix (anything below the top) raises nothing.
        let normal = vec![
            ("in-mic".to_string(), "Mic".to_string(), [0.73_f32, 0.70]),
            ("out-casque".to_string(), "Casque".to_string(), [0.66, 0.65]),
        ];
        let report = Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            meter_levels: &normal,
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        });
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.id == "metering_pinned"),
            "{:?}",
            report.findings
        );
    }

    /// The `timing` block the daemon publishes (`pw-metadata -n settings`), by
    /// default: 1024 samples at 48 kHz and no `pw-top` batch mode.
    fn healthy_timing() -> Value {
        json!({
            "source": "pw-metadata settings",
            "rate": 48000,
            "quantum": 1024,
            "force_quantum": 0,
            "effective_rate": 48000,
            "effective_quantum": 1024,
            "quantum_ms": 21.3333,
            "min_quantum": 16,
            "max_quantum": 2048,
            "settings_error": null,
            "pw_top": {
                "available": false,
                "reason": "pw-top has no batch mode (needs PipeWire >= 0.3.66)",
                "errors": 0,
                "rows": [],
            },
        })
    }

    fn with_timing(timing: Value) -> Value {
        let mut status = healthy_status();
        status["timing"] = timing;
        status
    }

    /// A forced quantum of 64 samples is the crackle case: the finding is a
    /// WARNING that names the value.
    #[test]
    fn a_quantum_below_256_is_a_warning() {
        let mut timing = healthy_timing();
        timing["force_quantum"] = json!(64);
        timing["effective_quantum"] = json!(64);
        timing["quantum_ms"] = json!(1.3333);
        let status = with_timing(timing);
        let report = collect(Some(&status), None);
        assert_eq!(report.engine.timing.quantum, 64);
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "small_quantum")
            .unwrap_or_else(|| panic!("no small_quantum in {:?}", report.findings));
        assert_eq!(finding.severity, Severity::Warning);
        assert!(finding.message.contains("below 256"), "{}", finding.message);
        assert!(
            finding.message.contains("quantum 64"),
            "{}",
            finding.message
        );
        assert!(finding.hint.is_some());
    }

    /// 512 samples is perfectly healthy: nothing is reported.
    #[test]
    fn a_healthy_quantum_reports_nothing() {
        let mut timing = healthy_timing();
        timing["effective_quantum"] = json!(512);
        timing["quantum_ms"] = json!(10.6667);
        let status = with_timing(timing);
        let report = collect(Some(&status), None);
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.id == "small_quantum"),
            "{:?}",
            report.findings
        );
        assert_eq!(
            report.engine.timing.summary().split(" — ").next(),
            Some("quantum 512 @ 48 kHz (10.7 ms)")
        );
    }

    /// A node may run below the settings' quantum (a client asking for low
    /// latency): when `pw-top` says so, the finding names the node.
    #[test]
    fn a_small_live_quantum_on_a_holaf_node_is_reported() {
        let mut timing = healthy_timing();
        timing["pw_top"] = json!({
            "available": true,
            "reason": null,
            "errors": 0,
            "rows": [
                {"id": 52, "name": "Holaf_Bus_Mic", "state": "R", "quantum": 128,
                 "rate": 48000, "wait": "1.1us", "busy": "0.7us", "err": 0, "format": "F32LE"},
            ],
        });
        let status = with_timing(timing);
        let report = collect(Some(&status), None);
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "small_quantum")
            .expect("a live 128-sample node is reported");
        assert!(
            finding.message.contains("Holaf_Bus_Mic (quantum 128)"),
            "{}",
            finding.message
        );
    }

    /// `pwt_errors` is a warning, and an ERROR from 100 xruns (Python
    /// `PWT_ERROR_THRESHOLD`), naming the worst rows.
    #[test]
    fn pw_top_errors_scale_with_the_count() {
        let rows = json!([
            {"id": 1, "name": "Holaf_Bus_Mic", "state": "R", "quantum": 1024,
             "rate": 48000, "wait": "1us", "busy": "1us", "err": 120, "format": "F32LE"},
            {"id": 2, "name": "Holaf_Bus_Casque", "state": "R", "quantum": 1024,
             "rate": 48000, "wait": "1us", "busy": "1us", "err": 30, "format": "F32LE"},
        ]);
        let mut timing = healthy_timing();
        timing["pw_top"] = json!({
            "available": true, "reason": null, "errors": 150, "rows": rows,
        });
        let status = with_timing(timing);
        let report = collect(Some(&status), None);
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "pwt_errors")
            .unwrap_or_else(|| panic!("no pwt_errors in {:?}", report.findings));
        assert_eq!(finding.severity, Severity::Error, "150 ≥ 100");
        assert!(finding.message.contains("150 processing error(s)"));
        assert!(finding.message.contains("Holaf_Bus_Mic (ERR 120)"));
        assert!(finding.message.contains("Holaf_Bus_Casque (ERR 30)"));

        // A few xruns are a warning, not an error.
        let mut timing = healthy_timing();
        timing["pw_top"] = json!({
            "available": true, "reason": null, "errors": 3,
            "rows": [{"id": 1, "name": "Holaf_Bus_Mic", "quantum": 1024, "rate": 48000, "err": 3}],
        });
        let status = with_timing(timing);
        let report = collect(Some(&status), None);
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "pwt_errors")
            .expect("3 xruns are still reported");
        assert_eq!(finding.severity, Severity::Warning);
    }

    /// The information finding exists **only** when the statistics really could
    /// not be collected, and it repeats the daemon's reason verbatim.
    #[test]
    fn pw_top_unavailable_is_reported_with_its_reason() {
        let status = with_timing(healthy_timing());
        let report = collect(Some(&status), None);
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "pw_top_unavailable")
            .unwrap_or_else(|| panic!("no pw_top_unavailable in {:?}", report.findings));
        assert_eq!(finding.severity, Severity::Info);
        assert!(
            finding
                .message
                .contains("pw-top has no batch mode (needs PipeWire >= 0.3.66)"),
            "{}",
            finding.message
        );

        // Available → no information finding at all.
        let mut timing = healthy_timing();
        timing["pw_top"] = json!({"available": true, "reason": null, "errors": 0, "rows": []});
        let status = with_timing(timing);
        let report = collect(Some(&status), None);
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.id == "pw_top_unavailable"),
            "{:?}",
            report.findings
        );
    }

    /// A daemon that predates `timing` must not make the report claim a quantum:
    /// nothing is reported rather than "quantum 0".
    #[test]
    fn a_status_without_timing_reports_no_clock_findings() {
        let status = healthy_status();
        let report = collect(Some(&status), None);
        assert!(!report.engine.timing.known());
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert!(report
            .engine
            .timing
            .summary()
            .starts_with("quantum unknown"));
    }

    #[test]
    fn a_degraded_graph_names_the_pending_work() {
        let mut status = healthy_status();
        status["degraded"] = json!(true);
        status["pending"] = json!([
            {"key": "strip:out-casque", "message": "device alsa_output.usb.analog-stereo not found"},
            {"key": "route:in-mic->out-casque", "message": "target port missing"},
        ]);
        let report = collect(Some(&status), None);
        let ids: Vec<&str> = report.findings.iter().map(|f| f.id).collect();
        assert!(ids.contains(&"graph_degraded"), "{ids:?}");
        assert!(!ids.contains(&"graph_pending"), "degraded wins: {ids:?}");
        let finding = report
            .findings
            .iter()
            .find(|f| f.id == "graph_degraded")
            .unwrap();
        assert_eq!(finding.severity, Severity::Error);
        assert!(
            finding.message.contains("strip:out-casque"),
            "{}",
            finding.message
        );
        assert!(finding.message.contains("route:in-mic->out-casque"));
    }

    #[test]
    fn pending_work_without_the_flag_is_still_reported() {
        let mut status = healthy_status();
        status["pending"] = json!([{"key": "strip:a", "message": "retrying"}]);
        let report = collect(Some(&status), None);
        let finding = report
            .findings
            .iter()
            .find(|f| f.id == "graph_pending")
            .expect("graph_pending");
        assert_eq!(finding.severity, Severity::Warning);
        // The summary caps at three items and counts the rest.
        let many: Vec<PendingItem> = (0..5)
            .map(|i| PendingItem {
                key: format!("strip:{i}"),
                message: "x".to_string(),
            })
            .collect();
        let summary = summarise_pending(&many);
        assert!(summary.contains("+2 more"), "{summary}");
    }

    #[test]
    fn an_unreachable_engine_is_a_single_error() {
        let report = collect(None, Some("cannot connect to PipeWire"));
        assert!(!report.engine.available);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].id, "daemon_unreachable");
        assert_eq!(report.findings[0].severity, Severity::Error);
        assert!(report.findings[0].message.contains("cannot connect"));
        // Only the reachability is judged: no `engine_not_running` noise.
        assert!(report.text().contains("daemon_unreachable"));
    }

    #[test]
    fn fx_host_and_mainloop_findings_match_the_python_ids() {
        // in-process hosting.
        let mut status = healthy_status();
        status["fx_host"] = json!({"mode": "inprocess", "pid": std::process::id(), "respawns": 0});
        let report = collect(Some(&status), None);
        assert!(report.findings.iter().any(|f| f.id == "fx_host_inprocess"));

        // external host gone.
        let mut status = healthy_status();
        status["fx_host"] = json!({"mode": "external", "pid": null, "respawns": 0});
        let report = collect(Some(&status), None);
        let finding = report
            .findings
            .iter()
            .find(|f| f.id == "fx_host_down")
            .expect("fx_host_down");
        assert_eq!(finding.severity, Severity::Error);

        // respawns: warning under 3, error from 3.
        let mut status = healthy_status();
        status["fx_host"] = json!({"mode": "external", "pid": 12, "respawns": 2});
        let report = collect(Some(&status), None);
        assert_eq!(
            report
                .findings
                .iter()
                .find(|f| f.id == "fx_host_respawns")
                .unwrap()
                .severity,
            Severity::Warning
        );
        status["fx_host"] = json!({"mode": "external", "pid": 12, "respawns": 3});
        let report = collect(Some(&status), None);
        assert_eq!(
            report
                .findings
                .iter()
                .find(|f| f.id == "fx_host_respawns")
                .unwrap()
                .severity,
            Severity::Error
        );

        // A dead mainloop with chains hosted **in-process** is a warning…
        let mut status = healthy_status();
        status["mainloop_pumps"] = json!(0);
        status["fx_host"] =
            json!({"mode": "inprocess", "pid": 12, "respawns": 0, "chains": ["a", "b"]});
        let report = collect(Some(&status), None);
        assert!(report
            .findings
            .iter()
            .any(|f| f.id == "mainloop_pumps_stalled"));
        // …and only an info when nothing is hosted in-process.
        status["fx_host"] = json!({"mode": "inprocess", "pid": 12, "respawns": 0, "chains": []});
        let report = collect(Some(&status), None);
        let finding = report
            .findings
            .iter()
            .find(|f| f.id == "mainloop_pumps_zero")
            .expect("mainloop_pumps_zero");
        assert_eq!(finding.severity, Severity::Info);
    }

    #[test]
    fn a_zero_pump_count_is_normal_with_an_external_fx_host() {
        // Regression: `fx host: mode=external` means the daemon hosts no chain —
        // its mainloop counter legitimately stays at 0. Reporting a stall there
        // was a false positive that buried the real findings (a real report had
        // it while the only actual problem was pw-top errors).
        let mut status = healthy_status();
        status["mainloop_pumps"] = json!(0);
        status["fx_host"] =
            json!({"mode": "external", "pid": 12, "respawns": 0, "chains": ["a", "b"]});
        let report = collect(Some(&status), None);
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.id == "mainloop_pumps_stalled"),
            "external FX host must not be reported as a stalled mainloop"
        );
        let finding = report
            .findings
            .iter()
            .find(|f| f.id == "mainloop_pumps_external")
            .expect("mainloop_pumps_external");
        assert_eq!(finding.severity, Severity::Info);
        assert!(finding.message.contains("external"), "{}", finding.message);

        // No chain at all, still external: nothing to say (not even an info).
        status["fx_host"] = json!({"mode": "external", "pid": 12, "respawns": 0, "chains": []});
        let report = collect(Some(&status), None);
        let ids: Vec<&str> = report.findings.iter().map(|f| f.id).collect();
        assert!(!ids.contains(&"mainloop_pumps_stalled"), "{ids:?}");
        assert!(!ids.contains(&"mainloop_pumps_zero"), "{ids:?}");
        assert!(!ids.contains(&"mainloop_pumps_external"), "{ids:?}");
    }

    #[test]
    fn stale_bus_names_and_missing_buses_are_reported() {
        let mut status = healthy_status();
        status["strips"][0]["node_name"] = json!("Holaf_Strip_in-mic");
        status["holaf_nodes"] = json!(["Holaf_Strip_in-mic", "Holaf_Bus_Casque"]);
        let report = collect(Some(&status), None);
        let finding = report
            .findings
            .iter()
            .find(|f| f.id == "legacy_bus_names")
            .expect("legacy_bus_names");
        assert!(finding.message.contains("Holaf_Strip_in-mic"));

        // `in-mic`'s bus is now named `Holaf_Bus_Mic` in the strip list but the
        // graph only has `Holaf_Bus_Casque`: the bus is missing.
        let mut status = healthy_status();
        status["holaf_nodes"] = json!(["Holaf_Bus_Casque"]);
        let report = collect(Some(&status), None);
        let finding = report
            .findings
            .iter()
            .find(|f| f.id == "strip_bus_missing")
            .expect("strip_bus_missing");
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.message.contains("Holaf_Bus_Mic"));

        // A daemon that publishes no node_name at all is stale.
        let mut status = healthy_status();
        status["strips"][0]
            .as_object_mut()
            .unwrap()
            .remove("node_name");
        status["strips"][1]
            .as_object_mut()
            .unwrap()
            .remove("node_name");
        let report = collect(Some(&status), None);
        assert!(report.findings.iter().any(|f| f.id == "strip_bus_unnamed"));
    }

    #[test]
    fn a_physical_strip_has_no_bus_to_miss() {
        // Regression: the engine publishes `node_name` for EVERY strip (it is
        // the public handle), but a physical strip's master node is its DEVICE —
        // no `Holaf_Bus_*` node is ever created for it. A real report flagged
        // Casque/HP/Mic as "bus missing" for exactly that reason.
        let mut status = healthy_status();
        // Only the virtual strip's bus is in the graph: `Holaf_Bus_Casque`
        // (physical strip `out-casque`) is legitimately absent.
        status["holaf_nodes"] = json!(["Holaf_Bus_Mic"]);
        let report = collect(Some(&status), None);
        let ids: Vec<&str> = report.findings.iter().map(|f| f.id).collect();
        assert!(!ids.contains(&"strip_bus_missing"), "{ids:?}");
        // The device it is bound to is listed, so no `device_missing` either.
        assert!(!ids.contains(&"device_missing"), "{ids:?}");

        // The same strip with its device gone IS an error — from
        // `device_missing`, the rule that actually applies to it.
        let strips = strips();
        let devices = vec![DeviceInfo {
            id: 1,
            name: "alsa_output.pci.analog-stereo".to_string(),
            media_class: "Audio/Sink".to_string(),
            description: None,
        }];
        let missing = vec![(
            "out-casque".to_string(),
            "Casque".to_string(),
            "alsa_output.usb.analog-stereo".to_string(),
        )];
        let report = Report::collect(&Inputs {
            status: Some(&status),
            devices: &devices,
            strips: &strips,
            missing_devices: &missing,
            meter_missing: &["in-mic".to_string()],
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        });
        let ids: Vec<&str> = report.findings.iter().map(|f| f.id).collect();
        assert!(ids.contains(&"device_missing"), "{ids:?}");
        assert!(!ids.contains(&"strip_bus_missing"), "{ids:?}");

        // A *virtual* strip still gets the bus check.
        let mut status = healthy_status();
        status["holaf_nodes"] = json!(["Holaf_Bus_Casque"]);
        let report = collect(Some(&status), None);
        let finding = report
            .findings
            .iter()
            .find(|f| f.id == "strip_bus_missing")
            .expect("strip_bus_missing");
        assert!(finding.message.contains("Holaf_Bus_Mic"));
        assert!(finding.message.contains("virtual"));
    }

    #[test]
    fn a_missing_device_and_a_sourceless_meter_are_reported() {
        let mut status = healthy_status();
        // No metering source registered by the engine.
        status["meter_sources"] = json!({});
        let strips = strips();
        let devices = vec![DeviceInfo {
            id: 1,
            name: "alsa_output.pci.analog-stereo".to_string(),
            media_class: "Audio/Sink".to_string(),
            description: None,
        }];
        let missing = vec![(
            "out-casque".to_string(),
            "Casque".to_string(),
            "alsa_output.usb.analog-stereo".to_string(),
        )];
        let report = Report::collect(&Inputs {
            status: Some(&status),
            devices: &devices,
            strips: &strips,
            missing_devices: &missing,
            meter_missing: &["in-mic".to_string()],
            config_path: Some(Path::new("/tmp/config.json")),
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        });
        let ids: Vec<&str> = report.findings.iter().map(|f| f.id).collect();
        assert!(ids.contains(&"device_missing"), "{ids:?}");
        assert!(ids.contains(&"metering_no_source"), "{ids:?}");
        assert!(report.text().contains("NOT FOUND"));
        assert!(report.text().contains("no meter source : in-mic"));
        assert!(report.text().contains("config         : /tmp/config.json"));
        // A device the engine lists but no strip references is not a finding.
        assert!(!ids.contains(&"device_not_listed"));
    }

    #[test]
    fn the_text_report_puts_the_findings_first() {
        let mut status = healthy_status();
        status["degraded"] = json!(true);
        status["pending"] = json!([{"key": "strip:x", "message": "boom"}]);
        let report = collect(Some(&status), None);
        let text = report.text();
        let findings_at = text.find("FINDINGS").expect("a FINDINGS section");
        let engine_at = text.find("\nENGINE").expect("an ENGINE section");
        let graph_at = text.find("\nGRAPH").expect("a GRAPH section");
        let system_at = text.find("\nSYSTEM").expect("a SYSTEM section");
        assert!(findings_at < engine_at && engine_at < graph_at && graph_at < system_at);
        assert!(text.starts_with("="));
        assert!(text.contains("Holaf-Mix — diagnostic report"));
        // The finding is quoted with its id and its hint arrow.
        assert!(text.contains("graph_degraded"));
        assert!(text.contains("→"));
        // The strip table lists the engine's bus name and the UI's label.
        assert!(text.contains("Holaf_Bus_Mic"));
    }

    #[test]
    fn findings_are_sorted_by_severity_then_id() {
        let mut status = healthy_status();
        status["degraded"] = json!(true);
        status["pending"] = json!([{"key": "k", "message": "m"}]);
        status["fx_host"] = json!({"mode": "inprocess", "pid": 1, "respawns": 4});
        status["mainloop_pumps"] = json!(0);
        let report = collect(Some(&status), None);
        let mut sorted = report.findings.clone();
        sorted.sort_by(findings_order);
        assert_eq!(report.findings, sorted);
        let severities: Vec<Severity> = report.findings.iter().map(|f| f.severity).collect();
        assert!(severities.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn timestamps_are_formatted_without_a_date_crate() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_timestamp(1_700_000_000), "2023-11-14 22:13:20 UTC");
        // A leap day, and the end of a leap year.
        assert_eq!(format_timestamp(1_709_164_800), "2024-02-29 00:00:00 UTC");
        assert_eq!(format_timestamp(1_735_689_599), "2024-12-31 23:59:59 UTC");
    }

    #[test]
    fn several_daemons_are_reported_as_an_ambiguity() {
        let status = healthy_status();
        let strips = strips();
        let report = Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            now_unix: 1_700_000_000,
            daemon_pids: Some(vec![100, 200]),
            ..Inputs::empty(&strips)
        });
        let finding = report
            .findings
            .iter()
            .find(|f| f.id == "multiple_daemons")
            .expect("multiple_daemons");
        assert!(finding.message.contains("100, 200"), "{}", finding.message);
        // A single daemon (the normal case) is not a finding.
        let report = Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            now_unix: 1_700_000_000,
            daemon_pids: Some(vec![100]),
            ..Inputs::empty(&strips)
        });
        assert!(!report.findings.iter().any(|f| f.id == "multiple_daemons"));
    }

    #[test]
    fn the_system_facts_never_panic() {
        // These read /proc and spawn `pkg-config`/`uname`: they must tolerate a
        // container without them (the version is then just `None`).
        let version = pipewire_version();
        assert!(version.is_none() || version.as_deref().is_some_and(|v| !v.is_empty()));
        assert!(!os_description().is_empty());
        let pids = daemon_pids();
        assert!(pids.windows(2).all(|pair| pair[0] < pair[1]), "{pids:?}");
        assert!(now_unix() > 1_600_000_000);
    }
}
