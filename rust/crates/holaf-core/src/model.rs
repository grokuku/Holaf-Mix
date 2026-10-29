//! Core data model: [`EffectKey`], [`StripType`], [`Strip`], [`EffectState`].
//!
//! JSON keys are `snake_case` and compatible with the current Python
//! `config.json` (see `src/models/strip_model.py`).  Unknown fields in an
//! incoming JSON document are ignored by serde (the struct does not model the
//! UI-only fields `mode`, `assigned_apps` or the MIDI mappings yet — they are
//! carried over untouched on load and dropped on save, as documented in the
//! rewrite spec §2).

use std::fmt;
use std::str::FromStr;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::fx;

/// The five LADSPA effects, in canonical chain order.
///
/// Chain order (Python `FX_PLUGIN_MAP`): gate → noise_cancel → eq → tube →
/// compressor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKey {
    Gate,
    NoiseCancel,
    Eq,
    Tube,
    Compressor,
}

impl EffectKey {
    /// All five effect keys in canonical chain order.
    pub fn all() -> [EffectKey; 5] {
        [
            EffectKey::Gate,
            EffectKey::NoiseCancel,
            EffectKey::Eq,
            EffectKey::Tube,
            EffectKey::Compressor,
        ]
    }

    /// Effects that apply to a given strip kind.
    ///
    /// Input strips process gate + noise_cancel + eq + tube + compressor (5).
    /// Output strips only get the tonal/mastering chain: eq + tube +
    /// compressor (3).  Mirrors `DEFAULT_EFFECT_PARAMS` usage and the
    /// `audio_engine` graph builders.
    pub fn for_strip(kind: StripType) -> Vec<EffectKey> {
        match kind {
            StripType::Input => Self::all().to_vec(),
            StripType::Output => vec![EffectKey::Eq, EffectKey::Tube, EffectKey::Compressor],
        }
    }

    /// Lowercase snake_case string used by config.json keys, e.g. `"gate"`,
    /// `"noise_cancel"`.
    pub fn as_str(self) -> &'static str {
        match self {
            EffectKey::Gate => "gate",
            EffectKey::NoiseCancel => "noise_cancel",
            EffectKey::Eq => "eq",
            EffectKey::Tube => "tube",
            EffectKey::Compressor => "compressor",
        }
    }
}

impl fmt::Display for EffectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EffectKey {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "gate" => Ok(EffectKey::Gate),
            "noise_cancel" => Ok(EffectKey::NoiseCancel),
            "eq" => Ok(EffectKey::Eq),
            "tube" => Ok(EffectKey::Tube),
            "compressor" => Ok(EffectKey::Compressor),
            _ => Err(()),
        }
    }
}

/// Whether a strip captures from a source (input) or plays to a device
/// (output).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StripType {
    Input,
    Output,
}

impl StripType {
    pub fn as_str(self) -> &'static str {
        match self {
            StripType::Input => "input",
            StripType::Output => "output",
        }
    }
}

impl fmt::Display for StripType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-effect runtime state: on/off switch plus the current control ports.
///
/// `params` preserves insertion order (an `IndexMap`) because the LADSPA port
/// order is meaningful: the audio engine serialises it to SPA-JSON and
/// golden-tests compare the exact ordering.  A `HashMap` would silently break
/// that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectState {
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub params: IndexMap<String, f32>,
}

impl EffectState {
    /// An inactive effect holding the plugin's default port values.
    pub fn inactive_with_defaults(key: EffectKey) -> Self {
        EffectState {
            active: false,
            params: fx::default_params(key),
        }
    }
}

