//! FX parameter metadata for the settings dialog.
//!
//! A port of `src/ui/dialogs/effect_settings_dialog.py`: the LADSPA plugins
//! expose plain `f32` control ports with **no metadata** coming from PipeWire
//! (no min/max/default in the props we get), so the Qt UI derives a slider range
//! from the port **name** with a keyword table.  That heuristic is the only
//! thing that makes an EQ band (−70…+30 dB) and a gate attack (0.01…1000 ms)
//! both usable, so it is reproduced verbatim — including the order of the
//! checks (a name containing both `hz` and `gain` is a band gain, an `hz` name
//! containing `filter` is a key filter, …).
//!
//! Two rules are pure and unit-tested here so the dialog code stays paint-only:
//!
//! * [`range_for_param`] → `(min, max, step)`;
//! * [`sorted_param_keys`] → the display order: EQ bands by numeric Hz
//!   (`50Hz`, `100Hz`, …, `20000Hz`), everything else alphabetically; ports whose
//!   name contains `=` are **hidden** (`spa::format_params` drops them, so a
//!   slider for them would never reach the plugin — the Qt dialog skips them for
//!   the same reason).

use indexmap::IndexMap;

/// Slider resolution: Qt maps a float range onto an integer slider by
/// multiplying by 10 when the step is below 1 (`slider_factor` in the Python
/// dialog), which is what makes a 0.01-step control draggable.
pub fn slider_factor(step: f32) -> f32 {
    if step > 0.0 && step < 1.0 {
        10.0
    } else {
        1.0
    }
}

/// Should this control port be shown at all?
pub fn is_editable(name: &str) -> bool {
    !name.contains('=')
}

/// `(min, max, step)` for one control port — `_get_range_for_param`.
pub fn range_for_param(name: &str) -> (f32, f32, f32) {
    let name_l = name.to_lowercase();

    // --- RNNoise (librnnoise_ladspa) -----------------------------------
    if name_l.contains("vad threshold") {
        return (0.0, 99.0, 1.0);
    }
    if name_l.contains("dry mix") {
        return (0.0, 1.0, 0.01);
    }
    if name_l.contains("grace") {
        if name_l.contains("retroactive") {
            return (0.0, 200.0, 1.0);
        }
        return (0.0, 1000.0, 1.0);
    }

    // --- Compressor sc4_1882: RMS/peak has no unit keyword --------------
    if name_l.contains("rms") {
        return (0.0, 1.0, 0.01);
    }

    // --- TAP TubeWarmth: Drive, checked BEFORE the generic rules --------
    if name_l.contains("drive") {
        return (0.1, 10.0, 0.1);
    }
    if name_l.contains("blend") {
        return (-10.0, 10.0, 0.5);
    }

    if name_l.contains("db") {
        if name_l.contains("threshold") {
            return (-70.0, 20.0, 0.5);
        }
        if name_l.contains("makeup") {
            return (0.0, 24.0, 0.5);
        }
        if name_l.contains("knee") {
            return (1.0, 10.0, 0.1);
        }
        if name_l.contains("range") {
            return (-90.0, 0.0, 0.5);
        }
        if name_l.contains("gain") {
            return (-24.0, 24.0, 0.5); // EQ band gains
        }
        return (-60.0, 10.0, 0.5);
    }

    if name_l.contains("hz") {
        if name_l.contains("filter") {
            // Gate key filters.  Qt clamps at 20 kHz, but the plugin's own
            // default for `HF key filter` is 23520 Hz: a 20 kHz ceiling would
            // silently rewrite the value on open, so the range is widened to
            // keep every default representable (asserted below).
            return (0.0, 24_000.0, 10.0);
        }
        // EQ band gains (mbeq_1197 plugin range: −70…+30).
        return (-70.0, 30.0, 0.1);
    }

    if name_l.contains("ms") {
        if name_l.contains("attack") {
            return (0.01, 1000.0, 0.1);
        }
        if name_l.contains("release") {
            return (2.0, 4000.0, 1.0);
        }
        if name_l.contains("hold") {
            return (2.0, 2000.0, 1.0);
        }
        if name_l.contains("decay") {
            return (2.0, 4000.0, 1.0);
        }
    }

    if name_l.contains("ratio") {
        return (1.0, 20.0, 0.5);
    }

    // Gate output select (−1 = key listen, 0 = gate, 1 = bypass).  Hidden in
    // practice (the name contains `=`), kept for the old config keys.
    if name_l.contains("output select") {
        return (-1.0, 1.0, 1.0);
    }

    (0.0, 10.0, 0.1)
}

/// Display order of a dialog: numeric `Hz` prefixes first (`50Hz`, `156Hz`, …),
/// then the rest alphabetically — `_sort_key` in the Python dialog.
pub fn param_sort_key(name: &str) -> (u32, u8, String) {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && name[digits.len()..].to_lowercase().starts_with("hz") {
        if let Ok(value) = digits.parse::<u32>() {
            return (value, 0, name.to_string());
        }
    }
    // `1` puts the alphabetic group after every numeric band.
    (u32::MAX, 1, name.to_lowercase())
}

/// Keys to show, in order (see [`param_sort_key`]).
pub fn sorted_param_keys(params: &IndexMap<String, f32>) -> Vec<String> {
    let mut keys: Vec<String> = params
        .keys()
        .filter(|key| is_editable(key))
        .cloned()
        .collect();
    keys.sort_by_key(|a| param_sort_key(a));
    keys
}

