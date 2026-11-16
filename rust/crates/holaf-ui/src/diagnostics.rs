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

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use holaf_core::model::Strip;
use serde_json::Value;

use crate::engine_json::DeviceInfo;
use crate::meters::MeterSample;

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
    /// Is the strip muted?  A muted strip's VU bar is dark **by design**
    /// (`app::feed_from_board` forces zero), so a report that shows seven bars
    /// at 0.000 must say how many strips were muted — otherwise the user reads
    /// “the metering is broken” on a perfectly healthy chain.
    pub mute: bool,
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

/// Environment variable overriding the recording client (`pw-cat`).
pub const RECORDER_ENV: &str = "HOLAF_PW_CAT";

/// The recording client the captures spawn when [`RECORDER_ENV`] is unset.
pub const DEFAULT_RECORDER: &str = "pw-cat";

/// Which recording client the captures really use, as resolved on this machine.
///
/// The whole VU chain hangs off one external binary, so a report that does not
/// say **which** binary — and whether it exists at all — cannot explain a dark
/// meter.  `path == None` is the “pw-cat is not installed” case (on Arch it
/// lives in `pipewire-audio`, not in the base `pipewire` package).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecorderProbe {
    /// `HOLAF_PW_CAT` when it is set (the effective override).
    pub env_override: Option<String>,
    /// The command spawned: the override, else [`DEFAULT_RECORDER`].
    pub command: String,
    /// Absolute path found in `PATH`; `None` = **not installed**.
    pub path: Option<String>,
    /// First line of `<command> --version`, when it could be read.
    pub version: Option<String>,
    /// `PIPEWIRE_REMOTE` (the daemon name/address `pw-cat` will connect to).
    ///
    /// The engine and the recorder are two different clients of PipeWire: if
    /// they do not reach the **same** daemon, every `--target` resolves to
    /// nothing, `pw-cat` records the default source and the meters stay dark
    /// while the graph is healthy.  Printing it costs one line and removes the
    /// guesswork.
    pub remote: Option<String>,
    /// `XDG_RUNTIME_DIR` (where the default socket lives), then
    /// `PIPEWIRE_RUNTIME_DIR` when it is set.
    pub runtime_dir: Option<String>,
    /// Does the recorder accept `--raw` (the flag that stops PipeWire 1.4+ from
    /// wrapping samples in a libsndfile `.au` container)?
    ///
    /// `None` when it could not be probed (no binary to ask).  `Some(false)` on
    /// a `pw-cat` older than 1.4 is *fine*: those versions already write raw
    /// samples to `-`.  The point of printing it is that the fix for “the
    /// recorder writes s16le” is exactly this flag, so a report must show whether
    /// it was used.
    pub raw: Option<bool>,
}

impl RecorderProbe {
    /// `pw-cat -> /usr/bin/pw-cat (version 1.6.11)` / `pw-cat -> NOT FOUND in
    /// PATH` — the one line the CAPTURES section opens with.
    ///
    /// ASCII on purpose: this string is also painted by the Graph tab, and the
    /// bundled egui font has no `→` (it draws a tofu box).
    pub fn summary(&self) -> String {
        let command = if self.command.is_empty() {
            DEFAULT_RECORDER
        } else {
            self.command.as_str()
        };
        let mut line = match &self.path {
            Some(path) => format!("{command} -> {path}"),
            None => format!("{command} -> NOT FOUND in PATH"),
        };
        if let Some(version) = &self.version {
            line.push_str(&format!(" (version {version})"));
        }
        line.push_str(&format!(
            " — {RECORDER_ENV} {}",
            match &self.env_override {
                Some(value) => format!("={value}"),
                None => "unset".to_string(),
            }
        ));
        // Whether `--raw` is driven: the one flag that stops PipeWire 1.4+ from
        // wrapping the samples in a libsndfile container (the “VU à zéro” cause
        // on 1.6).  Printed even when `false` (a pre-1.4 `pw-cat` already writes
        // raw samples, so `false` there is not a problem — only a fact).
        match self.raw {
            Some(true) => line.push_str(" — raw mode: yes"),
            Some(false) => line.push_str(" — raw mode: no (--raw not offered)"),
            None => line.push_str(" — raw mode: not probed"),
        }
        line
    }

    /// The daemon/socket the recorder will connect to, on one line
    /// (`remote default · XDG_RUNTIME_DIR=/run/user/1000`).
    ///
    /// A `pw-cat` that reaches *another* daemon than the engine resolves no
    /// `--target` at all and the bars stay dark with a healthy graph — this line
    /// is what makes that visible in a pasted report.
    pub fn endpoint(&self) -> String {
        format!(
            "remote {} · {}",
            self.remote.as_deref().unwrap_or("default"),
            match &self.runtime_dir {
                Some(dir) => format!("XDG_RUNTIME_DIR={dir}"),
                None => "XDG_RUNTIME_DIR unset".to_string(),
            }
        )
    }

    /// Can a capture possibly start?
    pub fn found(&self) -> bool {
        self.path.is_some()
    }
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

// ---------------------------------------------------------------------------
// Capture status — the VU chain's own health
// ---------------------------------------------------------------------------
//
// The report could say *what* the bars draw but not *why* they draw 0.000: a
// capture that never spawned (no `pw-cat`), one that died at once, one whose
// blocks were all dropped as implausible (a stream read with the wrong sample
// format) and a genuine silence all looked identical in the METERING section —
// which is exactly the report that came back from the user's machine (“METERING:
// TOUS les niveaux à 0.000”).
//
// So the capture children publish their own state (`crate::capture`) into the
// shared [`CaptureRegistry`] below, and the report prints it in the CAPTURES
// section.  Nothing in the VU chain can fail silently any more.

/// How many `pw-cat` stderr lines are kept per capture.
pub const STDERR_TAIL: usize = 5;

/// Why one capture block was **not** drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RejectReason {
    /// Decoded to a peak no audio path produces: the stream is not `f32le`.
    Implausible,
    /// Fewer bytes than one block arrived before the stream ended.
    Partial,
}

impl RejectReason {
    pub fn name(self) -> &'static str {
        match self {
            RejectReason::Implausible => "implausible",
            RejectReason::Partial => "partial",
        }
    }
}

/// Lifecycle of one capture child, as the report prints it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CaptureState {
    /// Spawned, not a single block decoded yet.
    #[default]
    Waiting,
    /// At least one block was decoded (and therefore drawn).
    Streaming,
    /// The stream ended: the child exited, or closed its stdout.
    Ended { detail: String },
    /// The child never started (missing binary, permission, …).
    Failed { reason: String },
}

impl CaptureState {
    pub fn name(&self) -> &'static str {
        match self {
            CaptureState::Waiting => "waiting",
            CaptureState::Streaming => "streaming",
            CaptureState::Ended { .. } => "ended",
            CaptureState::Failed { .. } => "failed",
        }
    }

    /// Is this capture definitely not feeding its bar any more?
    pub fn dead(&self) -> bool {
        matches!(self, CaptureState::Ended { .. } | CaptureState::Failed { .. })
    }

    /// The reason an ended/failed capture gives (`None` while it runs).
    pub fn detail(&self) -> Option<&str> {
        match self {
            CaptureState::Ended { detail } => Some(detail),
            CaptureState::Failed { reason } => Some(reason),
            CaptureState::Waiting | CaptureState::Streaming => None,
        }
    }
}

