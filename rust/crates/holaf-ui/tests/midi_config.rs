//! The user's **existing** MIDI mappings must be picked up by the Rust UI.
//!
//! The report that started this work was "le MIDI j'en ai besoin, je contrôle le
//! volume avec un midimix" plus "j'aimerais que ça récupère la config actuelle
//! quand je lance": the mappings are already in `config.json`, written by the
//! Python UI, and re-learning eight faders and twenty-four buttons by hand would
//! make the port useless.
//!
//! So this file locks the whole chain, end to end and without a single line of
//! MIDI hardware:
//!
//! 1. [`the_python_config_is_loaded_with_its_mappings`] loads the **real**
//!    `config.json` of the repository through `AppState::from_config` (the exact
//!    startup path of the binary) and checks every strip's `midi_volume` /
//!    `midi_mute` / `midi_mono`, the `null` of the unmapped strip included;
//! 2. [`the_mapped_controls_move_the_right_strip`] plays the MIDI Mix: a
//!    `control_change` on CC 19 moves the "Mic" fader (`value / 127`), a
//!    `note_on` on note 3 toggles its MUTE — i.e. the mapping is not merely
//!    *loaded*, it *drives* the mixer;
//! 3. [`a_save_keeps_the_python_shape`] writes the config back and compares the
//!    three keys of every strip with the file it came from, so a round trip
//!    through the Rust UI leaves a file the Python UI reads identically;
//! 4. [`the_mappings_survive_a_volume_change`] does the same after the routine
//!    edit that triggers most saves (a fader move), because losing them there
//!    would be a silent data-loss bug;
//! 5. [`a_bare_number_mapping_is_understood_and_repaired`] covers the one shape
//!    the Python UI would *crash* on (`"midi_volume": 61`): it is read, and the
//!    save rewrites it as the object both UIs understand.
//!
//! The format itself (verified against `src/models/strip_model.py`,
//! `src/config/settings.py` and the Python `midi_engine.py`):
//!
//! ```json
//! "midi_volume": {"channel": 0, "control": 61, "type": "control_change"},
//! "midi_mute":   {"channel": 0, "note": 24,    "type": "note_on"},
//! "midi_mono":   {"channel": 0, "note": 22,    "type": "note_on"},
//! "midi_volume": null                                    // not mapped
//! ```
//!
//! Channel and identifiers are 0-based (the UI prints `ch.1` for channel 0), and
//! `Strip.to_dict` copies the three values verbatim, so the Python side accepts
//! exactly these objects — no more, no less.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use holaf_ui::app::AppState;
use holaf_ui::midi::{Binding, MidiMessage, MidiTarget};
use serde_json::Value;

/// The repository's real `config.json` (the one the user runs with).
fn repo_config() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config.json")
}

/// A private copy of it, so a test never writes the repository file.
fn copy_of_repo_config(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "holaf-ui-midi-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::copy(repo_config(), &path).unwrap();
    path
}

/// The three `midi_*` keys of every strip of a file, keyed by label (stable
/// across the uid repair a legacy config may need).
fn mappings_by_label(document: &Value) -> BTreeMap<String, [Value; 3]> {
    let mut map = BTreeMap::new();
    for strip in document["strips"].as_array().expect("strips[]") {
        let label = strip["label"].as_str().expect("label").to_string();
        map.insert(
            label,
            [
                strip.get("midi_volume").cloned().unwrap_or(Value::Null),
                strip.get("midi_mute").cloned().unwrap_or(Value::Null),
                strip.get("midi_mono").cloned().unwrap_or(Value::Null),
            ],
        );
    }
    map
}

/// The binding of `target` on the strip whose label is `label`.
fn binding_of(state: &AppState, label: &str, target: MidiTarget) -> Option<Binding> {
    let strip = state
        .strips
        .iter()
        .find(|strip| strip.label == label)
        .unwrap_or_else(|| panic!("no strip labelled {label}"));
    state
        .midi_bindings_for(&strip.uid)
        .iter()
        .find(|(bound, _)| *bound == target)
        .map(|(_, binding)| *binding)
}

