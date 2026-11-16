//! Clock settings and processing statistics: the numbers the diagnostics
//! window needs to explain *crackling* (`small_quantum`, `pwt_errors`).
//!
//! # What is really available (measured on PipeWire 0.3.65)
//!
//! | number | source | available here |
//! |---|---|---|
//! | `clock.rate`, `clock.quantum`, `clock.force-quantum`, min/max | `pw-metadata -n settings` | **yes** (one line per key) |
//! | per-node live quantum / rate / WAIT / BUSY / **ERR** | `pw-top -b -n 2` (batch) | **no** — `-b` only exists from PipeWire 0.3.66 |
//!
//! So the quantum is published for real, and the ERR counters are reported as
//! *unavailable with a reason* instead of being invented: `pw-top` in 0.3.65 is
//! an ncurses TUI, and scraping a terminal for numbers would be a guess.  The
//! parser below is still written (and tested against a recorded `-b` sample) so
//! a machine with a newer PipeWire gets the real counters for free.
//!
//! `pw-metadata` costs ~10 ms (measured), so [`collect`] is called from
//! `status()` — a poll every second or so, on the daemon side.
//!
//! # Effective quantum
//!
//! `clock.force-quantum = 0` means "not forced": the effective value is
//! `clock.quantum`.  A user who forces 32/64 samples is exactly the case the
//! `small_quantum` finding exists for, so [`Settings::effective_quantum`]
//! reports the forced value when there is one.

use serde_json::{json, Value};

/// Below this, a graph overruns easily on a busy CPU (Python
/// `MIN_HEALTHY_QUANTUM`) — the threshold the diagnostics use.
pub const MIN_HEALTHY_QUANTUM: u32 = 256;

/// `pw-top` error count from which an xrun burst is an ERROR, not a warning
/// (Python `PWT_ERROR_THRESHOLD`).
pub const PWT_ERROR_THRESHOLD: u64 = 100;

/// The `settings` metadata of the daemon (`pw-metadata -n settings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub rate: u32,
    pub quantum: u32,
    pub force_quantum: u32,
    pub force_rate: u32,
    pub min_quantum: u32,
    pub max_quantum: u32,
}

impl Default for Settings {
    fn default() -> Self {
        // PipeWire's own defaults, used when the metadata cannot be read.
        Self {
            rate: 48_000,
            quantum: 1024,
            force_quantum: 0,
            force_rate: 0,
            min_quantum: 16,
            max_quantum: 2048,
        }
    }
}

impl Settings {
    /// The quantum the graph actually runs at: the forced one when it is set,
    /// the configured one otherwise.
    pub fn effective_quantum(&self) -> u32 {
        if self.force_quantum > 0 {
            self.force_quantum
        } else {
            self.quantum
        }
    }

    /// The rate the graph runs at (forced value first).
    pub fn effective_rate(&self) -> u32 {
        if self.force_rate > 0 {
            self.force_rate
        } else {
            self.rate
        }
    }

    /// Latency of one quantum, in milliseconds (`quantum / rate * 1000`).
    pub fn quantum_ms(&self) -> f64 {
        let rate = self.effective_rate().max(1) as f64;
        self.effective_quantum() as f64 * 1000.0 / rate
    }
}

/// One row of `pw-top -b` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopRow {
    pub id: u32,
    /// `S` (suspended), `R` (running), `I` (idle), `E` (error)…
    pub state: String,
    pub quantum: u32,
    pub rate: u32,
    pub wait: String,
    pub busy: String,
    pub err: u64,
    /// `<FORMAT> <channels> <rate> <node name…>` — the first token is the sample
    /// format, the rest of the tail is the name.
    pub format: Option<String>,
    pub name: Option<String>,
}

/// The `pw-top` block: rows (last sample wins) plus the summed error count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TopStats {
    pub rows: Vec<TopRow>,
    pub errors: u64,
}

impl TopStats {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Rows with at least one error, worst first, capped at `limit`.
    pub fn worst(&self, limit: usize) -> Vec<&TopRow> {
        let mut rows: Vec<&TopRow> = self.rows.iter().filter(|row| row.err > 0).collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.err));
        rows.truncate(limit);
        rows
    }

    /// Live quantum of a node, by `node.name` (what the user sees in `pw-dump`).
    pub fn quantum_of(&self, name: &str) -> Option<u32> {
        self.rows
            .iter()
            .find(|row| row.name.as_deref() == Some(name))
            .map(|row| row.quantum)
            .filter(|quantum| *quantum > 0)
    }
}

