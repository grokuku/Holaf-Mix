//! Per-application routing — the Rust twin of the Qt `AppSelectionDialog` +
//! `_enforce_app_routing`.
//!
//! # What the Qt UI does
//!
//! `pipewire_utils.get_sink_inputs()` lists the applications that are currently
//! playing (`pactl list sink-inputs`), the "SELECT APPS" button opens a dialog of
//! checkboxes, the checked names are stored in `strip.assigned_apps`, and a
//! 5-second timer (`ENFORCE_ROUTING_INTERVAL_MS`) moves every assigned
//! application back onto its strip's bus — because an application re-creates its
//! sink-input (and forgets the sink) whenever it restarts or changes track.
//!
//! # What is reproduced here
//!
//! * [`AppList::parse`] reads `Backend::app_list()`'s payload: the Python field
//!   names (`id`, `name`, `target_node`) plus the fields the Rust daemon adds
//!   (`sink` as a **sink node name**, `source` naming where the list came from,
//!   `excluded` explaining what was filtered out).  The old stub payload
//!   (`{"apps": [], "stub": true, "note": …}`) is still understood — an engine
//!   that cannot manage applications must say so, not look idle.
//! * [`plan`] is the whole enforcement policy as a pure function: strips → the
//!   applications that must move, with the destination sink name.  It is
//!   idempotent (an application already on the right bus produces nothing),
//!   which is what makes a 5-second timer safe.
//! * [`AppSelection`] is the dialog state (candidates + checkboxes), and
//!   [`candidate_names`] the Qt union rule (`running ∪ assigned`, sorted).
//!
//! The dialog itself is drawn by [`crate::dialogs`]; the model is here so the
//! policy is unit-tested without a window or a daemon.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use holaf_core::model::{Strip, StripType};

/// How often the assigned applications are put back on their bus
/// (Qt `ENFORCE_ROUTING_INTERVAL_MS`).
pub const ENFORCE_INTERVAL: Duration = Duration::from_secs(5);

/// One application currently producing audio (`Backend::app_list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    /// What `app_move` takes (PulseAudio sink-input index, as a string).
    pub id: String,
    /// `application.name` — what the user checks in the dialog and what
    /// `assigned_apps` stores.
    pub name: String,
    pub pid: Option<u32>,
    /// The sink it is playing on right now (`None` when unknown).
    pub sink: Option<String>,
}

impl AppInfo {
    /// `firefox (4242)` — what the dialog lists next to the checkbox.
    pub fn label(&self) -> String {
        match self.pid {
            Some(pid) => format!("{} ({pid})", self.name),
            None => self.name.clone(),
        }
    }
}

/// The answer to `Backend::app_list()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppList {
    pub apps: Vec<AppInfo>,
    /// The engine answered with the documented stub payload it used before app
    /// management existed: nothing can be moved there.
    pub stub: bool,
    /// Where the engine got the list (`"pactl"` for the real daemon). `Some`
    /// means "a capable engine answered", which an **empty** list must not be
    /// confused with: "nothing is playing" is not "this engine cannot move
    /// applications".
    pub source: Option<String>,
    pub note: Option<String>,
}