#[test]
fn the_python_config_is_loaded_with_its_mappings() {
    let state = AppState::from_config(repo_config());
    assert_eq!(state.strips.len(), 7, "the shipped config has seven strips");

    // Every mapped strip, exactly as the Python UI wrote it (0-based channel).
    let expected: [(&str, u8, u8, u8); 6] = [
        ("Casque", 61, 24, 22),
        ("HP", 62, 26, 27),
        ("Mic", 19, 3, 1),
        ("Default", 23, 6, 4),
        ("Web", 27, 9, 7),
        ("Chat", 31, 12, 10),
    ];
    for (label, cc, mute, mono) in expected {
        assert_eq!(
            binding_of(&state, label, MidiTarget::Volume),
            Some(Binding::control(0, cc)),
            "{label}: midi_volume"
        );
        assert_eq!(
            binding_of(&state, label, MidiTarget::Mute),
            Some(Binding::note(0, mute)),
            "{label}: midi_mute"
        );
        assert_eq!(
            binding_of(&state, label, MidiTarget::Mono),
            Some(Binding::note(0, mono)),
            "{label}: midi_mono"
        );
    }
    // The virtual bus is unmapped (`null` in the file) — not an error, and above
    // all not a mapping on CC 0.
    assert!(
        state.midi_bindings_for("5f2928d1-499c-41b6-961d-27926b51a70c").is_empty(),
        "Mix Out has no mapping"
    );
    // …and none of the `null`s became a binding: 6 strips × 3 mappings.
    assert_eq!(state.midi_map.len(), 18);

    // The lookup table resolves the MIDI Mix controls to the right strips: the
    // eight faders of the controller are CC 19, 23, 27, 31, 35, 39, 43, 47 on
    // channel 1 (0 here), and the user's file maps the first six.
    for (cc, label) in [
        (19u8, "Mic"),
        (23, "Default"),
        (27, "Web"),
        (31, "Chat"),
        (61, "Casque"),
        (62, "HP"),
    ] {
        let resolved = state.midi_map.lookup(MidiMessage::Control {
            channel: 0,
            control: cc,
            value: 0,
        });
        let uid = state
            .strips
            .iter()
            .find(|strip| strip.label == label)
            .map(|strip| strip.uid.as_str())
            .unwrap();
        assert_eq!(
            resolved,
            Some((uid, MidiTarget::Volume)),
            "CC {cc} must drive {label}"
        );
    }
}

#[test]
fn the_mapped_controls_move_the_right_strip() {
    let state = AppState::from_config(repo_config());
    let mut state = state;
    let now = std::time::Instant::now();
    state.set_clock(now);

    // A fader: CC 19 (the MIDI Mix's first fader) is "Mic" volume.
    state.handle_midi_message(
        MidiMessage::Control {
            channel: 0,
            control: 19,
            value: 64,
        },
        now,
    );
    let mic = state
        .strips
        .iter()
        .find(|strip| strip.label == "Mic")
        .unwrap()
        .volume;
    assert!((mic - 64.0 / 127.0).abs() < 1e-6, "volume = {mic}");
    // The endpoints are honoured (a fader all the way down / up).  Each message
    // is spaced by more than the 10 ms throttle floor, like a real fader sweep.
    for (step, (value, expected)) in [(0u8, 0.0f32), (127, 1.0)].into_iter().enumerate() {
        state.handle_midi_message(
            MidiMessage::Control {
                channel: 0,
                control: 19,
                value,
            },
            now + std::time::Duration::from_millis(50 + step as u64 * 20),
        );
        let volume = state
            .strips
            .iter()
            .find(|strip| strip.label == "Mic")
            .unwrap()
            .volume;
        assert!((volume - expected).abs() < 1e-6, "{value} -> {volume}");
    }

    // A button: note 3 is "Mic" MUTE, toggled on press only.
    let before = state
        .strips
        .iter()
        .find(|strip| strip.label == "Mic")
        .unwrap()
        .mute;
    state.handle_midi_message(
        MidiMessage::Note {
            channel: 0,
            note: 3,
            velocity: 127,
        },
        now + std::time::Duration::from_millis(100),
    );
    let muted = state
        .strips
        .iter()
        .find(|strip| strip.label == "Mic")
        .unwrap()
        .mute;
    assert_eq!(muted, !before);
    // The release must not toggle back (it would double every press on hardware
    // that sends both events).
    state.handle_midi_message(
        MidiMessage::Note {
            channel: 0,
            note: 3,
            velocity: 0,
        },
        now + std::time::Duration::from_millis(150),
    );
    assert_eq!(
        state
            .strips
            .iter()
            .find(|strip| strip.label == "Mic")
            .unwrap()
            .mute,
        muted
    );
    // The LED of that button follows the strip.
    let led = state.take_midi_feedback();
    assert!(
        led.contains(&MidiMessage::Note {
            channel: 0,
            note: 3,
            velocity: if muted { 127 } else { 0 }
        }),
        "{led:?}"
    );
    // An unmapped control (CC 35, the seventh fader) moves nothing.
    let before: Vec<f32> = state.strips.iter().map(|strip| strip.volume).collect();
    state.handle_midi_message(
        MidiMessage::Control {
            channel: 0,
            control: 35,
            value: 127,
        },
        now + std::time::Duration::from_millis(200),
    );
    let after: Vec<f32> = state.strips.iter().map(|strip| strip.volume).collect();
    assert_eq!(before, after, "CC 35 is not mapped in this config");
}