/// Clamp a slider value to its range and snap it to the step, so the value the
/// engine receives is always representable in the dialog (Qt's `QDoubleSpinBox`
/// does the same rounding).
pub fn snap(value: f32, min: f32, max: f32, step: f32) -> f32 {
    if !value.is_finite() {
        return min;
    }
    let snapped = if step > 0.0 {
        (value / step).round() * step
    } else {
        value
    };
    snapped.clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holaf_core::model::EffectKey;

    #[test]
    fn the_documented_ranges_are_reproduced() {
        // RNNoise
        assert_eq!(range_for_param("VAD Threshold (%)"), (0.0, 99.0, 1.0));
        assert_eq!(range_for_param("VAD Grace Period (ms)"), (0.0, 1000.0, 1.0));
        assert_eq!(
            range_for_param("Retroactive VAD Grace (ms)"),
            (0.0, 200.0, 1.0)
        );
        assert_eq!(range_for_param("Dry Mix"), (0.0, 1.0, 0.01));
        // Compressor
        assert_eq!(range_for_param("RMS/peak"), (0.0, 1.0, 0.01));
        assert_eq!(range_for_param("Threshold level (dB)"), (-70.0, 20.0, 0.5));
        assert_eq!(range_for_param("Makeup gain (dB)"), (0.0, 24.0, 0.5));
        assert_eq!(range_for_param("Knee radius (dB)"), (1.0, 10.0, 0.1));
        assert_eq!(range_for_param("Attack time (ms)"), (0.01, 1000.0, 0.1));
        assert_eq!(range_for_param("Release time (ms)"), (2.0, 4000.0, 1.0));
        assert_eq!(range_for_param("Ratio (1:n)"), (1.0, 20.0, 0.5));
        // Tube
        assert_eq!(range_for_param("Drive"), (0.1, 10.0, 0.1));
        assert_eq!(range_for_param("Tape--Tube Blend"), (-10.0, 10.0, 0.5));
        // Gate
        assert_eq!(range_for_param("LF key filter (Hz)"), (0.0, 24_000.0, 10.0));
        assert_eq!(range_for_param("Hold (ms)"), (2.0, 2000.0, 1.0));
        assert_eq!(range_for_param("Decay (ms)"), (2.0, 4000.0, 1.0));
        assert_eq!(range_for_param("Range (dB)"), (-90.0, 0.0, 0.5));
        // EQ band gains
        assert_eq!(
            range_for_param("50Hz gain (low shelving)"),
            (-70.0, 30.0, 0.1)
        );
        // Unknown name → the generic range.
        assert_eq!(range_for_param("Mystery"), (0.0, 10.0, 0.1));
    }

    #[test]
    fn every_default_param_of_every_effect_lands_in_its_range() {
        // A range that excludes the plugin default would make the dialog jump
        // the value on open — a real bug the Qt code shipped once.
        for key in EffectKey::all() {
            let params = holaf_core::fx::default_params(key);
            for (name, value) in &params {
                let (min, max, _step) = range_for_param(name);
                assert!(
                    *value >= min && *value <= max,
                    "{key}/{name}: default {value} outside {min}..{max}"
                );
            }
        }
    }

    #[test]
    fn the_eq_is_sorted_by_frequency_and_equals_ports_are_hidden() {
        let eq = holaf_core::fx::default_params(EffectKey::Eq);
        let keys = sorted_param_keys(&eq);
        assert_eq!(keys.len(), 15);
        assert_eq!(keys[0], "50Hz gain (low shelving)");
        assert_eq!(keys[1], "100Hz gain");
        assert_eq!(keys[2], "156Hz gain");
        assert_eq!(keys[14], "20000Hz gain");

        // The gate's "Output select (-1 = …)" port contains `=` and must not
        // appear (the SPA formatter drops it).
        let mut gate = holaf_core::fx::default_params(EffectKey::Gate);
        gate.insert(
            "Output select (-1 = key listen, 0 = gate, 1 = bypass)".to_string(),
            0.0,
        );
        let keys = sorted_param_keys(&gate);
        assert!(keys.iter().all(|k| !k.contains('=')));
        // Alphabetical tail: Attack before Decay before HF before Hold…
        let attack = keys.iter().position(|k| k == "Attack (ms)").unwrap();
        let hold = keys.iter().position(|k| k == "Hold (ms)").unwrap();
        assert!(attack < hold);
    }

    #[test]
    fn slider_factor_and_snapping() {
        assert_eq!(slider_factor(0.01), 10.0);
        assert_eq!(slider_factor(0.1), 10.0);
        assert_eq!(slider_factor(1.0), 1.0);
        assert_eq!(slider_factor(10.0), 1.0);
        // A 0.1-step control snaps to its step.
        assert!((snap(2.575, 0.1, 10.0, 0.1) - 2.6).abs() < 1e-5);
        // Out-of-range values are clamped, NaN falls back to the minimum.
        assert_eq!(snap(99.0, -70.0, 30.0, 0.1), 30.0);
        assert_eq!(snap(f32::NAN, -70.0, 30.0, 0.1), -70.0);
        assert_eq!(snap(-3.0, 0.0, 10.0, 0.0), 0.0);
    }
}
