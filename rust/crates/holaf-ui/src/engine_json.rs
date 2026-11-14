//! Parsers for the backend's JSON payloads the UI consumes.
//!
//! `holaf-core::backend::Backend` speaks JSON (`serde_json::Value`) so that the
//! daemon's IPC and the in-process PipeWire backend share one contract.  The UI
//! needs a handful of those payloads — the meter source map, the device list,
//! the application list and the full `status` — and all are pure functions of a
//! `Value`, so they are tested without a daemon:
//!
//! ```text
//! meter_sources() → {"running": true, "sources": {"<uid>": "<source name>"}}
//! device_list()   → {"devices": [{"id": 42, "name": "…", "media_class": "Audio/Sink",
//!                               "description": "Built-in Audio Analog Stereo"}]}
//! app_list()      → {"apps": [{"id": "…", "name": "firefox", "pid": 42, "sink": "…"}]}
//! ```
//!
//! All are tolerant on purpose: an unknown/renamed field yields an empty result
//! (the VU stays dark, the device combo only offers "Virtual Bus") rather than
//! an error the UI would have to surface as a failure.
//!
//! **`description` is optional** — the current `holaf-pw` payload only carries
//! `id`/`name`/`media_class` (the Python UI reads it from `pw-dump`), so the
//! field is parsed when present and `None` otherwise.  [`crate::devices`] relies
//! on it when available and falls back to node-name matching when it is not.

use std::collections::HashMap;

use serde_json::Value;

/// One physical audio device known to the engine (`Backend::device_list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// PipeWire global id (informational; the UI rebinds by **name**).
    pub id: u32,
    /// `node.name` — what `Strip::device_name` stores.
    pub name: String,
    /// `Audio/Sink` (an output device) or `Audio/Source` (an input device).
    pub media_class: String,
    /// `node.description` (or `node.nick`) when the engine publishes it.
    ///
    /// This is the *stable* half of a device identity: `node.name` embeds the
    /// card/instance id (`alsa_output.pci-0000_00_1f.3.analog-stereo`), which
    /// changes when the device is re-plugged or the machine reboots, while the
    /// description ("Built-in Audio Analog Stereo") does not.  `None` when the
    /// payload has no description — matching then falls back to names, see
    /// [`crate::devices`].
    pub description: Option<String>,
}

impl DeviceInfo {
    pub fn is_sink(&self) -> bool {
        self.media_class == "Audio/Sink"
    }

    pub fn is_source(&self) -> bool {
        self.media_class == "Audio/Source"
    }

    /// The short label the combo shows: the description when the engine
    /// publishes one (that is what the Qt combo displayed), else the shortened
    /// node name.
    pub fn display(&self) -> String {
        match &self.description {
            Some(description) if !description.trim().is_empty() => {
                let description = description.trim();
                if description.chars().count() <= 15 {
                    description.to_string()
                } else {
                    let mut short: String = description.chars().take(14).collect();
                    short.push('…');
                    short
                }
            }
            _ => crate::views::strip::device_display_name(&self.name),
        }
    }

    /// What the device picker lists next to the label.
    pub fn detail(&self) -> &str {
        &self.name
    }
}

/// `{uid → source name}` from a `meter_sources()` payload.
///
/// An engine that is not running reports `running: false` and no sources: the
/// UI then shows dark VUs (the Python `get_levels` behaves the same way when its
/// streams are gone).
pub fn meter_sources(payload: &Value) -> HashMap<String, String> {
    if payload
        .get("running")
        .and_then(Value::as_bool)
        .is_some_and(|running| !running)
    {
        return HashMap::new();
    }
    let Some(sources) = payload.get("sources").and_then(Value::as_object) else {
        return HashMap::new();
    };
    sources
        .iter()
        .filter_map(|(uid, value)| {
            let name = value.as_str()?.trim();
            if uid.is_empty() || name.is_empty() {
                return None;
            }
            Some((uid.clone(), name.to_string()))
        })
        .collect()
}

