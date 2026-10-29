//! `from_dict` migrations, ported from `Strip.from_dict` in
//! `src/models/strip_model.py`.
//!
//! Two kinds of legacy data are normalised:
//!
//! - **effects declared as booleans** (old config: `"gate": true`) become
//!   `{ active: <bool>, params: <defaults> }`;
//! - **stale/short control-port names** are renamed to the exact LADSPA port
//!   names (EQ short bands, compressor/gate/tube old names, rnnoise `Model`).
//!
//! Order is critical: renames run **before** missing defaults are filled,
//! otherwise the freshly-added default keys would satisfy the
//! `new_key not in params` guard and the user's old values would be silently
//! lost.  Unknown keys are never purged (Python behaviour).

use indexmap::IndexMap;
use serde_json::Value;

use crate::fx;
use crate::model::{EffectKey, EffectState, Strip, StripType};

/// Old short EQ band names → exact mbeq_1197 control-port names.
const EQ_RENAMES: &[(&str, &str)] = &[
    ("50Hz", "50Hz gain (low shelving)"),
    ("100Hz", "100Hz gain"),
    ("156Hz", "156Hz gain"),
    ("220Hz", "220Hz gain"),
    ("311Hz", "311Hz gain"),
    ("440Hz", "440Hz gain"),
    ("622Hz", "622Hz gain"),
    ("880Hz", "880Hz gain"),
    ("1250Hz", "1250Hz gain"),
    ("1750Hz", "1750Hz gain"),
    ("2500Hz", "2500Hz gain"),
    ("3500Hz", "3500Hz gain"),
    ("5000Hz", "5000Hz gain"),
    ("10000Hz", "10000Hz gain"),
    ("20000Hz", "20000Hz gain"),
];

/// Old short compressor names → exact sc4_1882 control-port names.
const COMPRESSOR_RENAMES: &[(&str, &str)] = &[
    ("Threshold (dB)", "Threshold level (dB)"),
    ("Attack (ms)", "Attack time (ms)"),
    ("Release (ms)", "Release time (ms)"),
    ("Makeup Gain (dB)", "Makeup gain (dB)"),
];

/// Old gate name — gate_1410 exposes `Decay (ms)`, not `Release (ms)`.
const GATE_RENAMES: &[(&str, &str)] = &[("Release (ms)", "Decay (ms)")];

/// Old tube plugin (valve_1209) names → new TAP TubeWarmth control ports.
const TUBE_RENAMES: &[(&str, &str)] = &[
    ("Distortion level", "Drive"),
    ("Distortion character", "Tape--Tube Blend"),
];

/// Migrate one effect's parameter dict in place: apply renames first, then
/// fill any missing known port with its default value.  Unknown keys are
/// left untouched.
pub fn migrate_effect_params(key: EffectKey, params: &mut IndexMap<String, f32>) {
    // Renames must run before default-filling (see module docs).
    let renames: &[(&str, &str)] = match key {
        EffectKey::Eq => EQ_RENAMES,
        EffectKey::Compressor => COMPRESSOR_RENAMES,
        EffectKey::Gate => GATE_RENAMES,
        EffectKey::Tube => TUBE_RENAMES,
        EffectKey::NoiseCancel => &[],
    };
    for (old, new) in renames {
        // Guard mirrors Python: only migrate when the old key exists AND the
        // new key isn't already present (never overwrite an up-to-date value).
        if params.contains_key(*old) && !params.contains_key(*new) {
            if let Some(value) = params.shift_remove(*old) {
                params.insert((*new).to_string(), value);
            }
        }
    }

    // rnnoise legacy "Model" placeholder → drop (real ports get their
    // defaults below).
    if key == EffectKey::NoiseCancel {
        params.shift_remove("Model");
    }

    // Fill missing defaults (new controls, migration gaps, old boolean format
    // fallback).  Missing keys are appended in canonical port order.
    for (port, default) in fx::default_params(key) {
        params.entry(port).or_insert(default);
    }
}