/// Parse `pw-metadata -n settings`.
///
/// One line per key, e.g. `update: id:0 key:'clock.quantum' value:'1024'
/// type:''`.  Unknown keys are ignored and a missing key keeps its default, so
/// a daemon that publishes only some of them (or none) is not an error.
pub fn parse_settings(text: &str) -> Settings {
    let mut settings = Settings::default();
    for line in text.lines() {
        let Some((key, value)) = key_value(line) else {
            continue;
        };
        let number = value.parse::<u32>().ok();
        match key {
            "clock.rate" => settings.rate = number.unwrap_or(settings.rate),
            "clock.quantum" => settings.quantum = number.unwrap_or(settings.quantum),
            "clock.force-quantum" => settings.force_quantum = number.unwrap_or(0),
            "clock.force-rate" => settings.force_rate = number.unwrap_or(0),
            "clock.min-quantum" => settings.min_quantum = number.unwrap_or(settings.min_quantum),
            "clock.max-quantum" => settings.max_quantum = number.unwrap_or(settings.max_quantum),
            _ => {}
        }
    }
    settings
}

/// `key:'…' value:'…'` of one `pw-metadata` update line.
fn key_value(line: &str) -> Option<(&str, &str)> {
    let key_start = line.find("key:'")? + "key:'".len();
    let key_end = key_start + line[key_start..].find('\'')?;
    let value_start = line[key_start..].find("value:'")? + key_start + "value:'".len();
    let value_end = value_start + line[value_start..].find('\'')?;
    Some((&line[key_start..key_end], &line[value_start..value_end]))
}

/// Parse `pw-top -b -n N` output.
///
/// Batch mode repeats the header for every iteration and only the **later**
/// blocks carry real measurements (the first one is a warm-up printing `---`),
/// so rows are keyed by node id and the last occurrence wins.  Text that is not
/// a `pw-top` table (an "invalid option" error, for instance) yields an empty
/// result rather than an error.
pub fn parse_pw_top(text: &str) -> TopStats {
    let mut rows: std::collections::BTreeMap<u32, TopRow> = Default::default();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("error") || lower.starts_with("can't") || lower.starts_with("failed") {
            continue;
        }
        let Some(row) = parse_top_row(line) else {
            continue;
        };
        rows.insert(row.id, row);
    }
    let errors = rows.values().map(|row| row.err).sum();
    TopStats {
        rows: rows.into_values().collect(),
        errors,
    }
}

/// `S   ID  QUANT   RATE    WAIT    BUSY   W/Q   B/Q  ERR FORMAT …` row.
fn parse_top_row(line: &str) -> Option<TopRow> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    // state id quant rate wait busy w/q b/q err format channels rate name…
    if fields.len() < 10 {
        return None;
    }
    let state = fields[0];
    if state.len() != 1 || !state.chars().all(char::is_alphabetic) {
        return None;
    }
    let id = fields[1].parse().ok()?;
    let quantum = fields[2].parse().ok()?;
    let rate = fields[3].parse().ok()?;
    let err: u64 = fields[8].parse().ok()?;
    let format = fields.get(9).map(|value| (*value).to_string());
    // `FORMAT channels rate <name…>`: the name may contain spaces.
    let name = if fields.len() > 12 {
        Some(fields[12..].join(" "))
    } else {
        None
    };
    Some(TopRow {
        id,
        state: state.to_uppercase(),
        quantum,
        rate,
        wait: fields[4].to_string(),
        busy: fields[5].to_string(),
        err,
        format,
        name,
    })
}

