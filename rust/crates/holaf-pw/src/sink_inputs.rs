//! Per-application routing: list the audio streams playing on the server, and
//! move one of them onto another sink.
//!
//! This is the Rust twin of `src/backend/pipewire_utils.py`
//! (`get_sink_inputs` / `move_sink_input`) that the Qt build has been using, and
//! it answers [`holaf_core::backend::Backend::app_list`] /
//! [`holaf_core::backend::Backend::app_move`].
//!
//! # Why `pactl` and not the PipeWire registry
//!
//! Moving a stream is **not** a node property: in PipeWire the links between a
//! stream's output ports and a sink's input ports are owned by the *session
//! manager* (WirePlumber).  Destroying/re-creating them by hand from the
//! registry is a race against that policy engine — it may re-link on the next
//! device change, and a client that re-creates its stream (every new track in a
//! browser) undoes the move anyway, which is exactly why the Qt UI enforces the
//! routing every 5 s.  The supported, session-manager-aware operation is
//! `pactl move-sink-input`, which is what the Qt build calls, and `pactl` is
//! part of `pipewire-pulse`, i.e. already required by anything that lets a
//! PulseAudio application talk to the server.
//!
//! The registry cannot even *see* what we need: it exports a fixed key subset
//! (see [`crate::live`]), and `application.name` / `application.process.id` are
//! not part of it.  `pactl -f json list sink-inputs` publishes them, plus the
//! PulseAudio index `move-sink-input` takes.
//!
//! # What "a stream" is
//!
//! `pactl list sink-inputs` lists exactly the **playback** streams, i.e. the
//! PipeWire nodes with `media.class = Stream/Output/Audio` (PulseAudio calls them
//! sink-inputs because they feed a sink).  Recording streams are *source-outputs*
//! and never appear here — which is why the metering `pw-cat --record` helpers
//! cannot pollute the list, and why they are still filtered by pid (see below)
//! rather than by name: a future daemon-side playback helper would.
//!
//! # What is excluded
//!
//! The daemon must never list itself, or the UI would offer to move the mixer's
//! own streams, or the `pw-cat` recorders that feed the VU meters:
//!
//! * the daemon's own pid (`application.process.id`), and
//! * any stream whose process is a **child** of the daemon (`/proc/<pid>/stat`
//!   ppid) — that is what the metering `pw-cat --record` helpers are, without
//!   matching a fragile name substring (the lesson the Qt comment records), and
//! * streams whose `node.name` is one of our graph nodes (`Holaf_*`,
//!   `input.Holaf_FX_*`, `output.Holaf_FX_*`), and
//! * the Qt application itself (`application.name == "Holaf-Mix"`).
//!
//! # Purity
//!
//! Everything except the two thin `pactl` calls is pure and unit-tested against
//! recorded `pactl` JSON: parsing, the name/pid fallbacks, the exclusion policy
//! and the payload shape.  The process probes (pid/ppid) are injected, so no
//! test needs a live server.

use std::collections::HashMap;
use std::process::Command;

use serde_json::{json, Map, Value};

use holaf_core::backend::BackendError;

/// One audio stream playing on the server (`pactl list sink-inputs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkInput {
    /// PulseAudio sink-input index — what `pactl move-sink-input` takes, and what
    /// the UI sends back as `app.id`.
    pub index: u32,
    /// `application.name`, falling back to `application.process.binary` then
    /// `node.name`: this is what the user checks in the dialog and what
    /// `assigned_apps` stores.
    pub name: String,
    /// `application.process.id`, when the client publishes it (`pw-cat` does
    /// not; a browser does).
    pub pid: Option<u32>,
    /// `node.name` of the stream itself (`pw-cat`, `firefox`, …).
    pub node_name: Option<String>,
    /// `pactl`'s `sink` index, `None` when the stream is not routed yet
    /// (unconnected: PulseAudio reports `4294967295`).
    pub sink_index: Option<u32>,
    /// `node.name` of the sink it plays on — filled in from `list sinks`.
    pub sink: Option<String>,
    /// `application.icon_name` (empty when absent).
    pub icon: String,
}

impl SinkInput {
    /// The stream is not attached to any sink (corked/unconnected): it reports
    /// no sink index, and [`attach_sinks`] resolved nothing either.
    pub fn is_unrouted(&self) -> bool {
        self.sink_index.is_none() && self.sink.is_none()
    }
}