/// One audio strip (input = microphone/app source, output = speakers/device).
///
/// Field semantics and JSON names mirror `Strip` from `strip_model.py`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Strip {
    /// Persistent unique id across restarts.
    pub uid: String,
    /// User-facing label, e.g. "Discord", "Micro", "Speakers".
    pub label: String,
    pub kind: StripType,
    /// 0.0–1.0 nominal (may go above 1.0).  Default 1.0 (unchanged).
    #[serde(default = "default_one")]
    pub volume: f32,
    #[serde(default)]
    pub mute: bool,
    /// Downmix stereo to mono.
    #[serde(default)]
    pub is_mono: bool,
    /// Routing matrix: output strip UIDs this input sends audio to.
    #[serde(default)]
    pub routes: Vec<String>,
    /// System default sink flag (input strips).
    #[serde(default)]
    pub is_default: bool,
    /// Hardware/PipeWire connection detail for physical strips.
    #[serde(default)]
    pub device_name: Option<String>,
    /// All five effects, always populated (uniform with Python `to_dict`).
    #[serde(default)]
    pub effects: IndexMap<EffectKey, EffectState>,
}

fn default_one() -> f32 {
    1.0
}

impl Strip {
    /// New strip with an empty `uid` (callers that persist a strip must set a
    /// real id).  The effects dict is populated for **all** five effects as
    /// inactive with default params — uniform with Python `Strip.__init__`.
    pub fn new(label: impl Into<String>, kind: StripType) -> Self {
        Self::with_uid("", label, kind)
    }

    pub fn with_uid(uid: impl Into<String>, label: impl Into<String>, kind: StripType) -> Self {
        Strip {
            uid: uid.into(),
            label: label.into(),
            kind,
            volume: 1.0,
            mute: false,
            is_mono: false,
            routes: Vec::new(),
            is_default: false,
            device_name: None,
            effects: default_effects(),
        }
    }

    pub fn is_input(&self) -> bool {
        self.kind == StripType::Input
    }

    pub fn is_output(&self) -> bool {
        self.kind == StripType::Output
    }
}

impl Default for Strip {
    fn default() -> Self {
        Strip::new("", StripType::Input)
    }
}