#[test]
fn a_save_keeps_the_python_shape() {
    let path = copy_of_repo_config("roundtrip");
    let original = holaf_core::config::load_config(&path).unwrap();
    let mut state = AppState::from_config(&path);
    let now = state.clock();
    state.save_pending(now + holaf_ui::config::SAVE_DEBOUNCE);

    let saved = holaf_core::config::load_config(&path).unwrap();
    assert_eq!(
        mappings_by_label(&saved),
        mappings_by_label(&original),
        "the three midi_* keys of every strip are written back unchanged"
    );
    // Byte-for-byte on one entry: the exact object shape `Strip.to_dict` writes.
    let mic = saved["strips"]
        .as_array()
        .unwrap()
        .iter()
        .find(|strip| strip["label"] == "Mic")
        .unwrap();
    assert_eq!(
        mic["midi_volume"],
        serde_json::json!({"channel": 0, "control": 19, "type": "control_change"})
    );
    assert_eq!(
        mic["midi_mute"],
        serde_json::json!({"channel": 0, "note": 3, "type": "note_on"})
    );
    // The unmapped strip keeps an explicit `null` (the Python shape), it is not
    // dropped: `Strip.from_dict` reads `.get(...)` and would restore `None`
    // either way, but a key that disappears is a diff the user notices.
    let mix = saved["strips"]
        .as_array()
        .unwrap()
        .iter()
        .find(|strip| strip["label"] == "Mix Out")
        .unwrap();
    assert_eq!(mix["midi_volume"], Value::Null);
    assert_eq!(mix["midi_mute"], Value::Null);
    assert_eq!(mix["midi_mono"], Value::Null);
    // Nothing else was disturbed (the Qt geometry blob, the modes).
    assert!(saved.get("window_geometry").is_some());
    assert_eq!(mic["mode"], "physical");
    state.strips.clear();
    state.config.as_mut().unwrap().cancel_save();
}

#[test]
fn the_mappings_survive_a_volume_change() {
    let path = copy_of_repo_config("volume-save");
    let original = holaf_core::config::load_config(&path).unwrap();
    let mut state = AppState::from_config(&path);
    let now = state.clock();
    let uid = state
        .strips
        .iter()
        .find(|strip| strip.label == "Web")
        .map(|strip| strip.uid.clone())
        .unwrap();
    // The routine edit: a fader move (what triggers most saves).
    state.set_clock(now);
    state.apply(holaf_ui::app::UiAction::Volume {
        uid: uid.clone(),
        volume: 0.42,
    });
    state.save_pending(now + holaf_ui::config::SAVE_DEBOUNCE);
    let saved = holaf_core::config::load_config(&path).unwrap();
    assert_eq!(mappings_by_label(&saved), mappings_by_label(&original));
    // The volume itself *did* change (so the save really happened).
    let web = saved["strips"]
        .as_array()
        .unwrap()
        .iter()
        .find(|strip| strip["label"] == "Web")
        .unwrap();
    assert!((web["volume"].as_f64().unwrap() - 0.42).abs() < 1e-6);
    state.strips.clear();
    state.config.as_mut().unwrap().cancel_save();
}