fn as_u32(value: &Value) -> Option<u32> {
    match value {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_str(entry: &Map<String, Value>, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Parse `pactl -f json list sink-inputs`.
///
/// Tolerant by contract: an entry without a usable `index` is skipped and a
/// malformed document yields an empty list — a listing failure must never take
/// the daemon down (the Qt build prints and returns `[]`).
///
/// The sink is left `None` here (the payload carries an index only); call
/// [`parse_sinks`] and [`attach_sinks`] to resolve it to a `node.name`.
pub fn parse_sink_inputs(json_text: &str) -> Vec<SinkInput> {
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(json_text) else {
        return Vec::new();
    };
    let mut inputs = Vec::with_capacity(entries.len());
    for entry in &entries {
        let Some(object) = entry.as_object() else {
            continue;
        };
        // PulseAudio reports "no sink" as -1 → 4294967295 once unsigned.
        let Some(index) = object.get("index").and_then(as_u32) else {
            continue;
        };
        let props = object
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let node_name = as_str(&props, "node.name");
        let Some(name) = as_str(&props, "application.name")
            .or_else(|| as_str(&props, "application.process.binary"))
            .or_else(|| node_name.clone())
        else {
            continue; // an unnamed stream cannot be assigned to a strip
        };
        inputs.push(SinkInput {
            index,
            name,
            pid: props.get("application.process.id").and_then(as_u32),
            node_name,
            sink_index: object
                .get("sink")
                .and_then(as_u32)
                .filter(|index| *index != u32::MAX),
            sink: None,
            icon: as_str(&props, "application.icon_name").unwrap_or_default(),
        });
    }
    inputs
}

/// Parse `pactl -f json list sinks` into `index → node.name`.
///
/// `node.name` is the name a strip's bus is targeted by (and what
/// `status.strips[].node_name` publishes), so it — not the PulseAudio
/// `description` — is what `app.sink` must contain.  The PulseAudio `name` is
/// used as a fallback: for a `module-null-sink`/`adapter` sink they are equal.
pub fn parse_sinks(json_text: &str) -> HashMap<u32, String> {
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(json_text) else {
        return HashMap::new();
    };
    let mut sinks = HashMap::with_capacity(entries.len());
    for entry in &entries {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let Some(index) = object.get("index").and_then(as_u32) else {
            continue;
        };
        let props = object.get("properties").and_then(Value::as_object);
        let name = props
            .and_then(|props| as_str(props, "node.name"))
            .or_else(|| as_str(object, "name"));
        if let Some(name) = name {
            sinks.insert(index, name);
        }
    }
    sinks
}

/// Resolve every stream's sink index to the sink's `node.name`.
pub fn attach_sinks(inputs: &mut [SinkInput], sinks: &HashMap<u32, String>) {
    for input in inputs.iter_mut() {
        input.sink = input
            .sink_index
            .and_then(|index| sinks.get(&index))
            .cloned();
    }
}

/// Why a stream was left out of the list (reported in the payload so the UI can
/// explain an empty dialog instead of just showing nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// `application.process.id` is the daemon itself.
    OwnPid,
    /// The stream's process is a child of the daemon (a metering `pw-cat`).
    OwnChild,
    /// `node.name` is one of our graph nodes (`Holaf_*`, `…Holaf_FX_*`).
    OurNode,
    /// The Qt application (`application.name == "Holaf-Mix"`).
    HolafApp,
}

impl Exclusion {
    pub fn reason(self) -> &'static str {
        match self {
            Exclusion::OwnPid => "own process",
            Exclusion::OwnChild => "child process (metering recorder)",
            Exclusion::OurNode => "Holaf node",
            Exclusion::HolafApp => "Holaf-Mix application",
        }
    }
}

/// The daemon's own `node.name` prefixes — a stream carrying one of those is one
/// of ours (see `holaf_core::graph`).
pub const OWN_NODE_PREFIXES: [&str; 3] = ["Holaf_", "input.Holaf_FX_", "output.Holaf_FX_"];

/// Is `input` one of our own streams? `parent_of` is injected (`/proc` in
/// production) so the policy stays pure and testable.
pub fn exclusion(
    input: &SinkInput,
    own_pid: u32,
    parent_of: &dyn Fn(u32) -> Option<u32>,
) -> Option<Exclusion> {
    if input.pid == Some(own_pid) {
        return Some(Exclusion::OwnPid);
    }
    if input
        .pid
        .and_then(parent_of)
        .is_some_and(|parent| parent == own_pid)
    {
        return Some(Exclusion::OwnChild);
    }
    if let Some(node_name) = &input.node_name {
        if OWN_NODE_PREFIXES
            .iter()
            .any(|prefix| node_name.starts_with(prefix))
        {
            return Some(Exclusion::OurNode);
        }
    }
    if input.name == "Holaf-Mix" {
        return Some(Exclusion::HolafApp);
    }
    None
}

/// The `app_list` payload: the streams a strip may take over, plus what was
/// filtered out and why.
///
/// Shape (the contract `holaf_ui::apps::AppList::parse` reads):
///
/// ```json
/// {"apps": [{"id": "52", "name": "firefox", "pid": 4242,
///            "sink": "Holaf_Bus_Web", "node_name": "firefox", "icon": "",
///            "index": 52}],
///  "stub": false, "source": "pactl",
///  "excluded": [{"index": 51, "name": "pw-cat", "reason": "child process …"}]}
/// ```
///
/// `sink` is the **sink's `node.name`** (never the PulseAudio index): that is
/// what `holaf_ui::apps::plan` compares with `status.strips[].node_name`, so an
/// application already on its bus produces no move.
pub fn app_list_payload(
    mut inputs: Vec<SinkInput>,
    sinks: &HashMap<u32, String>,
    own_pid: u32,
    parent_of: &dyn Fn(u32) -> Option<u32>,
) -> Value {
    attach_sinks(&mut inputs, sinks);
    let mut apps = Vec::with_capacity(inputs.len());
    let mut excluded = Vec::new();
    for input in inputs {
        if let Some(reason) = exclusion(&input, own_pid, parent_of) {
            excluded.push(json!({
                "index": input.index,
                "name": input.name,
                "reason": reason.reason(),
            }));
            continue;
        }
        apps.push(json!({
            "id": input.index.to_string(),
            "index": input.index,
            "name": input.name,
            "pid": input.pid,
            "sink": input.sink,
            "node_name": input.node_name,
            "icon": input.icon,
        }));
    }
    json!({
        "apps": apps,
        "stub": false,
        "source": "pactl",
        "excluded": excluded,
    })
}

/// `ppid` of `pid`, read from `/proc/<pid>/stat`.
///
/// The `stat` line wraps `comm` in parentheses and the process name may contain
/// spaces or parentheses, so the fields are counted from the **last** `)` — this
/// is the one subtlety of the format.
pub fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    // After ")" the fields are: state ppid pgrp … → `ppid` is the 2nd token.
    let mut fields = stat[close + 1..].split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

/// Run one `pactl` invocation and return its stdout.
///
/// A missing `pactl` is a *configuration* fact, not an internal error: it is
/// reported as `pactl_missing` so the UI can say "this daemon cannot manage
/// applications" instead of showing an empty list as if nothing were playing.
pub fn run_pactl(args: &[&str]) -> Result<String, BackendError> {
    let output = Command::new("pactl").args(args).output().map_err(|err| {
        BackendError::new(
            "pactl_missing",
            format!("cannot run pactl ({}): {err}", args.join(" ")),
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BackendError::new(
            "pactl_failed",
            format!("pactl {} failed: {}", args.join(" "), stderr.trim()),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// List the server's streams, minus our own (see [`exclusion`]).
pub fn list(own_pid: u32, parent_of: &dyn Fn(u32) -> Option<u32>) -> Result<Value, BackendError> {
    let inputs = run_pactl(&["-f", "json", "list", "sink-inputs"])?;
    // The sink list only decorates the answer: a server that cannot list sinks
    // still lets us list (and move) streams, they just show sink: null.
    let sinks = run_pactl(&["-f", "json", "list", "sinks"])
        .map(|json| parse_sinks(&json))
        .unwrap_or_default();
    Ok(app_list_payload(
        parse_sink_inputs(&inputs),
        &sinks,
        own_pid,
        parent_of,
    ))
}

/// Move stream `app_id` onto the sink whose `node.name` is `sink_name`, then
/// read the routing back.
///
/// The answer is `{"ok": true, …}` when the stream is *verified* to be on the
/// target afterwards, and `{"ok": false, "note": …}` otherwise — the shape the
/// UI already renders (`holaf_ui::bridge` turns `ok: false` into a status-line
/// failure naming the reason).
pub fn move_stream(
    app_id: &str,
    sink_name: &str,
    own_pid: u32,
    parent_of: &dyn Fn(u32) -> Option<u32>,
) -> Result<Value, BackendError> {
    let index: u32 = app_id.trim().parse().map_err(|_| {
        BackendError::new(
            "bad_app_id",
            format!("'{app_id}' is not a sink-input index (app_list publishes one)"),
        )
    })?;
    if sink_name.trim().is_empty() {
        return Err(BackendError::new("bad_sink", "an empty sink name"));
    }
    let target = sink_name.trim();
    if let Err(err) = run_pactl(&["move-sink-input", &index.to_string(), target]) {
        return Ok(json!({
            "ok": false,
            "id": index,
            "sink": target,
            "note": err.message,
        }));
    }
    // Read-back: `pactl` may accept the command and the session manager may
    // still refuse the link (a device that vanished in between).  Never report
    // a move we did not verify.
    let after = list(own_pid, parent_of)?;
    let landed = after
        .get("apps")
        .and_then(Value::as_array)
        .and_then(|apps| {
            apps.iter()
                .find(|app| app.get("index").and_then(as_u32) == Some(index))
        })
        .and_then(|app| app.get("sink"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let ok = landed.as_deref() == Some(target);
    Ok(json!({
        "ok": ok,
        "id": index,
        "sink": target,
        "now_on": landed,
        "note": if ok {
            Value::Null
        } else {
            json!(format!(
                "pactl accepted the move but the stream is on {:?}",
                landed
            ))
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `application.process.id` a real client publishes is a string, the
    /// index an integer, and `pw-cat` publishes no pid at all.
    const INPUTS: &str = r#"[
      {"index": 52, "sink": 44, "corked": false,
       "properties": {"application.name": "firefox", "node.name": "firefox",
                      "application.process.id": "4242",
                      "application.icon_name": "firefox"}},
      {"index": 53, "sink": 44, "corked": false,
       "properties": {"application.name": "chromium", "node.name": "chromium",
                      "application.process.binary": "chromium"}},
      {"index": 54, "sink": 4294967295, "corked": true,
       "properties": {"node.name": "pw-cat", "media.class": "Stream/Output/Audio"}},
      {"index": 55, "sink": 45, "corked": false,
       "properties": {"application.name": "Holaf-Mix", "application.process.id": "9"}},
      {"index": 56, "sink": 45, "corked": false,
       "properties": {"application.name": "recorder", "application.process.id": "77"}},
      {"index": 57, "sink": 45, "corked": false,
       "properties": {"application.name": "own", "application.process.id": "1"}},
      {"corked": false, "properties": {"application.name": "no index"}},
      {"index": 58, "properties": {}}
    ]"#;

    const SINKS: &str = r#"[
      {"index": 44, "name": "Holaf_Bus_Web",
       "properties": {"node.name": "Holaf_Bus_Web"}},
      {"index": 45, "name": "alsa_output.pci-0000_00_1f.3.analog-stereo",
       "properties": {"node.name": "alsa_output.pci-0000_00_1f.3.analog-stereo"}},
      {"index": 46, "name": "description-only"}
    ]"#;

    #[test]
    fn parsing_prefers_application_name_then_binary_then_node_name() {
        let inputs = parse_sink_inputs(INPUTS);
        // 7 entries, one without index and one without any name → dropped.
        assert_eq!(inputs.len(), 6);
        assert_eq!(inputs[0].name, "firefox");
        assert_eq!(inputs[0].pid, Some(4242));
        // No `application.name` → `application.process.binary`.
        assert_eq!(inputs[1].name, "chromium");
        assert_eq!(inputs[1].pid, None);
        // `pw-cat` publishes neither → `node.name`.
        assert_eq!(inputs[2].name, "pw-cat");
        assert_eq!(inputs[2].node_name.as_deref(), Some("pw-cat"));
    }

    #[test]
    fn an_unsigned_sink_index_of_max_means_unrouted() {
        let inputs = parse_sink_inputs(INPUTS);
        assert_eq!(inputs[0].sink_index, Some(44));
        assert_eq!(
            inputs[2].sink_index, None,
            "4294967295 (PulseAudio -1) is 'no sink', not a sink id"
        );
        assert!(inputs[2].is_unrouted());
        assert!(!inputs[0].is_unrouted());
    }

    #[test]
    fn sinks_are_resolved_by_node_name_and_attached_to_the_stream() {
        let sinks = parse_sinks(SINKS);
        assert_eq!(sinks.get(&44).map(String::as_str), Some("Holaf_Bus_Web"));
        assert_eq!(sinks.get(&46).map(String::as_str), Some("description-only"));
        let mut inputs = parse_sink_inputs(INPUTS);
        attach_sinks(&mut inputs, &sinks);
        assert_eq!(inputs[0].sink.as_deref(), Some("Holaf_Bus_Web"));
        assert_eq!(inputs[2].sink, None);
    }

    #[test]
    fn a_broken_document_is_an_empty_list_not_a_panic() {
        assert!(parse_sink_inputs("not json").is_empty());
        assert!(parse_sink_inputs("{}").is_empty());
        assert!(parse_sink_inputs("[]").is_empty());
        assert!(parse_sinks("\"nope\"").is_empty());
    }

    /// The daemon must not list itself: its own pid, its children (the metering
    /// `pw-cat` recorders), its graph nodes, and the Qt application.
    #[test]
    fn our_own_streams_are_excluded_with_a_reason() {
        // Process 1 = the daemon; 77 is one of its children.
        let parent_of = |pid: u32| -> Option<u32> {
            match pid {
                77 => Some(1),
                4242 => Some(900),
                _ => None,
            }
        };
        let payload = app_list_payload(
            parse_sink_inputs(INPUTS),
            &parse_sinks(SINKS),
            1,
            &parent_of,
        );
        let apps = payload["apps"].as_array().unwrap();
        let names: Vec<&str> = apps.iter().map(|a| a["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["firefox", "chromium", "pw-cat"],
            "a user's own pw-cat is legitimately listed"
        );
        assert_eq!(payload["stub"], json!(false));

        let excluded = payload["excluded"].as_array().unwrap();
        let reasons: Vec<&str> = excluded
            .iter()
            .map(|e| e["reason"].as_str().unwrap())
            .collect();
        assert_eq!(
            reasons,
            vec![
                "Holaf-Mix application",
                "child process (metering recorder)",
                "own process"
            ]
        );
    }

    #[test]
    fn a_stream_carrying_one_of_our_node_names_is_excluded() {
        let input = SinkInput {
            index: 60,
            name: "pw-cat".into(),
            pid: None,
            node_name: Some("output.Holaf_FX_out-bus".into()),
            sink_index: Some(44),
            sink: Some("Holaf_Bus_Web".into()),
            icon: String::new(),
        };
        let none = |_: u32| None;
        assert_eq!(exclusion(&input, 1, &none), Some(Exclusion::OurNode));
    }

    /// `app.id` is the index `pactl move-sink-input` takes, published as a
    /// string (the UI stores it verbatim) and repeated as `index` for scripts.
    #[test]
    fn the_payload_publishes_the_index_as_id_and_the_sink_as_a_node_name() {
        let payload =
            app_list_payload(parse_sink_inputs(INPUTS), &parse_sinks(SINKS), 1, &|_| None);
        let firefox = &payload["apps"][0];
        assert_eq!(firefox["id"], json!("52"));
        assert_eq!(firefox["index"], json!(52));
        assert_eq!(firefox["pid"], json!(4242));
        assert_eq!(firefox["sink"], json!("Holaf_Bus_Web"));
        assert_eq!(firefox["node_name"], json!("firefox"));
    }

    #[test]
    fn an_empty_server_lists_nothing_and_excludes_nothing() {
        let payload = app_list_payload(Vec::new(), &HashMap::new(), 1, &|_| None);
        assert_eq!(payload["apps"], json!([]));
        assert_eq!(payload["excluded"], json!([]));
        assert_eq!(payload["stub"], json!(false));
    }

    #[test]
    fn a_move_without_a_numeric_id_is_refused_before_touching_the_server() {
        let err = move_stream("firefox", "Holaf_Bus_Web", 1, &|_| None).unwrap_err();
        assert_eq!(err.code, "bad_app_id");
        let err = move_stream("52", "  ", 1, &|_| None).unwrap_err();
        assert_eq!(err.code, "bad_sink");
    }

    /// `/proc/<pid>/stat` wraps the process name in parentheses and that name may
    /// contain spaces or parentheses: `ppid` must be read after the **last** `)`.
    #[test]
    fn the_parent_pid_is_read_after_the_last_parenthesis() {
        let parent = parent_pid(std::process::id());
        assert!(parent.is_some(), "a running process has a parent");
        // Our own parent is a real pid (not zero, not ourselves).
        assert_ne!(parent, Some(std::process::id()));
        assert_eq!(parent_pid(u32::MAX), None, "no such process");
    }
}