/// Run one external tool, returning stdout on success and the *reason* on
/// failure (never an error: the diagnostics must degrade, not die).
fn run(bin: &str, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new(bin)
        .args(args)
        .output()
        .map_err(|err| format!("cannot run {bin}: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{bin} {} failed: {}",
            args.join(" "),
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Collect the clock settings and (when the tool supports it) the processing
/// statistics, as the JSON `status()` publishes under `timing`.
///
/// Shape:
///
/// ```json
/// {"source": "pw-metadata settings",
///  "rate": 48000, "quantum": 1024, "force_quantum": 0,
///  "effective_quantum": 1024, "effective_rate": 48000, "quantum_ms": 21.33,
///  "min_quantum": 16, "max_quantum": 2048,
///  "settings_error": null,
///  "pw_top": {"available": true, "reason": null, "errors": 3,
///             "rows": [{"id": 52, "name": "Holaf_Bus_Mix_Out", "quantum": 128,
///                       "rate": 48000, "err": 3, "state": "R"}]}}
/// ```
///
/// `pw_top.available = false` always carries a `reason` — that is what the
/// `pw_top_unavailable` finding prints.
pub fn collect() -> Value {
    collect_with(run)
}

/// [`collect`] with the process runner injected, so the whole payload is
/// testable without PipeWire installed.
pub fn collect_with(runner: impl Fn(&str, &[&str]) -> Result<String, String>) -> Value {
    let (settings, settings_error) = match runner("pw-metadata", &["-n", "settings"]) {
        Ok(text) => (parse_settings(&text), None),
        Err(reason) => (Settings::default(), Some(reason)),
    };

    // `-n 2`: two iterations, the second one carries real values (the first
    // prints `---` for WAIT/BUSY).
    let pw_top = match runner("pw-top", &["-b", "-n", "2"]) {
        Ok(text) => {
            let stats = parse_pw_top(&text);
            if stats.is_empty() {
                json!({
                    "available": false,
                    "reason": "pw-top printed no node row (no stream running?)",
                    "errors": 0,
                    "rows": [],
                })
            } else {
                json!({
                    "available": true,
                    "reason": Value::Null,
                    "errors": stats.errors,
                    "rows": stats
                        .rows
                        .iter()
                        .map(|row| json!({
                            "id": row.id,
                            "name": row.name,
                            "state": row.state,
                            "quantum": row.quantum,
                            "rate": row.rate,
                            "wait": row.wait,
                            "busy": row.busy,
                            "err": row.err,
                            "format": row.format,
                        }))
                        .collect::<Vec<Value>>(),
                })
            }
        }
        Err(reason) => {
            // PipeWire < 0.3.66 has no batch mode at all: say so precisely,
            // instead of "pw-top failed".
            let reason = if reason.contains("invalid option") {
                "pw-top has no batch mode (needs PipeWire >= 0.3.66)".to_string()
            } else {
                reason
            };
            json!({
                "available": false,
                "reason": reason,
                "errors": 0,
                "rows": [],
            })
        }
    };

    json!({
        "source": "pw-metadata settings",
        "rate": settings.rate,
        "quantum": settings.quantum,
        "force_quantum": settings.force_quantum,
        "force_rate": settings.force_rate,
        "effective_quantum": settings.effective_quantum(),
        "effective_rate": settings.effective_rate(),
        "quantum_ms": settings.quantum_ms(),
        "min_quantum": settings.min_quantum,
        "max_quantum": settings.max_quantum,
        "settings_error": settings_error,
        "pw_top": pw_top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded on the sandbox daemon (PipeWire 0.3.65, headless).
    const SETTINGS: &str = "\
Found \"settings\" metadata 29
update: id:0 key:'log.level' value:'2' type:''
update: id:0 key:'clock.rate' value:'48000' type:''
update: id:0 key:'clock.allowed-rates' value:'[ 48000 ]' type:''
update: id:0 key:'clock.quantum' value:'1024' type:''
update: id:0 key:'clock.min-quantum' value:'16' type:''
update: id:0 key:'clock.max-quantum' value:'2048' type:''
update: id:0 key:'clock.force-quantum' value:'0' type:''
update: id:0 key:'clock.force-rate' value:'0' type:''
";

    /// The sample the Python collector is tested with (`tests/test_diagnostics.py`):
    /// two batch iterations, the second one carries the measurements.
    const PW_TOP: &str = "\
S   ID  QUANT   RATE    WAIT    BUSY   W/Q   B/Q  ERR FORMAT           NAME
S  41   1024  48000     ---     ---   ---   ---    0    S32LE 2 48000  alsa_output.usb-PreSonus.analog-stereo
R  52    128  48000   1.2us   0.8us  0.00  0.00    3    F32LE 2 48000  Holaf_Bus_Mix_Out

S   ID  QUANT   RATE    WAIT    BUSY   W/Q   B/Q  ERR FORMAT           NAME
R  41   1024  48000   0.9us   0.5us  0.00  0.00    0    S32LE 2 48000  alsa_output.usb-PreSonus.analog-stereo
R  52    128  48000   1.1us   0.7us  0.00  0.00    3    F32LE 2 48000  Holaf_Bus_Mix_Out
";

    #[test]
    fn the_clock_settings_are_read_from_the_metadata_lines() {
        let settings = parse_settings(SETTINGS);
        assert_eq!(settings.rate, 48_000);
        assert_eq!(settings.quantum, 1024);
        assert_eq!(settings.min_quantum, 16);
        assert_eq!(settings.max_quantum, 2048);
        assert_eq!(settings.force_quantum, 0);
        // Nothing forced → the configured quantum is the effective one.
        assert_eq!(settings.effective_quantum(), 1024);
        assert!((settings.quantum_ms() - 21.333).abs() < 0.01);
    }

    #[test]
    fn a_forced_quantum_wins() {
        let settings = parse_settings(
            "update: id:0 key:'clock.quantum' value:'1024' type:''\n\
             update: id:0 key:'clock.force-quantum' value:'64' type:''\n\
             update: id:0 key:'clock.force-rate' value:'44100' type:''\n",
        );
        assert_eq!(settings.effective_quantum(), 64);
        assert_eq!(settings.effective_rate(), 44_100);
        assert!(settings.quantum_ms() < 2.0, "{}", settings.quantum_ms());
    }

    #[test]
    fn missing_or_broken_lines_keep_the_pipewire_defaults() {
        let settings = parse_settings("");
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.effective_quantum(), 1024);
        // A key with a non-numeric value is ignored, not fatal.
        let settings = parse_settings("update: id:0 key:'clock.quantum' value:'nope' type:''");
        assert_eq!(settings.quantum, 1024);
        // Garbage that is not a metadata line at all.
        assert_eq!(parse_settings("hello world"), Settings::default());
    }

    #[test]
    fn the_last_pw_top_sample_wins_and_errors_are_summed() {
        let stats = parse_pw_top(PW_TOP);
        assert_eq!(stats.rows.len(), 2);
        assert_eq!(
            stats.errors, 3,
            "the last sample's ERR, not the sum of both"
        );
        let holaf = stats.rows.iter().find(|row| row.id == 52).unwrap();
        assert_eq!(holaf.quantum, 128);
        assert_eq!(holaf.rate, 48_000);
        assert_eq!(holaf.err, 3);
        assert_eq!(holaf.state, "R");
        assert_eq!(holaf.busy, "0.7us", "the second block carries the values");
        assert_eq!(holaf.format.as_deref(), Some("F32LE"));
        assert_eq!(holaf.name.as_deref(), Some("Holaf_Bus_Mix_Out"));
        assert_eq!(stats.quantum_of("Holaf_Bus_Mix_Out"), Some(128));
        assert_eq!(stats.quantum_of("nope"), None);
    }

    #[test]
    fn pw_top_without_batch_mode_parses_to_nothing() {
        let stats = parse_pw_top("pw-top: invalid option -- 'b'\n");
        assert!(stats.is_empty());
        assert_eq!(stats.errors, 0);
        assert!(parse_pw_top("").is_empty());
    }

    #[test]
    fn the_worst_offenders_are_listed_by_error_count() {
        let stats = parse_pw_top(
            "R  1  256  48000  1us  1us  0.00  0.00    2    F32LE 2 48000  a\n\
             R  2  256  48000  1us  1us  0.00  0.00  150    F32LE 2 48000  b\n\
             R  3  256  48000  1us  1us  0.00  0.00    0    F32LE 2 48000  c\n",
        );
        let worst = stats.worst(2);
        assert_eq!(worst.len(), 2, "only the rows with errors");
        assert_eq!(worst[0].name.as_deref(), Some("b"));
        assert_eq!(stats.errors, 152);
    }

    #[test]
    fn the_payload_names_its_source_and_reports_quantum_and_absence_of_pw_top() {
        let payload = collect_with(|bin, _args| {
            if bin == "pw-metadata" {
                Ok(SETTINGS.to_string())
            } else {
                Err("pw-top: invalid option -- 'b'".to_string())
            }
        });
        assert_eq!(payload["source"], json!("pw-metadata settings"));
        assert_eq!(payload["quantum"], json!(1024));
        assert_eq!(payload["effective_quantum"], json!(1024));
        assert_eq!(payload["settings_error"], Value::Null);
        assert_eq!(payload["pw_top"]["available"], json!(false));
        assert_eq!(
            payload["pw_top"]["reason"],
            json!("pw-top has no batch mode (needs PipeWire >= 0.3.66)")
        );
        assert_eq!(payload["pw_top"]["errors"], json!(0));
    }

    #[test]
    fn a_running_pw_top_is_published_with_its_rows() {
        let payload = collect_with(|bin, _args| {
            if bin == "pw-metadata" {
                Ok(SETTINGS.to_string())
            } else {
                Ok(PW_TOP.to_string())
            }
        });
        assert_eq!(payload["pw_top"]["available"], json!(true));
        assert_eq!(payload["pw_top"]["errors"], json!(3));
        let rows = payload["pw_top"]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["name"], json!("Holaf_Bus_Mix_Out"));
        assert_eq!(rows[1]["quantum"], json!(128));
    }

    #[test]
    fn a_missing_tool_is_reported_not_fatal() {
        let payload = collect_with(|bin, _args| Err(format!("cannot run {bin}: not found")));
        assert_eq!(
            payload["settings_error"],
            json!("cannot run pw-metadata: not found")
        );
        // The defaults are still published, so the report has numbers to show.
        assert_eq!(payload["quantum"], json!(1024));
        assert_eq!(payload["pw_top"]["available"], json!(false));
        assert!(payload["pw_top"]["reason"]
            .as_str()
            .unwrap()
            .contains("pw-top"));
    }

    /// The live daemon of this sandbox answers both ways; skipped when there is
    /// none (the unit tests above cover the logic).
    #[test]
    fn collect_against_a_live_daemon_when_there_is_one() {
        // Two independent conditions, and BOTH must hold:
        //
        // 1. the runtime dir looks like the throwaway sandbox — this test
        //    *writes* `clock.force-quantum` on the daemon, so it must never
        //    touch a real user session;
        // 2. that daemon is actually **reachable**.  The path alone proves
        //    nothing: `start-pipewire-headless.sh --export` only exports the
        //    variables, so a shell that keeps them after a `stop` (the normal
        //    way to switch the sandbox off) still has `XDG_RUNTIME_DIR` set with
        //    no socket behind it.  Gating on the name alone made this test
        //    *fail* (`pw-metadata … can't connect: Host is down`) instead of
        //    skipping — the one thing a live test must never do.
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        if !(dir.contains("pw-runtime") || dir.contains("holaf")) {
            eprintln!("SKIP: no headless PipeWire configured");
            return;
        }
        if let Err(reason) = run("pw-metadata", &["-n", "settings"]) {
            eprintln!("SKIP: the headless PipeWire daemon does not answer: {reason}");
            return;
        }
        let payload = collect();
        eprintln!(
            "timing → {}",
            serde_json::to_string_pretty(&payload).unwrap()
        );
        assert_eq!(payload["source"], json!("pw-metadata settings"));
        assert_eq!(payload["settings_error"], Value::Null);
        assert_eq!(payload["rate"], json!(48_000));
        assert!(
            payload["effective_quantum"].as_u64().unwrap_or(0) >= 16,
            "{payload}"
        );
        // On 0.3.65 `pw-top -b` does not exist: the payload must say so with a
        // reason instead of pretending the counters are zero.
        let top = &payload["pw_top"];
        if top["available"] == json!(false) {
            assert!(top["reason"].as_str().is_some_and(|r| !r.is_empty()));
        }

        // Force a 64-sample quantum — the crackle case the `small_quantum`
        // finding exists for — read it back from the daemon, then restore.  The
        // sandbox daemon is throwaway, but it is still restored so the other
        // live tests are not perturbed.
        let force = |value: &str| {
            let _ = std::process::Command::new("pw-metadata")
                .args(["-n", "settings", "0", "clock.force-quantum", value])
                .output();
        };
        force("64");
        let forced = collect();
        force("0");
        assert_eq!(forced["force_quantum"], json!(64), "{forced}");
        assert_eq!(forced["effective_quantum"], json!(64), "{forced}");
        assert!(
            forced["quantum_ms"].as_f64().unwrap_or(0.0) < 2.0,
            "64 samples at 48 kHz is ~1.3 ms: {forced}"
        );
        // …and the setting really came back.
        let restored = collect();
        assert_eq!(restored["effective_quantum"], json!(1024), "{restored}");
    }
}