#[test]
fn a_bare_number_mapping_is_understood_and_repaired() {
    let dir = std::env::temp_dir().join(format!(
        "holaf-ui-midi-legacy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    // The shape the Python UI would crash on (`TypeError: 'int' object is not
    // subscriptable` in `_rebuild_midi_lookup`) — a hand-edited file.
    std::fs::write(
        &path,
        serde_json::json!({
            "strips": [{
                "uid": "in-mic",
                "label": "Mic",
                "kind": "input",
                "volume": 1.0,
                "mute": false,
                "is_mono": false,
                "routes": [],
                "device_name": null,
                "assigned_apps": [],
                "is_default": false,
                "midi_volume": 19,
                "midi_mute": 3,
                "midi_mono": null
            }]
        })
        .to_string(),
    )
    .unwrap();

    let mut state = AppState::from_config(&path);
    assert_eq!(
        binding_of(&state, "Mic", MidiTarget::Volume),
        Some(Binding::control(0, 19)),
        "a bare CC number is read as channel 0 + control_change"
    );
    assert_eq!(
        binding_of(&state, "Mic", MidiTarget::Mute),
        Some(Binding::note(0, 3)),
        "a bare number for a button is a note"
    );
    assert_eq!(binding_of(&state, "Mic", MidiTarget::Mono), None);
    // The mapping works…
    let now = state.clock();
    state.set_clock(now);
    state.handle_midi_message(
        MidiMessage::Control {
            channel: 0,
            control: 19,
            value: 32,
        },
        now,
    );
    let volume = state
        .strips
        .iter()
        .find(|strip| strip.label == "Mic")
        .unwrap()
        .volume;
    assert!((volume - 32.0 / 127.0).abs() < 1e-6, "{volume}");
    // …and the first save repairs the file into the object shape Python reads.
    state.save_pending(now + holaf_ui::config::SAVE_DEBOUNCE);
    let saved = holaf_core::config::load_config(&path).unwrap();
    assert_eq!(
        saved["strips"][0]["midi_volume"],
        serde_json::json!({"type": "control_change", "channel": 0, "control": 19})
    );
    assert_eq!(
        saved["strips"][0]["midi_mute"],
        serde_json::json!({"type": "note_on", "channel": 0, "note": 3})
    );
    assert_eq!(saved["strips"][0]["midi_mono"], Value::Null);
    state.strips.clear();
    state.config.as_mut().unwrap().cancel_save();
}

/// The port is **not** stored by the Python UI: `_init_midi` re-picked a port at
/// every start (first name containing `mix`, else the first port), so there is no
/// key to be compatible with.  The Rust UI adds `ui_rust.midi_port` — its own
/// namespace, which Python's `load_config`/`save_config` copy through untouched
/// because they rewrite only `strips[]` and `window_geometry`.
#[test]
fn the_port_preference_has_its_own_key_and_python_keeps_it() {
    let path = copy_of_repo_config("port-key");
    let mut state = AppState::from_config(&path);
    let now = state.clock();
    assert_eq!(state.midi_port_request, None, "the Python file has no port");
    assert!(state.midi_port_auto, "so the automatic pick is allowed");

    state.apply(holaf_ui::app::UiAction::MidiPortSelected {
        name: Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string()),
    });
    state.save_pending(now + holaf_ui::config::SAVE_DEBOUNCE);
    let saved = holaf_core::config::load_config(&path).unwrap();
    assert_eq!(
        saved["ui_rust"]["midi_port"], "MIDI Mix:MIDI Mix MIDI 1 24:0",
        "a Rust-only namespace, where Python's own keys never appear"
    );
    assert!(
        saved.get("midi_port").is_none(),
        "never at the top level: that would collide with a future Python key"
    );
    // Reloading keeps the choice and disables the automatic pick.
    let mut reloaded = AppState::from_config(&path);
    assert_eq!(
        reloaded.midi_port_request.as_deref(),
        Some("MIDI Mix:MIDI Mix MIDI 1 24:0")
    );
    assert!(!reloaded.midi_port_auto);
    reloaded.strips.clear();
    reloaded.config.as_mut().unwrap().cancel_save();
    state.strips.clear();
    state.config.as_mut().unwrap().cancel_save();
}