/// Normalise one `effects` entry (any legacy shape) into an [`EffectState`].
///
/// - missing / not-an-object  → inactive with default params;
/// - boolean (old format)     → active iff true, with default params;
/// - dict (new format)        → `active` + `params` migrated and
///   default-filled.
pub fn effect_state_from_value(key: EffectKey, value: Option<&Value>) -> EffectState {
    match value {
        None => EffectState::inactive_with_defaults(key),
        Some(Value::Bool(active)) => EffectState {
            active: *active,
            params: fx::default_params(key),
        },
        Some(Value::Object(obj)) => {
            let active = obj.get("active").and_then(Value::as_bool).unwrap_or(false);
            let mut params: IndexMap<String, f32> = obj
                .get("params")
                .and_then(Value::as_object)
                .map(|p| {
                    p.iter()
                        .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f as f32)))
                        .collect()
                })
                .unwrap_or_default();
            migrate_effect_params(key, &mut params);
            EffectState { active, params }
        }
        // Any other JSON shape (string, array, …) falls back like Python.
        Some(_) => EffectState::inactive_with_defaults(key),
    }
}

/// Recreate a [`Strip`] from a decoded `config.json` entry, applying all
/// effect migrations.
///
/// Port of `Strip.from_dict`.  All five effects are always present after this
/// call, even if the source omitted some (they default to inactive + plugin
/// defaults).  `data` must be a JSON object; otherwise a default (input)
/// strip is returned.
pub fn strip_from_dict(data: &Value) -> Strip {
    let Some(obj) = data.as_object() else {
        return Strip::default();
    };

    let label = obj
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let kind = match obj.get("kind").and_then(Value::as_str) {
        Some("input") => StripType::Input,
        _ => StripType::Output,
    };
    let uid = obj
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let volume = obj.get("volume").and_then(Value::as_f64).unwrap_or(1.0) as f32;
    let mute = obj.get("mute").and_then(Value::as_bool).unwrap_or(false);
    let is_mono = obj.get("is_mono").and_then(Value::as_bool).unwrap_or(false);
    let routes: Vec<String> = obj
        .get("routes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let is_default = obj
        .get("is_default")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let device_name = obj
        .get("device_name")
        .and_then(Value::as_str)
        .map(str::to_string);

    let effects_object = obj.get("effects").and_then(Value::as_object);
    let mut effects = IndexMap::with_capacity(EffectKey::all().len());
    for key in EffectKey::all() {
        let raw = effects_object.and_then(|eo| eo.get(key.as_str()));
        effects.insert(key, effect_state_from_value(key, raw));
    }

    Strip {
        uid,
        label,
        kind,
        volume,
        mute,
        is_mono,
        routes,
        is_default,
        device_name,
        effects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::default_params;
    use crate::model::EffectKey::*;

    fn map(pairs: &[(&str, f32)]) -> IndexMap<String, f32> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn gate_release_becomes_decay_and_defaults_filled() {
        let mut p = map(&[
            ("Threshold (dB)", -30.0),
            ("Attack (ms)", 5.0),
            ("Release (ms)", 200.0), // legacy key
            ("Hold (ms)", 50.0),
        ]);
        migrate_effect_params(Gate, &mut p);

        assert!(!p.contains_key("Release (ms)"));
        assert_eq!(p["Decay (ms)"], 200.0); // user value preserved through rename
        assert_eq!(p["Threshold (dB)"], -30.0);
        assert_eq!(p["Attack (ms)"], 5.0);
        assert_eq!(p["Hold (ms)"], 50.0);
        // Missing defaults filled.
        assert_eq!(p["LF key filter (Hz)"], 33.6);
        assert_eq!(p["HF key filter (Hz)"], 23520.0);
        assert_eq!(p["Range (dB)"], -90.0);
        assert_eq!(p.len(), 7);

        // Idempotence: a second pass is a no-op.
        let before = p.clone();
        migrate_effect_params(Gate, &mut p);
        assert_eq!(p, before);
    }

    #[test]
    fn eq_short_bands_renamed_and_defaults_filled() {
        let mut p = map(&[("50Hz", -3.0), ("10000Hz", 2.5), ("20000Hz", 1.0)]);
        migrate_effect_params(Eq, &mut p);

        assert!(!p.contains_key("50Hz"));
        assert!(!p.contains_key("10000Hz"));
        assert_eq!(p["50Hz gain (low shelving)"], -3.0);
        assert_eq!(p["10000Hz gain"], 2.5);
        assert_eq!(p["20000Hz gain"], 1.0);
        // The 12 untouched bands come from the defaults (all 0.0).
        assert_eq!(p.len(), 15);
        assert_eq!(p["100Hz gain"], 0.0);
        assert_eq!(p["5000Hz gain"], 0.0);

        let before = p.clone();
        migrate_effect_params(Eq, &mut p);
        assert_eq!(p, before);
    }

    #[test]
    fn compressor_old_names_renamed() {
        let mut p = map(&[
            ("Threshold (dB)", -18.0),
            ("Attack (ms)", 1.0),
            ("Release (ms)", 300.0),
            ("Makeup Gain (dB)", 6.0),
        ]);
        migrate_effect_params(Compressor, &mut p);

        assert!(!p.contains_key("Threshold (dB)"));
        assert!(!p.contains_key("Attack (ms)"));
        assert!(!p.contains_key("Release (ms)"));
        assert!(!p.contains_key("Makeup Gain (dB)"));
        assert_eq!(p["Threshold level (dB)"], -18.0);
        assert_eq!(p["Attack time (ms)"], 1.0);
        assert_eq!(p["Release time (ms)"], 300.0);
        assert_eq!(p["Makeup gain (dB)"], 6.0);
        assert_eq!(p["RMS/peak"], 0.0);
        assert_eq!(p["Ratio (1:n)"], 1.0);
        assert_eq!(p["Knee radius (dB)"], 3.25);
        assert_eq!(p.len(), 7);

        let before = p.clone();
        migrate_effect_params(Compressor, &mut p);
        assert_eq!(p, before);
    }

    #[test]
    fn tube_old_plugin_names_renamed() {
        let mut p = map(&[("Distortion level", 6.0), ("Distortion character", -2.0)]);
        migrate_effect_params(Tube, &mut p);

        assert!(!p.contains_key("Distortion level"));
        assert!(!p.contains_key("Distortion character"));
        assert_eq!(p["Drive"], 6.0);
        assert_eq!(p["Tape--Tube Blend"], -2.0);
        assert_eq!(p.len(), 2);

        let before = p.clone();
        migrate_effect_params(Tube, &mut p);
        assert_eq!(p, before);
    }

    #[test]
    fn noise_cancel_model_dropped_and_real_ports_added() {
        let mut p = map(&[("Model", 0.0)]);
        migrate_effect_params(NoiseCancel, &mut p);

        assert!(!p.contains_key("Model"));
        assert_eq!(p.len(), 4);
        assert_eq!(p["VAD Threshold (%)"], 74.25);
        assert_eq!(p["VAD Grace Period (ms)"], 500.0);
        assert_eq!(p["Retroactive VAD Grace (ms)"], 100.0);
        assert_eq!(p["Dry Mix"], 0.0);

        let before = p.clone();
        migrate_effect_params(NoiseCancel, &mut p);
        assert_eq!(p, before);
    }

    #[test]
    fn migration_does_not_purge_unknown_keys() {
        let mut p = map(&[("bogus-param", 5.0), ("50Hz", -3.0)]);
        migrate_effect_params(Eq, &mut p);
        // Unknown key survives, old short band migrated, all defaults filled.
        assert_eq!(p["bogus-param"], 5.0);
        assert_eq!(p["50Hz gain (low shelving)"], -3.0);
        assert_eq!(p.len(), 16);
    }

    #[test]
    fn migration_preserves_value_when_new_key_already_present() {
        // If a config already carries both the old and the new name, the new
        // (authoritative) value wins and the old key is kept untouched.
        let mut p = map(&[("50Hz", -9.0), ("50Hz gain (low shelving)", -3.0)]);
        migrate_effect_params(Eq, &mut p);
        assert_eq!(p["50Hz gain (low shelving)"], -3.0);
        assert!(p.contains_key("50Hz"));
    }

    #[test]
    fn up_to_date_config_is_idempotent() {
        // A fully-migrated dict (all defaults, no legacy names) is unchanged.
        for key in [Gate, NoiseCancel, Eq, Tube, Compressor] {
            let mut p = default_params(key);
            let before = p.clone();
            migrate_effect_params(key, &mut p);
            assert_eq!(
                p, before,
                "config already up-to-date was mutated for {:?}",
                key
            );
        }
    }

    #[test]
    fn effect_state_from_legacy_boolean() {
        let s = effect_state_from_value(Eq, Some(&Value::Bool(true)));
        assert!(s.active);
        assert_eq!(s.params, default_params(Eq));

        let s = effect_state_from_value(Eq, Some(&Value::Bool(false)));
        assert!(!s.active);
        assert_eq!(s.params, default_params(Eq));

        let s = effect_state_from_value(Eq, None);
        assert!(!s.active);
        assert_eq!(s.params, default_params(Eq));
    }

    #[test]
    fn strip_from_dict_migrates_whole_strip() {
        let json = r#"{
            "uid": "4d00edf7",
            "label": "Micro",
            "kind": "input",
            "volume": 0.9,
            "mute": false,
            "is_mono": true,
            "routes": ["out-1"],
            "device_name": "alsa_input.pci-0000_00_1f.analog-stereo",
            "is_default": false,
            "mode": "physical",
            "effects": {
                "gate": { "active": true, "params": { "Release (ms)": 120.0, "Threshold (dB)": -45.0 } },
                "noise_cancel": { "active": false, "params": { "Model": 0.0 } },
                "eq": { "active": true, "params": { "50Hz": -6.0, "311Hz": 2.0 } },
                "tube": { "active": false, "params": { "Distortion level": 4.5 } },
                "compressor": { "active": false, "params": { "Threshold (dB)": -12.0 } }
            }
        }"#;
        let strip = strip_from_dict(&serde_json::from_str(json).unwrap());

        assert_eq!(strip.uid, "4d00edf7");
        assert_eq!(strip.label, "Micro");
        assert_eq!(strip.kind, StripType::Input);
        assert_eq!(strip.volume, 0.9);
        assert!(strip.is_mono);
        assert_eq!(strip.routes, vec!["out-1".to_string()]);
        assert_eq!(
            strip.device_name.as_deref(),
            Some("alsa_input.pci-0000_00_1f.analog-stereo")
        );
        assert_eq!(strip.effects.len(), 5);

        // Gate: renamed + defaults filled, user value kept.
        let gate = &strip.effects[&Gate];
        assert!(gate.active);
        assert_eq!(gate.params["Decay (ms)"], 120.0);
        assert_eq!(gate.params["Threshold (dB)"], -45.0);
        assert_eq!(gate.params.len(), 7);

        // Noise cancel: Model dropped, real ports present.
        let nc = &strip.effects[&NoiseCancel];
        assert!(!nc.active);
        assert!(!nc.params.contains_key("Model"));
        assert_eq!(nc.params.len(), 4);

        // EQ: migrated + defaults.
        let eq = &strip.effects[&Eq];
        assert!(eq.active);
        assert_eq!(eq.params["50Hz gain (low shelving)"], -6.0);
        assert_eq!(eq.params["311Hz gain"], 2.0);
        assert_eq!(eq.params.len(), 15);

        // Tube: legacy value carried to new port.
        let tube = &strip.effects[&Tube];
        assert_eq!(tube.params["Drive"], 4.5);
        assert_eq!(tube.params["Tape--Tube Blend"], 10.0);

        // Compressor old threshold mapped.
        let comp = &strip.effects[&Compressor];
        assert_eq!(comp.params["Threshold level (dB)"], -12.0);
    }

    #[test]
    fn strip_from_dict_old_boolean_format() {
        let json = r#"{
            "uid": "b1", "label": "B", "kind": "output",
            "effects": {
                "gate": true,
                "eq": false
            }
        }"#;
        let strip = strip_from_dict(&serde_json::from_str(json).unwrap());
        // All five effects always present after from_dict.
        assert_eq!(strip.effects.len(), 5);
        assert!(strip.effects[&Gate].active);
        assert_eq!(strip.effects[&Gate].params.len(), 7);
        assert!(!strip.effects[&Eq].active);
        assert_eq!(strip.effects[&Eq].params.len(), 15);
        assert!(!strip.effects[&Compressor].active);
        assert_eq!(
            strip.effects[&Compressor].params,
            default_params(Compressor)
        );
    }

    #[test]
    fn strip_from_dict_reproduces_current_repo_config_semantics() {
        // A snippet with the shape of the real config.json on disk (which
        // still contains legacy short names) must load without error.
        let json = r#"{
            "uid": "x1", "label": "Casque", "kind": "output", "volume": 0.0,
            "mute": false, "is_mono": false, "routes": [], "is_default": false,
            "effects": {
                "gate":   { "active": false, "params": { "Threshold (dB)": -30.0, "Attack (ms)": 5.0, "Release (ms)": 200.0, "Hold (ms)": 50.0 } },
                "noise_cancel": { "active": false, "params": { "Model": 0.0 } }
            }
        }"#;
        let strip = strip_from_dict(&serde_json::from_str(json).unwrap());
        assert_eq!(strip.kind, StripType::Output);
        assert_eq!(strip.effects[&Gate].params["Decay (ms)"], 200.0);
        assert!(!strip.effects[&NoiseCancel].params.contains_key("Model"));
    }
}