/// A fresh effects map: all five keys, each inactive with the plugin's default
/// control values (like Python's `copy.deepcopy(DEFAULT_EFFECT_PARAMS[..])`).
pub fn default_effects() -> IndexMap<EffectKey, EffectState> {
    EffectKey::all()
        .into_iter()
        .map(|k| (k, EffectState::inactive_with_defaults(k)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_key_snake_serde_names() {
        assert_eq!(EffectKey::Gate.as_str(), "gate");
        assert_eq!(EffectKey::NoiseCancel.as_str(), "noise_cancel");
        assert_eq!(EffectKey::Eq.as_str(), "eq");
        assert_eq!(EffectKey::Tube.as_str(), "tube");
        assert_eq!(EffectKey::Compressor.as_str(), "compressor");

        // serde rename_all = "snake_case" must agree with as_str().
        for k in EffectKey::all() {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()), "serde name mismatch");
            assert_eq!(
                serde_json::from_str::<EffectKey>(&json).unwrap(),
                k,
                "serde parse mismatch"
            );
        }
    }

    #[test]
    fn effect_key_parse_and_display() {
        for k in EffectKey::all() {
            assert_eq!(k.to_string(), k.as_str());
            assert_eq!(k.to_string().parse::<EffectKey>().unwrap(), k);
        }
        assert!("bogus".parse::<EffectKey>().is_err());
        assert_eq!(
            EffectKey::from_str("noise_cancel"),
            Ok(EffectKey::NoiseCancel)
        );
    }

    #[test]
    fn effect_key_all_has_canonical_order() {
        let all = EffectKey::all();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0], EffectKey::Gate);
        assert_eq!(all[1], EffectKey::NoiseCancel);
        assert_eq!(all[2], EffectKey::Eq);
        assert_eq!(all[3], EffectKey::Tube);
        assert_eq!(all[4], EffectKey::Compressor);
    }

    #[test]
    fn for_strip_input_has_five_output_three() {
        let input = EffectKey::for_strip(StripType::Input);
        assert_eq!(input.len(), 5);
        assert_eq!(input, EffectKey::all().to_vec());

        let output = EffectKey::for_strip(StripType::Output);
        assert_eq!(
            output,
            vec![EffectKey::Eq, EffectKey::Tube, EffectKey::Compressor]
        );
    }

    #[test]
    fn strip_type_serde_and_display() {
        assert_eq!(
            serde_json::to_string(&StripType::Input).unwrap(),
            "\"input\""
        );
        assert_eq!(
            serde_json::to_string(&StripType::Output).unwrap(),
            "\"output\""
        );
        assert_eq!(
            serde_json::from_str::<StripType>("\"input\"").unwrap(),
            StripType::Input
        );
        assert_eq!(StripType::Input.to_string(), "input");
        assert_eq!(StripType::Output.to_string(), "output");
    }

    #[test]
    fn fresh_strip_matches_python_defaults() {
        let strip = Strip::new("Micro", StripType::Input);
        assert_eq!(strip.uid, "");
        assert_eq!(strip.label, "Micro");
        assert_eq!(strip.kind, StripType::Input);
        assert_eq!(strip.volume, 1.0);
        assert!(!strip.mute);
        assert!(!strip.is_mono);
        assert!(strip.routes.is_empty());
        assert!(!strip.is_default);
        assert_eq!(strip.device_name, None);
        assert_eq!(strip.effects.len(), 5);

        // Every effect is present, inactive and carries its default ports.
        for (k, state) in &strip.effects {
            assert!(!state.active);
            assert!(!state.params.is_empty());
            assert_eq!(state.params.len(), crate::fx::default_params(*k).len());
        }
    }

    #[test]
    fn serde_round_trip_complete_strip() {
        let mut strip = Strip::with_uid("uid-1", "Casque", StripType::Output);
        strip.volume = 0.75;
        strip.mute = true;
        strip.is_mono = false;
        strip.routes = vec!["out-a".into(), "out-b".into()];
        strip.is_default = false;
        strip.device_name = Some("alsa_output.usb-0.analog-stereo".into());
        strip
            .effects
            .get_mut(&EffectKey::Eq)
            .unwrap()
            .params
            .insert("100Hz gain".into(), -3.5);
        strip
            .effects
            .get_mut(&EffectKey::Compressor)
            .unwrap()
            .active = true;

        let json = serde_json::to_string(&strip).unwrap();
        let back: Strip = serde_json::from_str(&json).unwrap();
        assert_eq!(back, strip);

        // Key/value structure sanity on the raw JSON.
        let obj = serde_json::from_str::<serde_json::Value>(&json).unwrap();
        assert_eq!(obj["uid"], "uid-1");
        assert_eq!(obj["kind"], "output");
        assert_eq!(obj["volume"], 0.75);
        assert_eq!(obj["effects"]["compressor"]["active"], true);
        assert_eq!(obj["effects"]["eq"]["params"]["100Hz gain"], -3.5);
    }

    #[test]
    fn serde_defaults_when_fields_missing() {
        // Only the required fields present: everything else must fall back to
        // the Python defaults (volume 1.0, no effects, etc.).
        let json = r#"{"uid":"u","label":"L","kind":"output"}"#;
        let strip: Strip = serde_json::from_str(json).unwrap();
        assert_eq!(strip.volume, 1.0);
        assert!(!strip.mute);
        assert!(!strip.is_mono);
        assert!(strip.routes.is_empty());
        assert_eq!(strip.device_name, None);
        assert!(strip.effects.is_empty());
    }

    #[test]
    fn serde_ignores_unknown_fields() {
        // The current config.json carries UI-only fields (mode, assigned_apps,
        // midi_*) not yet modelled in J0 — they must not break deserialization.
        let json = r#"{
            "uid":"u","label":"L","kind":"input","volume":0.5,
            "mode":"virtual","assigned_apps":["a"],
            "midi_volume":{"channel":0}
        }"#;
        let strip: Strip = serde_json::from_str(json).unwrap();
        assert_eq!(strip.volume, 0.5);
        assert_eq!(strip.kind, StripType::Input);
    }

    #[test]
    fn default_effects_covers_all_keys() {
        let effects = default_effects();
        assert_eq!(effects.len(), 5);
        for k in EffectKey::all() {
            assert!(effects.contains_key(&k));
        }
    }
}