/// Every device of a `device_list()` payload, sorted like the Qt combo
/// (`nodes.sort(key=description.lower())`; we only have `node.name`).
pub fn devices(payload: &Value) -> Vec<DeviceInfo> {
    let Some(list) = payload.get("devices").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut devices: Vec<DeviceInfo> = list
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name").and_then(Value::as_str)?.trim();
            let media_class = entry.get("media_class").and_then(Value::as_str)?.trim();
            if name.is_empty() || !(media_class == "Audio/Sink" || media_class == "Audio/Source") {
                return None;
            }
            Some(DeviceInfo {
                id: entry.get("id").and_then(Value::as_u64).unwrap_or(0) as u32,
                name: name.to_string(),
                media_class: media_class.to_string(),
                description: entry
                    .get("description")
                    .or_else(|| entry.get("nick"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                    .map(str::to_string),
            })
        })
        .collect();
    devices.sort_by_key(|a| a.name.to_lowercase());
    devices.dedup_by(|a, b| a.name == b.name);
    devices
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn meter_sources_are_parsed_and_filtered() {
        let payload = json!({
            "running": true,
            "sources": {
                "in-mic": "Holaf_Bus_Mic.monitor",
                "out-hp": "alsa_output.usb.analog-stereo.monitor",
                "ghost": "",
                "": "whatever.monitor"
            }
        });
        let sources = meter_sources(&payload);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources["in-mic"], "Holaf_Bus_Mic.monitor");
        assert_eq!(sources["out-hp"], "alsa_output.usb.analog-stereo.monitor");
    }

    #[test]
    fn a_stopped_engine_reports_no_source() {
        assert!(meter_sources(&json!({"running": false, "sources": {"a": "b"}})).is_empty());
        assert!(meter_sources(&json!({})).is_empty());
        assert!(meter_sources(&json!(null)).is_empty());
        assert!(meter_sources(&json!({"sources": []})).is_empty());
    }

    #[test]
    fn devices_are_sorted_filtered_and_parsed() {
        let payload = json!({
            "devices": [
                {"id": 7, "name": "bluez_output.AA.a2dp-sink", "media_class": "Audio/Sink"},
                {"id": 3, "name": "alsa_output.pci.analog-stereo", "media_class": "Audio/Sink"},
                {"id": 9, "name": "Midi Bridge", "media_class": "Midi/Bridge"},
                {"id": 4, "name": "alsa_input.pci.analog-stereo", "media_class": "Audio/Source"},
                {"id": 5, "name": "alsa_output.pci.analog-stereo", "media_class": "Audio/Sink"}
            ]
        });
        let devices = devices(&payload);
        assert_eq!(devices.len(), 3, "duplicate name + MIDI dropped");
        assert_eq!(
            devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            [
                "alsa_input.pci.analog-stereo",
                "alsa_output.pci.analog-stereo",
                "bluez_output.AA.a2dp-sink"
            ]
        );
        assert_eq!(devices[0].id, 4);
        assert!(devices[0].is_source() && !devices[0].is_sink());
        assert_eq!(devices[2].display(), "a2dp-sink");
    }

    #[test]
    fn a_broken_device_payload_is_empty_not_an_error() {
        assert!(devices(&json!(null)).is_empty());
        assert!(devices(&json!({"devices": "nope"})).is_empty());
        assert!(devices(&json!({"devices": [{"id": 1}]})).is_empty());
    }

    #[test]
    fn a_description_is_optional_and_preferred_for_display() {
        let payload = json!({
            "devices": [
                {"id": 1, "name": "alsa_output.pci.analog-stereo",
                 "media_class": "Audio/Sink", "description": "Built-in Audio Analog Stereo"},
                {"id": 2, "name": "alsa_output.usb.analog-stereo",
                 "media_class": "Audio/Sink", "description": "  "},
                {"id": 3, "name": "alsa_input.pci.analog-stereo",
                 "media_class": "Audio/Source", "nick": "Internal Mic"}
            ]
        });
        let devices = devices(&payload);
        assert_eq!(devices.len(), 3);
        assert_eq!(
            devices[1].description.as_deref(),
            Some("Built-in Audio Analog Stereo")
        );
        // A blank description is the same as none.
        assert_eq!(devices[2].description, None);
        // `nick` is accepted as a description.
        assert_eq!(devices[0].description.as_deref(), Some("Internal Mic"));
        // Display prefers the description (this is what the Qt combo showed),
        // and elides it past 15 chars.
        assert_eq!(devices[1].display(), "Built-in Audio…");
        assert_eq!(devices[0].display(), "Internal Mic");
        // Without a description the node name rule applies.
        assert_eq!(devices[2].display(), "analog-stereo");
    }
}