/// One capture child, as the report prints it.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureFacts {
    /// Strip uid the capture feeds.
    pub uid: String,
    /// Source the engine named (PulseAudio vocabulary: `<node>.monitor`).
    pub source: String,
    /// What `pw-cat --target` really received (the node name).
    pub target: String,
    /// The **exact** command line that was spawned, quoted for a shell.
    ///
    /// This is the single most actionable line of the whole section: the user can
    /// paste it and see what `pw-cat` really does on their machine (whether
    /// `-P '{"stream.capture.sink":true}'` is accepted there, whether the target
    /// resolves, what format comes out).
    pub command: String,
    /// Is the target a **sink monitor** (`-P stream.capture.sink=true`)?
    pub monitor: bool,
    /// The child's pid, `None` when it never started.
    pub pid: Option<u32>,
    pub state: CaptureState,
    /// Blocks decoded and drawn.
    pub blocks: u64,
    /// Blocks refused (see [`RejectReason`]).
    pub rejected: u64,
    pub last_rms: Option<[f32; 2]>,
    pub last_peak: Option<[f32; 2]>,
    /// Seconds since the last block (`None` = never).
    pub since_last_block: Option<Duration>,
    /// The last [`STDERR_TAIL`] non-empty lines the child wrote.
    pub stderr_tail: Vec<String>,
    /// Best-effort guess at the real byte format, when its blocks were refused.
    pub format_guess: Option<String>,
    /// The sample format the capture **detected and converted** from.
    ///
    /// `None` while the stream is the nominal `f32le` (nothing to say).
    /// `Some("s16le")` means the recorder wrote s16le despite `--format f32` and
    /// every block is being converted — the VU works, and the report says why the
    /// bytes are not f32le (the “VU à zéro” report on PipeWire 1.6).
    pub detected_format: Option<String>,
}

impl CaptureFacts {
    /// Is this capture gone (spawn failed, or the stream ended)?
    pub fn dead(&self) -> bool {
        self.state.dead()
    }

    /// `pid 42 · streaming · 210 block(s), 0 rejected · last rms 0.0031/0.0030
    /// peak 0.0044/0.0044 (0.0 s ago)` — one line for the report and the Graph
    /// tab.
    pub fn describe(&self) -> String {
        let pid = self
            .pid
            .map(|pid| format!("pid {pid}"))
            .unwrap_or_else(|| "pid none".to_string());
        let mut line = format!(
            "{pid} · {} · {} block(s), {} rejected",
            self.state.name(),
            self.blocks,
            self.rejected
        );
        if let (Some(rms), Some(peak)) = (self.last_rms, self.last_peak) {
            line.push_str(&format!(
                " · last rms {:.4}/{:.4} peak {:.4}/{:.4}",
                rms[0], rms[1], peak[0], peak[1]
            ));
        }
        if let Some(since) = self.since_last_block {
            line.push_str(&format!(" ({:.1} s ago)", since.as_secs_f32()));
        } else {
            line.push_str(" (no block yet)");
        }
        if let Some(detail) = self.state.detail() {
            line.push_str(&format!(" — {detail}"));
        }
        if let Some(guess) = &self.format_guess {
            line.push_str(&format!(" — the bytes look like {guess}"));
        }
        if let Some(detected) = &self.detected_format {
            line.push_str(&format!(" · detected {detected} and converted"));
        }
        line
    }
}

/// Snapshot of every capture, for [`Report::captures`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaptureSummary {
    /// The recording client the captures spawn (`HOLAF_PW_CAT` or `pw-cat`).
    pub recorder: String,
    /// Blocks decoded **and drawn** since start-up.
    pub blocks: u64,
    /// Blocks refused since start-up.
    pub rejected: u64,
    /// `reason → count` (see [`RejectReason`]).
    pub rejects: BTreeMap<String, u64>,
    /// One entry per strip the engine asked for, sorted by uid.
    pub sources: Vec<CaptureFacts>,
    /// Why **no** capture was started, when that is the case (the engine
    /// refused `meter_sources`, reported none, or is not connected).  Written by
    /// the engine thread — the only place that knows — and printed by the
    /// CAPTURES section: “no capture is running” alone names no cause.
    pub note: Option<String>,
}

impl CaptureSummary {
    /// No capture was ever asked for (no engine, no source, mock build).
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Every capture is gone: failed to spawn, or its stream ended.
    pub fn all_dead(&self) -> bool {
        !self.sources.is_empty() && self.sources.iter().all(CaptureFacts::dead)
    }

    /// **No byte ever arrived**: not one block was decoded, and not one was
    /// refused either.  That is the dead-chain case (no `pw-cat`, a refused
    /// target, a stream that never opens) — a silent source still delivers
    /// blocks (all zeros), and a *refused* block proves the recorder did write
    /// something (a format mismatch, reported by `metering_implausible`).
    pub fn never_streamed(&self) -> bool {
        !self.sources.is_empty() && self.blocks == 0 && self.rejected == 0
    }

    /// How many captures are in `state` (`CaptureState::name`).
    pub fn count(&self, state: &str) -> usize {
        self.sources
            .iter()
            .filter(|facts| facts.state.name() == state)
            .count()
    }

    /// Blocks refused for `reason`.
    pub fn rejects_of(&self, reason: RejectReason) -> u64 {
        self.rejects.get(reason.name()).copied().unwrap_or(0)
    }

    /// Test helper: a note without a live registry (production goes through
    /// [`CaptureRegistry::set_note`]).
    #[cfg(test)]
    fn set_note_for_test(&mut self, note: &str) {
        self.note = Some(note.to_string());
    }

    /// Did the engine thread say why it started no capture?
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref().filter(|note| !note.is_empty())
    }

    /// The capture that best explains a failure (a failed spawn first, then an
    /// ended stream), for a finding's hint.
    pub fn first_failure(&self) -> Option<&CaptureFacts> {
        self.sources
            .iter()
            .find(|facts| matches!(facts.state, CaptureState::Failed { .. }))
            .or_else(|| {
                self.sources
                    .iter()
                    .find(|facts| matches!(facts.state, CaptureState::Ended { .. }))
            })
    }
}

/// The registry the capture children write into and the report reads.
///
/// Cheap to clone (`Arc`), never blocks the audio path for long (one `Mutex`
/// held for a few field writes per block, no I/O under it) and independent of
/// the `engine-pw` feature, so the report can hold an empty one when the build
/// has no real engine.
#[derive(Debug, Clone, Default)]
pub struct CaptureRegistry {
    inner: Arc<Mutex<CaptureInner>>,
}

#[derive(Debug, Default)]
struct CaptureInner {
    recorder: String,
    blocks: u64,
    rejected: u64,
    rejects: BTreeMap<String, u64>,
    sources: BTreeMap<String, CaptureEntry>,
    /// Why no capture runs (see `CaptureSummary::note`).
    note: Option<String>,
}

#[derive(Debug)]
struct CaptureEntry {
    facts: CaptureFacts,
    /// When the last block arrived (`CaptureFacts` keeps the age, not the
    /// instant, so the snapshot stays a plain value).
    last_block_at: Option<Instant>,
}