impl AppList {
    /// Parse an `app_list()` payload, tolerantly (a broken payload is an empty
    /// list, never an error).
    pub fn parse(payload: &serde_json::Value) -> Self {
        let stub = payload
            .get("stub")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let note = payload
            .get("note")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let source = payload
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let apps = payload
            .get("apps")
            .and_then(serde_json::Value::as_array)
            .map(|entries| entries.iter().filter_map(parse_app).collect())
            .unwrap_or_default();
        Self {
            apps,
            stub,
            source,
            note,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    /// The applications playing on `sink` (byte-exact: sink names are stable).
    pub fn on_sink<'a>(&'a self, sink: &'a str) -> impl Iterator<Item = &'a AppInfo> {
        self.apps
            .iter()
            .filter(move |app| app.sink.as_deref() == Some(sink))
    }
}

fn parse_app(entry: &serde_json::Value) -> Option<AppInfo> {
    let id = match entry.get("id")? {
        serde_json::Value::String(id) => id.trim().to_string(),
        serde_json::Value::Number(id) => id.to_string(),
        _ => return None,
    };
    if id.is_empty() {
        return None;
    }
    let name = entry
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())?;
    Some(AppInfo {
        id,
        name: name.to_string(),
        pid: entry
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .map(|pid| pid as u32),
        sink: entry
            .get("sink")
            .or_else(|| entry.get("target_node"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

/// Does this strip take over the applications assigned to it?
///
/// Exactly the Qt condition: an **input**, **virtual** (`device_name == None`,
/// which is what `mode == VIRTUAL` meant) and **not the default sink** (the
/// default sink already receives everything).
pub fn enforces(strip: &Strip, assigned: &[String]) -> bool {
    strip.kind == StripType::Input
        && strip.device_name.is_none()
        && !strip.is_default
        && !assigned.is_empty()
}

/// The sink name an application must be moved to for `strip`.
///
/// The daemon publishes it as `status.strips[].node_name` (`Holaf_Bus_<Label>`,
/// `_2` on a label collision) and that is authoritative; the fallback rebuilds
/// the same name from the label for a UI that never got a status (mock engine,
/// no daemon).  Rebuilding it from the **uid** would be the pre-J5 shape
/// (`Holaf_Strip_<uid>`) and is deliberately not used.
pub fn sink_name_for(
    strips: &[Strip],
    uid: &str,
    node_names: &HashMap<String, String>,
) -> Option<String> {
    if let Some(name) = node_names.get(uid) {
        if !name.trim().is_empty() {
            return Some(name.clone());
        }
    }
    let index = strips.iter().position(|strip| strip.uid == uid)?;
    let strip = &strips[index];
    let ordinal = strips[..=index]
        .iter()
        .filter(|other| {
            holaf_core::graph::slug(&other.label) == holaf_core::graph::slug(&strip.label)
        })
        .count();
    Some(holaf_core::graph::bus_node_name(&strip.label, ordinal))
}

/// One application the UI must move, and where to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppMove {
    pub app_id: String,
    pub app_name: String,
    pub sink: String,
    /// The strip that owns this application (for logging/status).
    pub uid: String,
}

/// The enforcement policy: which applications have to move, and where.
///
/// * an application assigned to a strip that *enforces* (see [`enforces`]) and
///   not already on that strip's bus is moved;
/// * an application assigned to two strips is moved by the **first** strip in
///   the list (deterministic, never a fight between two timers);
/// * an application that is not running produces nothing (nothing to move);
/// * a strip whose bus name cannot be resolved produces nothing.
pub fn plan(
    strips: &[Strip],
    assigned: &HashMap<String, Vec<String>>,
    node_names: &HashMap<String, String>,
    running: &AppList,
) -> Vec<AppMove> {
    let mut moves: Vec<AppMove> = Vec::new();
    let mut claimed: HashSet<&str> = HashSet::new();
    for strip in strips {
        let names = assigned.get(&strip.uid).map(Vec::as_slice).unwrap_or(&[]);
        if !enforces(strip, names) {
            continue;
        }
        let Some(sink) = sink_name_for(strips, &strip.uid, node_names) else {
            log::warn!("apps: cannot resolve the bus name of {:?}", strip.label);
            continue;
        };
        for app in &running.apps {
            if !names.contains(&app.name) {
                continue;
            }
            if app.sink.as_deref() == Some(sink.as_str()) {
                continue; // already on its bus: nothing to do
            }
            if !claimed.insert(app.name.as_str()) {
                log::debug!(
                    "apps: {:?} is assigned twice, {:?} keeps it",
                    app.name,
                    strip.label
                );
                continue;
            }
            moves.push(AppMove {
                app_id: app.id.clone(),
                app_name: app.name.clone(),
                sink: sink.clone(),
                uid: strip.uid.clone(),
            });
        }
    }
    moves
}

/// The checkbox list of the "SELECT APPS" dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSelection {
    pub uid: String,
    pub label: String,
    /// Names offered, sorted (running applications ∪ already assigned ones).
    pub candidates: Vec<String>,
    /// Checked names.
    pub selected: Vec<String>,
    /// The engine's payload was the documented stub: moving is a no-op there.
    pub stub: bool,
    pub note: Option<String>,
}

impl AppSelection {
    /// Build the dialog state from the strip, its assignment and the last app
    /// list (the Qt `AppSelectionDialog.__init__`).
    pub fn new(uid: &str, label: &str, assigned: &[String], list: &AppList) -> Self {
        Self {
            uid: uid.to_string(),
            label: label.to_string(),
            candidates: candidate_names(list, assigned),
            selected: assigned.to_vec(),
            stub: list.stub,
            note: list.note.clone(),
        }
    }

    /// One line of the dialog's hint.
    pub fn hint(&self) -> String {
        if self.stub {
            self.note
                .clone()
                .unwrap_or_else(|| "this engine cannot move applications".to_string())
        } else if self.candidates.is_empty() {
            "no application is playing audio right now".to_string()
        } else {
            "checked applications are routed to this strip and kept there".to_string()
        }
    }

    pub fn is_selected(&self, name: &str) -> bool {
        self.selected.iter().any(|selected| selected == name)
    }

    pub fn toggle(&mut self, name: &str) {
        if self.is_selected(name) {
            self.selected.retain(|selected| selected != name);
        } else {
            self.selected.push(name.to_string());
        }
    }

    /// The checked names, in candidate order (what the model stores).
    pub fn chosen(&self) -> Vec<String> {
        let mut chosen: Vec<String> = self
            .candidates
            .iter()
            .filter(|name| self.is_selected(name))
            .cloned()
            .collect();
        // A previously assigned application that is not running any more is
        // still kept (it may come back), exactly like the Qt dialog which lists
        // the union.
        for name in &self.selected {
            if !chosen.iter().any(|existing| existing == name) {
                chosen.push(name.clone());
            }
        }
        chosen
    }
}

/// `running ∪ assigned`, sorted — the Qt dialog's candidate list.
pub fn candidate_names(list: &AppList, assigned: &[String]) -> Vec<String> {
    let mut names: Vec<String> = list.apps.iter().map(|app| app.name.clone()).collect();
    for name in assigned {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.clone());
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use holaf_core::model::Strip;
    use serde_json::json;

    fn strip(uid: &str, label: &str, kind: StripType, device: Option<&str>) -> Strip {
        let mut strip = Strip::with_uid(uid, label, kind);
        strip.device_name = device.map(str::to_string);
        strip
    }

    fn apps() -> AppList {
        AppList::parse(&json!({
            "apps": [
                {"id": 12, "name": "firefox", "pid": 4242, "target_node": "alsa_output.other.analog-stereo"},
                {"id": "13", "name": "discord", "sink": "Holaf_Bus_Mic"},
                {"id": 14, "name": "mpv"},
            ]
        }))
    }

    fn assigned(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(uid, names)| {
                (
                    (*uid).to_string(),
                    names.iter().map(|name| (*name).to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn the_payload_accepts_the_python_field_names_and_the_stub() {
        let list = apps();
        assert!(!list.stub);
        assert_eq!(list.apps.len(), 3);
        assert_eq!(list.apps[0].id, "12");
        assert_eq!(
            list.apps[0].sink.as_deref(),
            Some("alsa_output.other.analog-stereo")
        );
        assert_eq!(list.apps[1].id, "13", "an id may be a string");
        assert_eq!(list.apps[2].sink, None);
        assert_eq!(list.apps[0].label(), "firefox (4242)");
        assert_eq!(list.apps[2].label(), "mpv");

        let stub = AppList::parse(&json!({
            "apps": [], "stub": true,
            "note": "sink-input management is not implemented in J2"
        }));
        assert!(stub.stub && stub.is_empty());
        assert!(stub.note.unwrap().contains("not implemented"));
        // A real answer names its source, so "nothing is playing" (empty list,
        // capable engine) is distinguishable from the stub.
        let real = AppList::parse(&json!({
            "apps": [], "stub": false, "source": "pactl", "excluded": []
        }));
        assert!(real.is_empty());
        assert!(!real.stub);
        assert_eq!(real.source.as_deref(), Some("pactl"));
        assert_eq!(apps().source, None, "a payload without source stays None");

        // A broken payload is empty, not an error.
        assert!(AppList::parse(&json!({"apps": "nope"})).is_empty());
        assert!(AppList::parse(&json!(null)).is_empty());
        assert!(AppList::parse(&json!({"apps": [{"name": "no id"}]})).is_empty());
    }

    #[test]
    fn only_a_virtual_non_default_input_enforces() {
        let names = vec!["firefox".to_string()];
        assert!(enforces(
            &strip("in-a", "A", StripType::Input, None),
            &names
        ));
        // A physical input has a real source: the apps do not belong there.
        assert!(!enforces(
            &strip("in-b", "B", StripType::Input, Some("alsa_input.x")),
            &names
        ));
        // An output strip never takes applications.
        assert!(!enforces(
            &strip("out-a", "A", StripType::Output, None),
            &names
        ));
        // Nothing assigned: nothing to enforce.
        assert!(!enforces(&strip("in-c", "C", StripType::Input, None), &[]));
        // The default sink already gets everything.
        let mut default = strip("in-d", "D", StripType::Input, None);
        default.is_default = true;
        assert!(!enforces(&default, &names));
    }

    #[test]
    fn the_bus_name_prefers_the_daemon_and_falls_back_to_the_label() {
        let strips = vec![
            strip("in-mic", "Mic", StripType::Input, None),
            strip("out-1", "Mix Out", StripType::Output, None),
        ];
        // The daemon is authoritative (`_2` on a collision cannot be guessed).
        let node_names = HashMap::from([("in-mic".to_string(), "Holaf_Bus_Mic_2".to_string())]);
        assert_eq!(
            sink_name_for(&strips, "in-mic", &node_names).as_deref(),
            Some("Holaf_Bus_Mic_2")
        );
        // Without a status, the label rule of `holaf_core::graph`.
        assert_eq!(
            sink_name_for(&strips, "out-1", &node_names).as_deref(),
            Some("Holaf_Bus_Mix_Out")
        );
        assert_eq!(sink_name_for(&strips, "ghost", &node_names), None);
    }

    #[test]
    fn the_plan_moves_only_what_is_not_already_in_place() {
        let strips = vec![
            strip("in-mic", "Mic", StripType::Input, None),
            strip("out-hp", "HP", StripType::Output, None),
        ];
        let assigned = assigned(&[("in-mic", &["firefox", "discord"])]);
        let moves = plan(&strips, &assigned, &HashMap::new(), &apps());
        // `discord` is already on `Holaf_Bus_Mic`; `firefox` is elsewhere; `mpv`
        // is not assigned.
        assert_eq!(moves.len(), 1, "{moves:?}");
        assert_eq!(moves[0].app_id, "12");
        assert_eq!(moves[0].app_name, "firefox");
        assert_eq!(moves[0].sink, "Holaf_Bus_Mic");
        assert_eq!(moves[0].uid, "in-mic");
        // Idempotence: once it is there, the timer produces nothing.
        let running = AppList::parse(&json!({
            "apps": [{"id": 12, "name": "firefox", "sink": "Holaf_Bus_Mic"}]
        }));
        assert!(plan(&strips, &assigned, &HashMap::new(), &running).is_empty());
    }

    #[test]
    fn an_application_assigned_twice_is_moved_by_the_first_strip_only() {
        let strips = vec![
            strip("in-a", "A", StripType::Input, None),
            strip("in-b", "B", StripType::Input, None),
        ];
        let assigned = assigned(&[("in-a", &["firefox"]), ("in-b", &["firefox"])]);
        let moves = plan(&strips, &assigned, &HashMap::new(), &apps());
        assert_eq!(moves.len(), 1, "{moves:?}");
        assert_eq!(moves[0].uid, "in-a");
        assert_eq!(moves[0].sink, "Holaf_Bus_A");
    }

    #[test]
    fn a_stub_engine_never_produces_a_move() {
        let strips = vec![strip("in-mic", "Mic", StripType::Input, None)];
        let assigned = assigned(&[("in-mic", &["firefox"])]);
        let stub = AppList::parse(&json!({"apps": [], "stub": true, "note": "nope"}));
        assert!(plan(&strips, &assigned, &HashMap::new(), &stub).is_empty());
    }

    #[test]
    fn the_dialog_offers_the_union_of_running_and_assigned_sorted() {
        let list = apps();
        let selection = AppSelection::new(
            "in-mic",
            "Mic",
            &["zoom".to_string(), "discord".to_string()],
            &list,
        );
        assert_eq!(
            selection.candidates,
            ["discord", "firefox", "mpv", "zoom"],
            "running + assigned, sorted"
        );
        assert!(selection.is_selected("discord") && selection.is_selected("zoom"));
        assert!(!selection.is_selected("firefox"));
        // Toggling is a set operation, and `chosen()` keeps the candidate order.
        let mut selection = selection;
        selection.toggle("firefox");
        selection.toggle("discord");
        assert_eq!(selection.chosen(), ["firefox", "zoom"]);
        assert!(selection.hint().contains("routed to this strip"));
    }

    #[test]
    fn the_dialog_says_why_it_is_empty() {
        let empty = AppList::default();
        let selection = AppSelection::new("in-a", "A", &[], &empty);
        assert!(selection.candidates.is_empty());
        assert!(selection.hint().contains("no application"));
        let stub = AppList::parse(&json!({"apps": [], "stub": true, "note": "J2 stub"}));
        let selection = AppSelection::new("in-a", "A", &[], &stub);
        assert_eq!(selection.hint(), "J2 stub");
        assert!(selection.chosen().is_empty());
    }
}