impl CaptureRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Which command the captures spawn (recorded once, for the report).
    pub fn set_recorder(&self, recorder: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.recorder = recorder.to_string();
        }
    }

    pub fn recorder(&self) -> String {
        self.inner
            .lock()
            .map(|inner| inner.recorder.clone())
            .unwrap_or_default()
    }

    /// A capture child was spawned and is waiting for its first block.
    pub fn started(
        &self,
        uid: &str,
        source: &str,
        target: &str,
        monitor: bool,
        pid: Option<u32>,
        command: &str,
    ) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.sources.insert(
            uid.to_string(),
            CaptureEntry {
                facts: CaptureFacts {
                    uid: uid.to_string(),
                    source: source.to_string(),
                    target: target.to_string(),
                    command: command.to_string(),
                    monitor,
                    pid,
                    state: CaptureState::Waiting,
                    blocks: 0,
                    rejected: 0,
                    last_rms: None,
                    last_peak: None,
                    since_last_block: None,
                    stderr_tail: Vec::new(),
                    format_guess: None,
                    detected_format: None,
                },
                last_block_at: None,
            },
        );
    }

    /// The child could not be spawned at all (missing `pw-cat`, permission…).
    ///
    /// Its entry stays in the registry: `warned` in the capture stops the retry
    /// loop, and this is the only place a user can see *why* the bar is dark.
    pub fn spawn_failed(
        &self,
        uid: &str,
        source: &str,
        target: &str,
        monitor: bool,
        command: &str,
        reason: &str,
    ) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.sources.insert(
            uid.to_string(),
            CaptureEntry {
                facts: CaptureFacts {
                    uid: uid.to_string(),
                    source: source.to_string(),
                    target: target.to_string(),
                    command: command.to_string(),
                    monitor,
                    pid: None,
                    state: CaptureState::Failed {
                        reason: reason.to_string(),
                    },
                    blocks: 0,
                    rejected: 0,
                    last_rms: None,
                    last_peak: None,
                    since_last_block: None,
                    stderr_tail: Vec::new(),
                    format_guess: None,
                    detected_format: None,
                },
                last_block_at: None,
            },
        );
    }

    /// One block was decoded **and drawn**.
    pub fn block(&self, uid: &str, sample: &MeterSample) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.blocks += 1;
        if let Some(entry) = inner.sources.get_mut(uid) {
            entry.facts.blocks += 1;
            entry.facts.state = CaptureState::Streaming;
            entry.facts.last_rms = Some(sample.rms);
            entry.facts.last_peak = Some(sample.peak);
            entry.last_block_at = Some(Instant::now());
        }
    }

    /// One block was refused (see [`RejectReason`]).
    ///
    /// `format_guess` is only computed once per capture (`crate::capture`), so a
    /// `None` must not erase the guess already recorded.
    pub fn reject(&self, uid: &str, reason: RejectReason, format_guess: Option<String>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.rejected += 1;
        *inner.rejects.entry(reason.name().to_string()).or_default() += 1;
        if let Some(entry) = inner.sources.get_mut(uid) {
            entry.facts.rejected += 1;
            if format_guess.is_some() {
                entry.facts.format_guess = format_guess;
            }
        }
    }

    /// The first block showed the recorder writes another sample format than
    /// `f32le`, and every block is being **converted** from it: the VU works.
    ///
    /// This is the opposite of [`Self::reject`]: nothing is dropped, so it is
    /// recorded *without* counting a rejection (the bar is fed).  `peak` is the
    /// converted peak, kept so a bug report shows the detection was sound.
    pub fn format_detected(&self, uid: &str, format: &str, peak: f32) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(entry) = inner.sources.get_mut(uid) {
            entry.facts.detected_format = Some(format.to_string());
            entry.facts.format_guess = Some(format!("{format} (peak {peak:.3})"));
        }
    }

    /// The detected format stopped explaining the stream (a format change
    /// mid-way): the reader will redetect on the next block.
    pub fn format_changed(&self, uid: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(entry) = inner.sources.get_mut(uid) {
            if let Some(detected) = entry.facts.detected_format.take() {
                entry.facts.format_guess = Some(format!("{detected} (changed mid-stream)"));
            }
        }
    }

    /// A line the child wrote on stderr (the last [`STDERR_TAIL`] are kept).
    pub fn push_stderr(&self, uid: &str, line: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(entry) = inner.sources.get_mut(uid) {
            entry.facts.stderr_tail.push(line.to_string());
            let excess = entry.facts.stderr_tail.len().saturating_sub(STDERR_TAIL);
            if excess > 0 {
                entry.facts.stderr_tail.drain(..excess);
            }
        }
    }

    /// The child's stdout ended.
    pub fn ended(&self, uid: &str, detail: impl Into<String>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(entry) = inner.sources.get_mut(uid) {
            if !matches!(entry.facts.state, CaptureState::Failed { .. }) {
                entry.facts.state = CaptureState::Ended {
                    detail: detail.into(),
                };
            }
        }
    }

    /// The child was reaped (`try_wait` on a snapshot): its exit code is more
    /// precise than “stdout closed”, so it replaces the ended detail.
    pub fn exited(&self, uid: &str, code: Option<i32>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(entry) = inner.sources.get_mut(uid) {
            if matches!(entry.facts.state, CaptureState::Failed { .. }) {
                return;
            }
            entry.facts.state = CaptureState::Ended {
                detail: match code {
                    Some(code) => format!("the process exited with code {code}"),
                    None => "the process was killed by a signal".to_string(),
                },
            };
        }
    }

    /// The capture was stopped on purpose (strip removed, source changed,
    /// engine restart): the entry goes away, so a stale status cannot describe
    /// a graph it no longer belongs to.
    pub fn stopped(&self, uid: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.sources.remove(uid);
        }
    }

    /// Record **why no capture runs** (the engine side of the chain): the report
    /// prints it instead of leaving “no capture is running” to the reader.
    pub fn set_note(&self, note: impl Into<String>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.note = Some(note.into());
        }
    }

    /// Drop the note (captures are running again).
    pub fn clear_note(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.note = None;
        }
    }

    /// Everything, as a plain value (the report never holds the mutex).
    pub fn summary(&self) -> CaptureSummary {
        let Ok(inner) = self.inner.lock() else {
            return CaptureSummary::default();
        };
        let now = Instant::now();
        let sources = inner
            .sources
            .values()
            .map(|entry| {
                let mut facts = entry.facts.clone();
                facts.since_last_block = entry
                    .last_block_at
                    .map(|at| now.saturating_duration_since(at));
                facts
            })
            .collect();
        CaptureSummary {
            recorder: inner.recorder.clone(),
            blocks: inner.blocks,
            rejected: inner.rejected,
            rejects: inner.rejects.clone(),
            sources,
            note: inner.note.clone(),
        }
    }
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
    /// `(uid, linear gain)` applied to a capture that is tapped **before** the
    /// strip's fader (a physical device's source/monitor), printed in the
    /// METERING section.
    ///
    /// It exists so “my VU ignores the fader” and “my VU is far too quiet” can
    /// both be diagnosed from a pasted report: a `×0.027` next to a strip at
    /// volume 0.3 is the post-fader correction working, and a missing gain on a
    /// strip that *should* have one names the strip the UI treated as a virtual
    /// bus.
    pub vu_gains: Vec<(String, f32)>,
    /// The capture children themselves: source, pid, state, blocks, rejects,
    /// last level and what `pw-cat` said on stderr (see [`CaptureSummary`]).
    pub captures: CaptureSummary,
    /// Does this build/engine actually instrument the captures?
    ///
    /// `false` (a mock bridge, `--no-default-features`, a report built without
    /// capture facts) means an *empty* [`Report::captures`] proves nothing: the
    /// absence of a capture cannot be reported as a failure there.  `true` means
    /// the engine thread publishes a registry, so “no capture running” while the
    /// engine names sources is a real error (`metering_not_started`).
    pub captures_instrumented: bool,
    /// Which recorder the captures spawn, resolved on this machine.
    pub recorder: RecorderProbe,
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
    /// `(uid, linear gain)` the UI multiplies a **pre-fader** capture by (see
    /// [`Report::vu_gains`]).  Only strips whose fader sits after the capture
    /// tap appear here.
    pub vu_gains: &'a [(String, f32)],
    /// The capture children's own status (`None` = no capture instrumentation:
    /// a mock build, or no real engine attached).
    pub captures: Option<&'a CaptureSummary>,
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
            vu_gains: &[],
            captures: None,
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
            vu_gains: inputs.vu_gains.to_vec(),
            captures: inputs.captures.cloned().unwrap_or_default(),
            captures_instrumented: inputs.captures.is_some(),
            recorder: recorder_probe(),
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

        // --- the capture chain itself -----------------------------------------
        //
        // The METERING section says what the bars draw; these rules say **why**
        // they draw nothing.  Without them a `pw-cat` that never started, one
        // that died at once, a stream read with the wrong sample format and a
        // genuinely silent bus produced the same report — the user's “METERING:
        // TOUS les niveaux à 0.000”, which named no cause at all.
        let captures = &self.captures;

        // **The engine names sources but this UI records none.**  That is the
        // other half of “all the bars read 0.000”: the capture never even
        // *started* (the engine thread could not ask, or the board had no slot),
        // and no `pw-cat` exists to describe.  Nothing else in the report can
        // show it — `meter_sources` is present in the status payload while the
        // CAPTURES section is empty.
        if self.captures_instrumented && engine.meter_sources > 0 && captures.is_empty() {
            findings.push(
                Finding::new(
                    "metering_not_started",
                    Severity::Error,
                    format!(
                        "the engine declares {} metering source(s) but this UI started no capture \
                         at all{}",
                        engine.meter_sources,
                        self.captures
                            .note()
                            .map(|note| format!(" — {note}"))
                            .unwrap_or_default()
                    ),
                )
                .with_hint(
                    "the capture lives in the engine thread: an engine that answers `status` but \
                     refuses `meter_sources` (or a disconnect) leaves every bar at 0.000 — check \
                     the Journal tab (a warning is logged on every re-sync), then restart the \
                     audio engine from the tray menu",
                ),
            );
        }

        if captures.never_streamed() {
            let failed = captures.count("failed");
            let ended = captures.count("ended");
            let waiting = captures.count("waiting");
            let total = captures.sources.len();
            let message = format!(
                "not one meter block was recorded for {total} source(s): every VU bar can only \
                 read 0.000 ({failed} capture(s) failed to start, {ended} ended, {waiting} running \
                 without data)"
            );
            // The hint names the *probable* cause, chosen from what the captures
            // themselves reported: this is the line the user acts on.
            let hint = if failed == total {
                format!(
                    "`{}` could not be started{}: install it (on Arch `pw-cat` ships with \
                     `pipewire-audio`) or point {RECORDER_ENV} at a working pw-cat",
                    self.recorder.command,
                    captures
                        .first_failure()
                        .and_then(|facts| facts.state.detail())
                        .map(|detail| format!(" ({detail})"))
                        .unwrap_or_default()
                )
            } else if ended == total {
                format!(
                    "every capture ended as soon as it started — {}: run `{} --record --target \
                     <node> -P '{{\"stream.capture.sink\":true}}' --format f32 -` by hand to see \
                     what it says",
                    captures
                        .first_failure()
                        .and_then(|facts| facts.state.detail())
                        .unwrap_or("no reason given"),
                    self.recorder.command
                )
            } else {
                "the captures are running but their stdout stays empty: paste the exact command \
                 from the CAPTURES section in a terminal (it is printed under each source), check \
                 that the target node exists (`pw-cli ls Node`) and that the recorder writes f32le \
                 — a WAV header or another sample format would be counted as “partial” blocks"
                    .to_string()
            };
            findings.push(
                Finding::new("metering_dead", Severity::Error, message).with_hint(hint),
            );
        }

        // A block whose f32 reading is impossible is a **format** mismatch, not
        // silence: name the format the bytes really look like.
        //
        // Note: a detected-and-converted stream does NOT land here (nothing is
        // dropped) — it shows as `detected <fmt> and converted` in the CAPTURES
        // section (`CaptureFacts::detected_format`).  This finding is now only
        // the *residual* case: bytes no known sample format explains.
        let implausible = captures.rejects_of(RejectReason::Implausible);
        if implausible > 0 {
            let guess = captures
                .sources
                .iter()
                .find_map(|facts| facts.format_guess.clone());
            let mut message = format!(
                "{implausible} metering block(s) were dropped as impossible (peak above +{:.0} \
                 dBFS): no known sample format explains what `{}` wrote to stdout",
                crate::meters::amplitude_to_db(crate::meters::IMPLAUSIBLE_PEAK),
                self.recorder.command
            );
            if let Some(guess) = guess {
                message.push_str(&format!(" — the bytes look like {guess}"));
            }
            findings.push(
                Finding::new("metering_implausible", Severity::Error, message).with_hint(
                    "a plausible sample format (s16le/s32le/f64le) is converted automatically, so \
                     this is bytes that are neither audio nor a format pw-cat writes — a container \
                     header (a WAV/.au header is counted as “partial” blocks, not here); run the \
                     recorder by hand and check what it really writes, or set the recorder to a \
                     wrapper that emits f32le",
                ),
            );
        }

        // Blocks arrive, but every bar reads zero: the chain works and the
        // *material* is silent (muted strips, zero faders, or a captured node
        // that carries no signal).  An info, because it is often intentional —
        // but it is the difference between “dead chain” and “nothing playing”.
        if captures.blocks > 0
            && !drawn.is_empty()
            && drawn
                .iter()
                .all(|(_, _, [left, right])| *left == 0.0 && *right == 0.0)
        {
            findings.push(
                Finding::new(
                    "metering_silent",
                    Severity::Info,
                    {
                        let muted = engine.strips.iter().filter(|strip| strip.mute).count();
                        let mut message = format!(
                            "the {} capture(s) do receive blocks ({} decoded) but every VU bar \
                             reads 0.000: the recorded buses carry silence right now",
                            captures.sources.len(),
                            captures.blocks
                        );
                        if muted > 0 {
                            message.push_str(&format!(
                                " ({} of {} strip(s) are MUTED — a muted strip's bar is dark by \
                                 design)",
                                muted,
                                engine.strips.len()
                            ));
                        }
                        message
                    },
                )
                .with_hint(
                    "strips muted or faders at zero, or the captured node carries no signal \
                     (an FX-chain output instead of the bus monitor) — play something with the \
                     strips unmuted before calling the metering broken",
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
                    "    - {:<24} bus={:<28} device={:<40} vol={:.3}{} fx={}",
                    strip.label,
                    strip.node_name.as_deref().unwrap_or("?"),
                    strip.device_name.as_deref().unwrap_or("-"),
                    strip.volume,
                    if strip.mute { " MUTED" } else { "" },
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
                // A pre-fader capture (a physical strip's device tap) is scaled
                // by the strip's fader before it is drawn: say so, otherwise a
                // bar that does not match the raw monitor looks like a bug.
                let gain = self
                    .vu_gains
                    .iter()
                    .find(|(gain_uid, _)| gain_uid == uid)
                    .map(|(_, gain)| *gain)
                    .unwrap_or(1.0);
                let fader = if (gain - 1.0).abs() > f32::EPSILON {
                    format!("  (post-fader x{gain:.3})")
                } else {
                    String::new()
                };
                lines.push(format!(
                    "    - {:<24} L={left:.3} R={right:.3}{source}{fader}",
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

        // CAPTURES — the VU chain's own health.  The METERING section above says
        // what is drawn; this one says whether the recorder runs at all, how many
        // blocks it delivered, and what it said on stderr.  A report without it
        // cannot explain a bar at 0.000 (nor a bar pinned at 1.000).
        lines.push("CAPTURES".to_string());
        lines.push(format!("  recorder        : {}", self.recorder.summary()));
        lines.push(format!("  recorder daemon : {}", self.recorder.endpoint()));
        if let Some(note) = self.captures.note() {
            lines.push(format!("  ! no capture    : {note}"));
        }
        if self.captures.is_empty() {
            lines.push(if self.captures_instrumented {
                "  (no capture is running - the engine thread has asked for no source)".to_string()
            } else {
                "  (this build/engine does not instrument the captures: no pid/state to show)"
                    .to_string()
            });
        } else {
            let rejects = if self.captures.rejects.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    self.captures
                        .rejects
                        .iter()
                        .map(|(reason, count)| format!("{count} {reason}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            lines.push(format!(
                "  blocks          : {} accepted, {} rejected{rejects}",
                self.captures.blocks, self.captures.rejected
            ));
            lines.push(format!("  sources ({}):", self.captures.sources.len()));
            for facts in &self.captures.sources {
                // The label comes from the engine's strip list (the uid is what
                // the capture is keyed by); a strip the engine no longer shows
                // keeps its uid, which is what a bug report needs anyway.
                let label = self
                    .engine
                    .strips
                    .iter()
                    .find(|strip| strip.uid == facts.uid)
                    .map(|strip| strip.label.as_str())
                    .unwrap_or(&facts.uid);
                lines.push(format!(
                    "    - {:<24} uid={} source={} -> target {} ({})",
                    label,
                    facts.uid,
                    facts.source,
                    facts.target,
                    if facts.monitor {
                        "sink monitor"
                    } else {
                        "capture node"
                    }
                ));
                lines.push(format!("        {}", facts.describe()));
                lines.push(format!("        $ {}", facts.command));
                for line in &facts.stderr_tail {
                    lines.push(format!("        stderr: {line}"));
                }
            }
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
                        mute: entry.get("mute").and_then(Value::as_bool).unwrap_or(false),
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

/// The recording client the captures spawn: `HOLAF_PW_CAT`, else `pw-cat`.
///
/// A single place, because three modules need the same answer: the capture
/// (`crate::capture`), the report and the tests.
pub fn default_recorder() -> String {
    std::env::var(RECORDER_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_RECORDER.to_string())
}

/// Is this path an executable file (a `which`-like check)?
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Resolve `command` the way `execvp` does: a path with a `/` is used as is,
/// a bare name is searched in `PATH`.
pub fn resolve_in_path(command: &str) -> Option<std::path::PathBuf> {
    if command.is_empty() {
        return None;
    }
    if command.contains('/') {
        let path = Path::new(command);
        return is_executable(path).then(|| path.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable(candidate))
}

/// The recorder, its resolved path and its version — see [`RecorderProbe`].
///
/// Spawns one short process (`<recorder> --version`) when the binary exists; a
/// failure is `None`, never an error (the version line is a convenience, the
/// missing path is the diagnosis).
pub fn recorder_probe() -> RecorderProbe {
    let env_override = std::env::var(RECORDER_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let command = env_override
        .clone()
        .unwrap_or_else(|| DEFAULT_RECORDER.to_string());
    let path = resolve_in_path(&command);
    let version = path.as_ref().and_then(|_| recorder_version(&command));
    // Only ask a binary that exists: probing a missing `pw-cat` would just print
    // the same ENOENT the spawn already reports.
    let raw = path
        .is_some()
        .then(|| crate::capture::recorder_supports_raw(&command));
    RecorderProbe {
        env_override,
        command,
        path: path.map(|path| path.display().to_string()),
        version,
        // The recorder is a **second** PipeWire client: it must reach the same
        // daemon as the engine, otherwise every `--target` resolves to nothing
        // and the meters stay dark on a perfectly healthy graph.
        remote: std::env::var("PIPEWIRE_REMOTE")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        runtime_dir: std::env::var("PIPEWIRE_RUNTIME_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("XDG_RUNTIME_DIR")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            }),
        raw,
    }
}

/// The version line of `<command> --version`, when the tool answers.
///
/// `pw-cat --version` prints its name first and the library it links second
/// (“pw-cat”, “Compiled with libpipewire 0.3.65”, “Linked with libpipewire
/// 0.3.65”), so the *last line carrying a number* is the informative one — the
/// first line alone would only repeat the command name.
///
/// The probe is **bounded and cached**: a recorder that is a shell wrapper (the
/// documented workaround for a `pw-cat` writing the wrong sample format) ignores
/// `--version` and starts recording, so an unbounded wait froze the report for
/// minutes (measured).  Once a command has answered, the answer is reused — the
/// report is rebuilt often, a child spawn per rebuild would not do.
fn recorder_version(command: &str) -> Option<String> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Option<String>>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(answer) = guard.get(command) {
            return answer.clone();
        }
    }
    let answer = probe_version(command);
    if let Ok(mut guard) = cache.lock() {
        guard.insert(command.to_string(), answer.clone());
    }
    answer
}

/// How long `<recorder> --version` is given to answer ([`recorder_version`]).
const VERSION_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

fn probe_version(command: &str) -> Option<String> {
    let output = crate::capture::recorder_probe_output(command, "--version", VERSION_PROBE_TIMEOUT)?;
    if !output.success {
        return None;
    }
    let stdout = output.stdout;
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let line = lines
        .iter()
        .rev()
        .find(|line| line.chars().any(|c| c.is_ascii_digit()))
        .or_else(|| lines.first())?;
    // “Linked with libpipewire 0.3.65” → `0.3.65`: the number is what a report
    // reader compares with their installed PipeWire.
    let token = line
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.'))
        .find(|word| {
            word.starts_with(|c: char| c.is_ascii_digit()) && word.contains('.')
        });
    Some(token.unwrap_or(line).to_string())
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
        // A **pre-fader** capture (a physical strip's device tap) is scaled by
        // the strip's fader before it is drawn; the report prints the factor so
        // “my VU ignores the fader” and “my VU is far too quiet” are
        // distinguishable — and a strip with no factor is visibly a virtual one.
        let report = Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            meter_levels: &levels,
            vu_gains: &[("in-mic".to_string(), 0.614_125_f32)],
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        });
        let text = report.text();
        assert!(
            text.contains("L=0.301 R=0.299") && text.contains("(post-fader x0.614)"),
            "{text}"
        );
        // …and the strip without a gain shows none.
        let casque_lines = text
            .lines()
            .filter(|line| line.contains("Casque"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!casque_lines.contains("post-fader"), "{casque_lines}");

        // A report with no strips drawing anything says so (never an empty
        // heading that reads like a truncated report).
        let empty = Report::collect(&Inputs::empty(&[]));
        assert!(
            empty.text().contains("(no strip is drawing a level)"),
            "{}",
            empty.text()
        );
    }

    // -----------------------------------------------------------------
    // CAPTURES: the VU chain's own health
    // -----------------------------------------------------------------

    /// One capture as the report sees it.
    fn capture_facts(uid: &str, source: &str, state: CaptureState, blocks: u64) -> CaptureFacts {
        CaptureFacts {
            uid: uid.to_string(),
            source: source.to_string(),
            target: source.trim_end_matches(".monitor").to_string(),
            command: format!(
                "pw-cat --record -P '{{\"stream.capture.sink\":true}}' --target {} --format f32 \
                 --rate 48000 --channels 2 -",
                source.trim_end_matches(".monitor")
            ),
            monitor: source.ends_with(".monitor"),
            pid: Some(4242),
            state,
            blocks,
            rejected: 0,
            last_rms: (blocks > 0).then_some([0.0031, 0.0030]),
            last_peak: (blocks > 0).then_some([0.0044, 0.0044]),
            since_last_block: (blocks > 0).then(|| std::time::Duration::from_millis(20)),
            stderr_tail: Vec::new(),
            format_guess: None,
            detected_format: None,
        }
    }

    fn summary_of(sources: Vec<CaptureFacts>) -> CaptureSummary {
        let blocks = sources.iter().map(|facts| facts.blocks).sum();
        let rejected = sources.iter().map(|facts| facts.rejected).sum();
        CaptureSummary {
            recorder: "pw-cat".to_string(),
            blocks,
            rejected,
            rejects: BTreeMap::new(),
            sources,
            note: None,
        }
    }

    fn report_with_captures(captures: &CaptureSummary) -> Report {
        let status = healthy_status();
        let strips = strips();
        let levels = vec![
            ("in-mic".to_string(), "Mic".to_string(), [0.0_f32, 0.0]),
            ("out-casque".to_string(), "Casque".to_string(), [0.0, 0.0]),
        ];
        Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            meter_levels: &levels,
            captures: Some(captures),
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        })
    }

    /// **The user's report**: every bar at 0.000, and no way to tell “no
    /// `pw-cat`” from “the stream died” or “nothing is playing”.  The capture
    /// facts turn that into one error finding that names the cause.
    #[test]
    fn a_capture_chain_that_never_delivered_a_block_is_an_error() {
        // (a) `pw-cat` is not installed: every spawn failed.
        let mut missing = capture_facts(
            "in-mic",
            "Holaf_Bus_Mic.monitor",
            CaptureState::Failed {
                reason: "pw-cat: No such file or directory (os error 2)".to_string(),
            },
            0,
        );
        missing.pid = None;
        let mut other = missing.clone();
        other.uid = "out-casque".to_string();
        other.source = "alsa_output.usb.analog-stereo.monitor".to_string();
        let captures = summary_of(vec![missing.clone(), other]);
        assert!(captures.never_streamed() && captures.all_dead());

        let report = report_with_captures(&captures);
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "metering_dead")
            .unwrap_or_else(|| panic!("no metering_dead in {:?}", report.findings));
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.message.contains("0.000"), "{}", finding.message);
        assert!(finding.message.contains("2 capture(s) failed"), "{}", finding.message);
        let hint = finding.hint.as_deref().unwrap_or_default();
        assert!(hint.contains("`pw-cat` could not be started"), "{hint}");
        assert!(hint.contains("No such file"), "{hint}");
        assert!(hint.contains(RECORDER_ENV) && hint.contains("pipewire-audio"), "{hint}");

        // (b) every capture ended at once (a refused `--target`).
        let mut ended = capture_facts(
            "in-mic",
            "Holaf_Bus_Mic.monitor",
            CaptureState::Ended {
                detail: "the process exited with code 1".to_string(),
            },
            0,
        );
        ended.stderr_tail = vec!["stream refused by the session manager".to_string()];
        let report = report_with_captures(&summary_of(vec![ended]));
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "metering_dead")
            .expect("metering_dead");
        let hint = finding.hint.as_deref().unwrap_or_default();
        assert!(hint.contains("ended as soon as it started"), "{hint}");
        assert!(hint.contains("code 1"), "{hint}");

        // (c) the captures run but nothing arrives on stdout.
        let running = capture_facts("in-mic", "Holaf_Bus_Mic.monitor", CaptureState::Waiting, 0);
        let report = report_with_captures(&summary_of(vec![running]));
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "metering_dead")
            .expect("metering_dead");
        assert!(
            finding
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("stdout stays empty"),
            "{:?}",
            finding.hint
        );

        // A stream that delivers blocks is NOT this finding — even silent ones
        // (otherwise “the strips are muted” would read as a broken meter).
        let streaming = capture_facts("in-mic", "Holaf_Bus_Mic.monitor", CaptureState::Streaming, 42);
        let report = report_with_captures(&summary_of(vec![streaming]));
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.id == "metering_dead"),
            "{:?}",
            report.findings
        );
    }

    /// A refused block (peak over +40 dBFS) is a **format** mismatch, and the
    /// report names the format the bytes really look like instead of leaving a
    /// dark bar with no explanation.
    #[test]
    fn refused_blocks_name_the_format_the_recorder_really_writes() {
        let mut facts = capture_facts("in-mic", "Holaf_Bus_Mic.monitor", CaptureState::Waiting, 0);
        facts.rejected = 210;
        facts.format_guess = Some("s16le (peak 0.750)".to_string());
        let mut captures = summary_of(vec![facts]);
        captures.rejected = 210;
        captures.rejects.insert("implausible".to_string(), 210);
        assert_eq!(captures.rejects_of(RejectReason::Implausible), 210);

        let report = report_with_captures(&captures);
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "metering_implausible")
            .unwrap_or_else(|| panic!("no metering_implausible in {:?}", report.findings));
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.message.contains("210 metering block(s)"), "{}", finding.message);
        assert!(finding.message.contains("+40 dBFS"), "{}", finding.message);
        assert!(finding.message.contains("s16le"), "{}", finding.message);
        // The message says the bytes are *unexplainable*, not “not f32le”: a
        // plausible format (s16le/s32le/f64le) is now detected and converted, so
        // this finding is the residual case (a container header, no format at
        // all) — and the hint explains that a real sample format would have been
        // handled automatically.
        assert!(
            finding.message.contains("no known sample format explains"),
            "{}",
            finding.message
        );
        assert!(
            finding
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("converted automatically")),
            "{:?}",
            finding.hint
        );
        // It does not claim the chain is dead: blocks *are* arriving.
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.id == "metering_dead"),
            "{:?}",
            report.findings
        );
    }

    /// Silent-but-alive is not a failure: blocks arrive and every bar reads
    /// zero because nothing is playing (or the strips are muted).  It is an
    /// **info**, the one that separates “dead chain” from “quiet material”.
    #[test]
    fn a_live_but_silent_capture_is_only_an_info() {
        let streaming = capture_facts("in-mic", "Holaf_Bus_Mic.monitor", CaptureState::Streaming, 300);
        let report = report_with_captures(&summary_of(vec![streaming]));
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "metering_silent")
            .unwrap_or_else(|| panic!("no metering_silent in {:?}", report.findings));
        assert_eq!(finding.severity, Severity::Info);
        assert!(finding.message.contains("300 decoded"), "{}", finding.message);
        assert_eq!(report.errors(), 0, "{:?}", report.findings);

        // Blocks + a moving bar: nothing to say.
        let status = healthy_status();
        let strips = strips();
        let levels = vec![
            ("in-mic".to_string(), "Mic".to_string(), [0.73_f32, 0.70]),
            ("out-casque".to_string(), "Casque".to_string(), [0.66, 0.65]),
        ];
        let streaming = capture_facts("in-mic", "Holaf_Bus_Mic.monitor", CaptureState::Streaming, 300);
        let captures = summary_of(vec![streaming]);
        let report = Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            meter_levels: &levels,
            captures: Some(&captures),
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        });
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.id == "metering_silent"),
            "{:?}",
            report.findings
        );
    }

    /// A muted strip's bar is dark **by design** (`app::feed_from_board` forces
    /// zero), so the “nothing moves” info has to say it: seven dark bars on a
    /// muted mixer is not a broken metering chain.
    #[test]
    fn a_muted_mixer_is_named_in_the_silence_info() {
        let mut status = healthy_status();
        status["strips"][0]["mute"] = json!(true);
        status["strips"][1]["mute"] = json!(true);
        let strips = strips();
        let streaming = capture_facts("in-mic", "Holaf_Bus_Mic.monitor", CaptureState::Streaming, 42);
        let captures = summary_of(vec![streaming]);
        let levels = vec![
            ("in-mic".to_string(), "Mic".to_string(), [0.0_f32, 0.0]),
            ("out-casque".to_string(), "Casque".to_string(), [0.0, 0.0]),
        ];
        let report = Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            meter_levels: &levels,
            captures: Some(&captures),
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        });
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "metering_silent")
            .expect("metering_silent");
        assert!(
            finding.message.contains("2 of 2 strip(s) are MUTED"),
            "{}",
            finding.message
        );
        // The ENGINE table carries the flag too, so a pasted report is readable
        // without cross-checking the window.
        assert!(report.text().contains("MUTED"), "{}", report.text());
        // A mute is never an *error*: the chain is healthy.
        assert_eq!(report.errors(), 0, "{:?}", report.findings);
    }

    /// **The engine names sources, the UI records none.**  The capture never
    /// even started (the engine thread could not ask, the board had no slot, the
    /// bridge is not connected), so there is no `pw-cat` to describe — the old
    /// report showed seven bars at 0.000 and nothing else.
    #[test]
    fn no_capture_started_at_all_is_an_error_naming_the_reason() {
        let status = healthy_status();
        let strips = strips();
        let mut captures = CaptureSummary::default();
        captures.set_note_for_test(
            "the engine refused `meter_sources`: the engine is not connected",
        );
        let levels = vec![
            ("in-mic".to_string(), "Mic".to_string(), [0.0_f32, 0.0]),
            ("out-casque".to_string(), "Casque".to_string(), [0.0, 0.0]),
        ];
        let report = Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            meter_levels: &levels,
            captures: Some(&captures),
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        });
        assert!(report.captures_instrumented);
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == "metering_not_started")
            .unwrap_or_else(|| panic!("no metering_not_started in {:?}", report.findings));
        assert_eq!(finding.severity, Severity::Error);
        assert!(
            finding.message.contains("declares 1 metering source(s)"),
            "{}",
            finding.message
        );
        assert!(
            finding.message.contains("refused `meter_sources`"),
            "{}",
            finding.message
        );
        let text = report.text();
        assert!(text.contains("! no capture    :"), "{text}");

        // A report whose capture chain is not instrumented says nothing (an
        // empty section there proves nothing).
        let plain = Report::collect(&Inputs {
            status: Some(&status),
            strips: &strips,
            now_unix: 1_700_000_000,
            ..Inputs::empty(&strips)
        });
        assert!(!plain.captures_instrumented);
        assert!(
            !plain
                .findings
                .iter()
                .any(|finding| finding.id == "metering_not_started"),
            "{:?}",
            plain.findings
        );
    }

    /// The CAPTURES section must be **self-explanatory**: which recorder, whether
    /// it exists, what each capture targets, its pid/state/blocks and what
    /// `pw-cat` said.  It is the section the user will paste.
    #[test]
    fn the_captures_section_names_the_recorder_and_every_source() {
        let mut facts = capture_facts("in-mic", "Holaf_Bus_Mic.monitor", CaptureState::Streaming, 210);
        facts.stderr_tail = vec!["cannot open the target Holaf_Bus_Mic".to_string()];
        let mut ended = capture_facts(
            "out-casque",
            "alsa_output.usb-PreSonus.analog.monitor",
            CaptureState::Ended {
                detail: "the process exited with code 2".to_string(),
            },
            0,
        );
        ended.pid = None;
        let mut captures = summary_of(vec![facts, ended]);
        captures.rejects.insert("partial".to_string(), 3);
        captures.rejected = 3;
        let report = report_with_captures(&captures);
        let text = report.text();

        let captures_at = text.find("\nCAPTURES").unwrap_or_else(|| panic!("{text}"));
        let devices_at = text.find("\nDEVICES").expect("a DEVICES section");
        assert!(captures_at < devices_at, "CAPTURES sits before DEVICES");

        // The recorder line: the command, its resolved path and the env var.
        assert!(text.contains("recorder        : pw-cat ->"), "{text}");
        assert!(text.contains(RECORDER_ENV), "{text}");
        // Global counters, with the reject reason spelled out.
        assert!(text.contains("210 accepted, 3 rejected (3 partial)"), "{text}");
        // Per capture: the source, the node given to `--target`, the pid and the
        // state, and the stderr tail.
        assert!(
            text.contains("source=Holaf_Bus_Mic.monitor -> target Holaf_Bus_Mic (sink monitor)"),
            "{text}"
        );
        assert!(text.contains("uid=in-mic"), "{text}");
        assert!(text.contains("pid 4242 · streaming · 210 block(s), 0 rejected"), "{text}");
        assert!(text.contains("last rms 0.0031/0.0030"), "{text}");
        assert!(text.contains("stderr: cannot open the target Holaf_Bus_Mic"), "{text}");
        assert!(text.contains("pid none · ended · 0 block(s), 0 rejected"), "{text}");
        assert!(text.contains("the process exited with code 2"), "{text}");
        // The label of the strip comes from the engine list, not from the uid.
        assert!(text.contains("Mic ") && text.contains("Casque"), "{text}");

        // No capture at all: the section says so instead of reading like a
        // truncated report — and it distinguishes “nothing runs” (instrumented)
        // from “this build shows no capture state” (no registry).
        let empty = Report::collect(&Inputs::empty(&[]));
        assert!(!empty.captures_instrumented);
        assert!(
            empty
                .text()
                .contains("(this build/engine does not instrument the captures"),
            "{}",
            empty.text()
        );
        let not_running = Report::collect(&Inputs {
            captures: Some(&CaptureSummary::default()),
            ..Inputs::empty(&[])
        });
        assert!(not_running.captures_instrumented);
        assert!(
            not_running
                .text()
                .contains("(no capture is running - the engine thread has asked for no source)"),
            "{}",
            not_running.text()
        );
    }

    /// The registry is what makes the engine thread's captures visible to the UI
    /// thread: the same facts, in the order the strips were assigned.
    #[test]
    fn the_capture_registry_snapshots_what_the_children_report() {
        let registry = CaptureRegistry::new();
        registry.set_recorder("pw-cat");
        assert_eq!(registry.recorder(), "pw-cat");
        registry.started(
            "in-mic",
            "Holaf_Bus_Mic.monitor",
            "Holaf_Bus_Mic",
            true,
            Some(77),
            "pw-cat --record --target Holaf_Bus_Mic -",
        );
        registry.started(
            "out-casque",
            "alsa_input.x",
            "alsa_input.x",
            false,
            Some(78),
            "pw-cat --record --target alsa_input.x -",
        );
        // A block on one of them, a refused block on the other.
        registry.block(
            "in-mic",
            &crate::meters::analyse_block(&[0.5_f32, -0.5, 0.5, -0.5], 2),
        );
        registry.reject("out-casque", RejectReason::Implausible, Some("s16le".to_string()));
        registry.reject("out-casque", RejectReason::Implausible, None);
        registry.push_stderr("out-casque", "boom");

        let summary = registry.summary();
        assert_eq!(summary.recorder, "pw-cat");
        assert_eq!(summary.blocks, 1);
        assert_eq!(summary.rejected, 2);
        assert_eq!(summary.rejects_of(RejectReason::Implausible), 2);
        assert_eq!(summary.sources.len(), 2);
        // Sorted by uid, so the report is diffable between two runs.
        assert!(summary.sources.windows(2).all(|pair| pair[0].uid < pair[1].uid));
        let mic = summary
            .sources
            .iter()
            .find(|facts| facts.uid == "in-mic")
            .expect("in-mic");
        assert_eq!(mic.state, CaptureState::Streaming);
        assert_eq!(mic.pid, Some(77));
        assert_eq!(mic.blocks, 1);
        assert!(mic.since_last_block.is_some());
        let casque = summary
            .sources
            .iter()
            .find(|facts| facts.uid == "out-casque")
            .expect("out-casque");
        // A refused block does not make it "streaming" (nothing was drawn) — but
        // it does prove the recorder wrote bytes, which is why a format
        // mismatch is `metering_implausible` and not `metering_dead`.
        assert_eq!(casque.state, CaptureState::Waiting);
        assert_eq!(casque.rejected, 2);
        assert_eq!(casque.format_guess.as_deref(), Some("s16le"), "the first guess wins");
        assert_eq!(casque.stderr_tail, vec!["boom".to_string()]);

        // Ended / exited / stopped are reflected, and a stopped capture leaves
        // no stale entry behind.
        registry.ended("in-mic", "the recorder closed its stdout");
        let state = |uid: &str| {
            registry
                .summary()
                .sources
                .into_iter()
                .find(|facts| facts.uid == uid)
                .map(|facts| facts.state)
        };
        assert!(matches!(state("in-mic"), Some(CaptureState::Ended { .. })));
        registry.exited("in-mic", Some(3));
        assert_eq!(
            state("in-mic"),
            Some(CaptureState::Ended {
                detail: "the process exited with code 3".to_string()
            })
        );
        registry.spawn_failed(
            "new",
            "source.x",
            "source.x",
            false,
            "pw-cat --record --target source.x -",
            "ENOENT",
        );
        let summary = registry.summary();
        assert_eq!(summary.count("failed"), 1);
        assert!(summary.first_failure().is_some());
        registry.stopped("in-mic");
        assert_eq!(registry.summary().sources.len(), 2);
        assert!(!registry.summary().sources.iter().any(|f| f.uid == "in-mic"));

        // The engine-side note explains an empty registry (the other way every
        // bar reads 0.000: no capture was even started).
        registry.set_note("the engine is not connected - no capture can run");
        assert_eq!(
            registry.summary().note(),
            Some("the engine is not connected - no capture can run")
        );
        registry.clear_note();
        assert_eq!(registry.summary().note(), None);

        // The stderr tail is bounded (a chatty pw-cat must not grow the report).
        for index in 0..(STDERR_TAIL + 4) {
            registry.push_stderr("new", &format!("line {index}"));
        }
        let tail = registry
            .summary()
            .sources
            .into_iter()
            .find(|facts| facts.uid == "new")
            .expect("new")
            .stderr_tail;
        assert_eq!(tail.len(), STDERR_TAIL);
        assert_eq!(tail.last().map(String::as_str), Some("line 8"));
    }

    /// **The report must never wait on the recorder.**  A recorder that is a
    /// shell wrapper (the documented workaround for a `pw-cat` writing the wrong
    /// sample format — and every test stand-in) ignores the options it does not
    /// know and starts recording: `Command::output()` then never returns, and
    /// the diagnostic report froze for minutes (measured while capturing with
    /// such a wrapper).  Both probes are bounded.
    #[test]
    fn a_recorder_that_never_answers_a_probe_cannot_hang_the_report() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "holaf-recorder-hang-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("recorder.sh");
        // A wrapper that only knows `--help` and records for ever otherwise —
        // exactly what a `pw-cat` stand-in looks like.
        std::fs::write(
            &script,
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = --help ] && { echo '  -a, --raw   RAW mode'; exit 0; }; done\nsleep 60\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        let command = script.to_string_lossy().into_owned();

        let started = std::time::Instant::now();
        // `--version` is not answered: the probe gives up (None) instead of
        // waiting for the `sleep 60`.
        assert_eq!(recorder_version(&command), None);
        let elapsed = started.elapsed();
        assert!(
            elapsed < VERSION_PROBE_TIMEOUT * 3,
            "the version probe must be bounded, took {elapsed:?}"
        );
        // The bounded helper also reports a *successful* answer, so a healthy
        // recorder keeps naming its version.
        let answered = crate::capture::recorder_probe_output(
            &command,
            "--help",
            VERSION_PROBE_TIMEOUT,
        )
        .expect("an answer within the bound");
        assert!(answered.success, "{answered:?}");
        assert!(answered.text().contains("--raw"), "{answered:?}");
    }

    /// The recorder probe must never panic and must answer the two questions a
    /// dark meter raises: *which* binary, and does it exist?
    #[test]
    fn the_recorder_probe_resolves_the_command_and_its_path() {
        // A command that certainly exists (the test binary's own executable).
        let me = std::env::current_exe().expect("the test executable");
        assert_eq!(resolve_in_path(&me.display().to_string()), Some(me.clone()));
        // A bare name is looked up in PATH; `sh` is on every Unix.
        let sh = resolve_in_path("sh").expect("sh in PATH");
        assert!(sh.is_absolute());
        // A name nobody has, and the empty string.
        assert_eq!(resolve_in_path("holaf-no-such-recorder"), None);
        assert_eq!(resolve_in_path(""), None);

        let probe = recorder_probe();
        assert!(!probe.command.is_empty());
        assert_eq!(probe.command, default_recorder());
        let line = probe.summary();
        assert!(line.contains(&probe.command), "{line}");
        assert!(line.contains(RECORDER_ENV), "{line}");
        if probe.found() {
            assert!(line.contains("->"), "{line}");
        } else {
            assert!(line.contains("NOT FOUND in PATH"), "{line}");
        }
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
