//! FX tables — single source of truth copied from the Python
//! `strip_model.py` (`DEFAULT_EFFECT_PARAMS`, `BYPASS_PARAMS`) and
//! `audio_engine.py` (`FX_PLUGIN_MAP`, `STEREO_PLUGINS`).
//!
//! Everything here is ordered data: the LADSPA control-port order must be
//! preserved end-to-end (IndexMap), because the SPA-JSON we generate and the
//! golden tests we compare against are order-sensitive.

use indexmap::IndexMap;
use std::path::Path;

use crate::model::{EffectKey, EffectState, StripType};

/// LADSPA plugin directory (identical to the Python `audio_engine.LADSPA_PATH`).
pub const LADSPA_PATH: &str = "/usr/lib/ladspa";

/// Absolute path of a LADSPA `.so` given its file basename (e.g. `"gate_1410"`
/// → `/usr/lib/ladspa/gate_1410.so`).  Mirrors the Python
/// `os.path.join(LADSPA_PATH, f"{plugin_file}.so")`.
pub fn plugin_so_path(plugin_file: &str) -> String {
    format!("{LADSPA_PATH}/{plugin_file}.so")
}

/// Canonical chain order: gate → noise_cancel → eq → tube → compressor.
pub const FX_ORDER: [EffectKey; 5] = [
    EffectKey::Gate,
    EffectKey::NoiseCancel,
    EffectKey::Eq,
    EffectKey::Tube,
    EffectKey::Compressor,
];

pub fn fx_order() -> [EffectKey; 5] {
    FX_ORDER
}

// ---------------------------------------------------------------------------
// DEFAULT_EFFECT_PARAMS (ordered port -> default value)
// ---------------------------------------------------------------------------

/// gate_1410 defaults (transparent/open gate by default).
///
/// NOTE vs the Python source: the gate plugin also exposes
/// `"Output select (-1 = key listen, 0 = gate, 1 = bypass)"`, which Python
/// keeps in its defaults dict.  That key contains `'='` and is therefore
/// ALWAYS dropped by SPA formatting (`spa::format_params`), so it never
/// reaches PipeWire.  Per the J0 task spec we intentionally omit it from the
/// default table to keep the canonical params clean; the SPA output is
/// byte-identical either way.
const GATE_DEFAULT: &[(&str, f32)] = &[
    ("LF key filter (Hz)", 33.6),
    ("HF key filter (Hz)", 23520.0),
    ("Threshold (dB)", -70.0),
    ("Attack (ms)", 250.008),
    ("Hold (ms)", 1500.5),
    ("Decay (ms)", 2001.0),
    ("Range (dB)", -90.0),
];

/// librnnoise_ladspa (noise_suppressor_stereo) defaults.
const NOISE_CANCEL_DEFAULT: &[(&str, f32)] = &[
    ("VAD Threshold (%)", 74.25),
    ("VAD Grace Period (ms)", 500.0),
    ("Retroactive VAD Grace (ms)", 100.0),
    ("Dry Mix", 0.0),
];

/// mbeq_1197 defaults — 15 flat (0.0 dB) bands, exact LADSPA port names.
const EQ_DEFAULT: &[(&str, f32)] = &[
    ("50Hz gain (low shelving)", 0.0),
    ("100Hz gain", 0.0),
    ("156Hz gain", 0.0),
    ("220Hz gain", 0.0),
    ("311Hz gain", 0.0),
    ("440Hz gain", 0.0),
    ("622Hz gain", 0.0),
    ("880Hz gain", 0.0),
    ("1250Hz gain", 0.0),
    ("1750Hz gain", 0.0),
    ("2500Hz gain", 0.0),
    ("3500Hz gain", 0.0),
    ("5000Hz gain", 0.0),
    ("10000Hz gain", 0.0),
    ("20000Hz gain", 0.0),
];

/// TAP TubeWarmth defaults.
const TUBE_DEFAULT: &[(&str, f32)] = &[("Drive", 2.575), ("Tape--Tube Blend", 10.0)];

/// sc4_1882 defaults (plugin's own defaults, transparent compressor).
const COMPRESSOR_DEFAULT: &[(&str, f32)] = &[
    ("RMS/peak", 0.0),
    ("Attack time (ms)", 101.125),
    ("Release time (ms)", 401.0),
    ("Threshold level (dB)", 0.0),
    ("Ratio (1:n)", 1.0),
    ("Knee radius (dB)", 3.25),
    ("Makeup gain (dB)", 0.0),
];

/// Ordered default control ports for an effect.
pub fn default_params(key: EffectKey) -> IndexMap<String, f32> {
    let table: &[(&str, f32)] = match key {
        EffectKey::Gate => GATE_DEFAULT,
        EffectKey::NoiseCancel => NOISE_CANCEL_DEFAULT,
        EffectKey::Eq => EQ_DEFAULT,
        EffectKey::Tube => TUBE_DEFAULT,
        EffectKey::Compressor => COMPRESSOR_DEFAULT,
    };
    table.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
}

// ---------------------------------------------------------------------------
// BYPASS_PARAMS (neutral values merged over the defaults when inactive)
// ---------------------------------------------------------------------------

/// Neutral values that make the plugin transparent when bypassed.
///
/// Per the J0 task spec, the gate entry only lists the two parameters that
/// differ from the defaults (`Threshold (dB)` = -100.0 always-open and
/// `Range (dB)` = 0.0 no reduction).  Python additionally documents
/// `"Output select..."` here but it is filtered out at format time and is not
/// modelled (see [`GATE_DEFAULT`]).
const GATE_BYPASS: &[(&str, f32)] = &[("Threshold (dB)", -100.0), ("Range (dB)", 0.0)];

const NOISE_CANCEL_BYPASS: &[(&str, f32)] = &[("Dry Mix", 1.0)]; // pure dry passthrough

// All 15 EQ bands are already 0.0 dB in the defaults → nothing to override.
const EQ_BYPASS: &[(&str, f32)] = &[];

const TUBE_BYPASS: &[(&str, f32)] = &[("Drive", 0.1)]; // Drive min 0.1 → ~no saturation

const COMPRESSOR_BYPASS: &[(&str, f32)] = &[("Ratio (1:n)", 1.0)]; // 1:1 = no compression

/// Neutral (bypass) values for an effect, to merge on top of the defaults.
pub fn bypass_params(key: EffectKey) -> IndexMap<String, f32> {
    let table: &[(&str, f32)] = match key {
        EffectKey::Gate => GATE_BYPASS,
        EffectKey::NoiseCancel => NOISE_CANCEL_BYPASS,
        EffectKey::Eq => EQ_BYPASS,
        EffectKey::Tube => TUBE_BYPASS,
        EffectKey::Compressor => COMPRESSOR_BYPASS,
    };
    table.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
}

/// Full control dict for an effect given its on/off state.
///
/// Mirrors `_resolve_effect_controls` from `audio_engine.py`:
///
/// - **active**: the user/default params are used as-is;
/// - **inactive**: the BYPASS values are merged **over** the defaults so that
///   *every* control port receives an explicit value.  This matters because
///   PipeWire `set-param` *replaces* (not merges) all control values; a
///   partial spec would reset unmentioned ports to LADSPA defaults and could
///   produce audible artefacts.
pub fn effective_params(key: EffectKey, active: bool) -> IndexMap<String, f32> {
    let mut params = default_params(key);
    if active {
        return params;
    }
    for (port, value) in bypass_params(key) {
        // Insert on an existing key updates in place (IndexMap preserves the
        // canonical port order), exactly like Python dict assignment.
        params.insert(port, value);
    }
    params
}

// ---------------------------------------------------------------------------
// Plugin mapping (FX_PLUGIN_MAP + STEREO_PLUGINS)
// ---------------------------------------------------------------------------

/// LADSPA plugin file, LADSPA label, Python internal node label and whether
/// it has true stereo ports.
///
/// Columns mirror `audio_engine.py`'s `FX_PLUGIN_MAP` tuples
/// `(effect_key, internal_name, plugin_file, ladspa_label)` plus the
/// `STEREO_PLUGINS` stereo flag:
///
/// | Effect | `internal_name` (Python) | `ladspa_file` | `label` | stereo |
/// |---|---|---|---|---|
/// | gate | `gate` | gate_1410 | gate | no → dual-mono `gate_L`/`gate_R` |
/// | noise_cancel | `rnnoise` | librnnoise_ladspa | noise_suppressor_stereo | yes → single node `rnnoise` |
/// | eq | `eq` | mbeq_1197 | mbeq | no → dual-mono `eq_L`/`eq_R` |
/// | tube | `tube` | tap_tubewarmth | tap_tubewarmth | no → dual-mono `tube_L`/`tube_R` |
/// | compressor | `comp` | sc4_1882 | sc4 | yes → single node `comp` |
///
/// Node-name rule for J2 (see `audio_engine.py` `_build_fx_graph` /
/// `_build_fx_props`): a plugin with `stereo == true` is instantiated as a
/// **single** filter-chain node whose `name` is exactly `node_label` (e.g.
/// `"rnnoise"`, `"comp"`).  A mono plugin (`stereo == false`) is
/// instantiated twice as dual-mono twin nodes named `<node_label>_L` and
/// `<node_label>_R` (e.g. `gate_L`, `gate_R`), with `Input`/`Output` ports;
/// the Props control block then carries twin keys `"<node_label>_L"` and
/// `"<node_label>_R"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginSpec {
    /// `.so` basename under the LADSPA path (without extension), e.g.
    /// `"gate_1410"` → `/usr/lib/ladspa/gate_1410.so`.
    pub ladspa_file: &'static str,
    /// LADSPA label of the plugin, e.g. `"gate"`, `"mbeq"`.
    pub label: &'static str,
    /// Internal **node base name** the Python `audio_engine.py` uses for this
    /// effect in its filter.graph / Props SPA-JSON
    /// (`FX_PLUGIN_MAP[..][1]`, the `internal_name` column above).  Twin
    /// suffixing for dual-mono plugins happens *below* this base name.
    pub node_label: &'static str,
    /// True for single-node stereo plugins (sc4_1882, librnnoise_ladspa);
    /// false for dual-mono plugins (gate, mbeq, tap_tubewarmth).
    pub stereo: bool,
}

pub fn plugin_for(key: EffectKey) -> PluginSpec {
    match key {
        EffectKey::Gate => PluginSpec {
            ladspa_file: "gate_1410",
            label: "gate",
            node_label: "gate",
            stereo: false,
        },
        EffectKey::NoiseCancel => PluginSpec {
            ladspa_file: "librnnoise_ladspa",
            label: "noise_suppressor_stereo",
            node_label: "rnnoise",
            stereo: true,
        },
        EffectKey::Eq => PluginSpec {
            ladspa_file: "mbeq_1197",
            label: "mbeq",
            node_label: "eq",
            stereo: false,
        },
        EffectKey::Tube => PluginSpec {
            ladspa_file: "tap_tubewarmth",
            label: "tap_tubewarmth",
            node_label: "tube",
            stereo: false,
        },
        EffectKey::Compressor => PluginSpec {
            ladspa_file: "sc4_1882",
            label: "sc4",
            node_label: "comp",
            stereo: true,
        },
    }
}

/// Audio-port names of a **true-stereo** LADSPA plugin (`STEREO_PLUGINS` in
/// `audio_engine.py`).  Different plugins use different naming conventions:
///
/// - `sc4_1882` (compressor): `Left input` / `Right input` /
///   `Left output` / `Right output`;
/// - `librnnoise_ladspa` (noise_suppressor_stereo): `Input (L)` / `Input (R)` /
///   `Output (L)` / `Output (R)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StereoPorts {
    pub in_l: &'static str,
    pub in_r: &'static str,
    pub out_l: &'static str,
    pub out_r: &'static str,
}

impl PluginSpec {
    /// True-stereo audio port names when the plugin is a single-node stereo
    /// plugin (`STEREO_PLUGINS` lookup); `None` for dual-mono plugins, which
    /// always expose mono ports literally named `Input` / `Output`.
    pub fn stereo_ports(self) -> Option<StereoPorts> {
        match self.ladspa_file {
            "sc4_1882" => Some(StereoPorts {
                in_l: "Left input",
                in_r: "Right input",
                out_l: "Left output",
                out_r: "Right output",
            }),
            "librnnoise_ladspa" => Some(StereoPorts {
                in_l: "Input (L)",
                in_r: "Input (R)",
                out_l: "Output (L)",
                out_r: "Output (R)",
            }),
            _ => None,
        }
    }

    /// Absolute path of this plugin's `.so` under [`LADSPA_PATH`].
    pub fn so_path(self) -> String {
        plugin_so_path(self.ladspa_file)
    }
}

/// Which LADSPA `.so` files exist / are enabled for a chain build.
///
/// Python checks `os.path.exists` on every plugin file inside
/// `_build_fx_graph`/`_build_fx_props`; plugins that are missing from disk are
/// silently skipped.  This set models that gate **explicitly** so the pure
/// builders stay deterministic (golden tests do not depend on the ambient
/// plugin directory).  The headless container has `gate_1410`, `mbeq_1197`,
/// `tap_tubewarmth` and `sc4_1882` (no `librnnoise_ladspa`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSet {
    /// `.so` basenames (e.g. `"mbeq_1197"`) treated as available.
    files: Vec<String>,
}

impl PluginSet {
    /// A set containing exactly the given `.so` basenames.
    pub fn from_files<I: AsRef<str>>(files: &[I]) -> Self {
        PluginSet {
            files: files.iter().map(|f| f.as_ref().to_string()).collect(),
        }
    }

    /// All five plugins of the catalogue (`FX_PLUGIN_MAP`).
    pub fn all() -> Self {
        Self::from_files(&[
            "gate_1410",
            "librnnoise_ladspa",
            "mbeq_1197",
            "tap_tubewarmth",
            "sc4_1882",
        ])
    }

    /// The plugins actually present on disk under [`LADSPA_PATH`], replicating
    /// the Python `os.path.exists` probe at build time.
    pub fn on_disk() -> Self {
        let mut files = Vec::new();
        for file in [
            "gate_1410",
            "librnnoise_ladspa",
            "mbeq_1197",
            "tap_tubewarmth",
            "sc4_1882",
        ] {
            if Path::new(&plugin_so_path(file)).exists() {
                files.push(file.to_string());
            }
        }
        PluginSet { files }
    }

    /// Whether a plugin is in the set.
    pub fn contains(&self, spec: PluginSpec) -> bool {
        self.files.iter().any(|file| file == spec.ladspa_file)
    }

    /// Number of `.so` files in the set (used for logs).
    pub fn count(&self) -> usize {
        self.files.len()
    }
}

/// Effects applicable to a strip kind — convenience over
/// [`EffectKey::for_strip`] (input = 5, output = eq/tube/comp only).
pub fn effects_for_strip(kind: StripType) -> Vec<EffectKey> {
    EffectKey::for_strip(kind)
}

/// Build a map of effects (all applicable to `kind`) as inactive with default
/// params — used to seed a fresh strip before any user data is applied.
pub fn default_effect_states(kind: StripType) -> IndexMap<EffectKey, EffectState> {
    effects_for_strip(kind)
        .into_iter()
        .map(|k| (k, EffectState::inactive_with_defaults(k)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EffectKey::*;

    #[test]
    fn fx_order_is_canonical() {
        let order = fx_order();
        assert_eq!(order, [Gate, NoiseCancel, Eq, Tube, Compressor]);
        assert_eq!(order, EffectKey::all());
    }

    #[test]
    fn eq_default_has_exactly_15_bands() {
        let eq = default_params(Eq);
        assert_eq!(eq.len(), 15);
        let expected: Vec<&str> = EQ_DEFAULT.iter().map(|(k, _)| *k).collect();
        assert_eq!(expected.len(), 15);
        assert!(expected.iter().all(|k| eq.contains_key(*k)));
        assert!(
            eq.values().all(|v| *v == 0.0),
            "all bands must default to 0.0"
        );
        // Band names are the exact LADSPA port names (from strip_model.py).
        assert_eq!(
            eq.get_index(0).unwrap(),
            (&"50Hz gain (low shelving)".to_string(), &0.0)
        );
        assert_eq!(
            eq.get_index(14).unwrap(),
            (&"20000Hz gain".to_string(), &0.0)
        );
    }

    #[test]
    fn effective_params_active_returns_defaults() {
        for k in fx_order() {
            assert_eq!(effective_params(k, true), default_params(k));
        }
        // And active must never carry a bypass override.
        assert_eq!(effective_params(Gate, true)["Threshold (dB)"], -70.0);
        assert_eq!(effective_params(Gate, true)["Range (dB)"], -90.0);
    }

    #[test]
    fn effective_params_inactive_merges_bypass_over_defaults() {
        // Gate: always-open threshold + no gain reduction.
        let gate = effective_params(Gate, false);
        assert_eq!(gate.len(), default_params(Gate).len());
        assert_eq!(gate["Threshold (dB)"], -100.0);
        assert_eq!(gate["Range (dB)"], 0.0);
        // Un-touched ports keep their defaults.
        assert_eq!(gate["LF key filter (Hz)"], 33.6);
        assert_eq!(gate["Hold (ms)"], 1500.5);
        assert_eq!(gate["Attack (ms)"], 250.008);
        // Canonical port order must be preserved (bypass merges in place).
        let keys: Vec<&str> = gate.keys().map(|s| s.as_str()).collect();
        assert_eq!(
            keys,
            [
                "LF key filter (Hz)",
                "HF key filter (Hz)",
                "Threshold (dB)",
                "Attack (ms)",
                "Hold (ms)",
                "Decay (ms)",
                "Range (dB)"
            ]
        );

        // Noise cancel inactive: pure dry passthrough.
        let nc = effective_params(NoiseCancel, false);
        assert_eq!(nc["Dry Mix"], 1.0);
        assert_eq!(nc["VAD Threshold (%)"], 74.25);

        // EQ: nothing to bypass — all bands already 0.0, unchanged.
        assert_eq!(effective_params(Eq, false), default_params(Eq));

        // Tube inactive: Drive at its minimum.
        let tube = effective_params(Tube, false);
        assert_eq!(tube["Drive"], 0.1);
        assert_eq!(tube["Tape--Tube Blend"], 10.0);

        // Compressor inactive: 1:1 ratio.
        let comp = effective_params(Compressor, false);
        assert_eq!(comp["Ratio (1:n)"], 1.0);
        assert_eq!(comp.len(), default_params(Compressor).len());
    }

    #[test]
    fn every_inactive_effect_has_explicit_control_for_all_ports() {
        for k in fx_order() {
            let inactive = effective_params(k, false);
            let defaults = default_params(k);
            assert_eq!(
                inactive.len(),
                defaults.len(),
                "inactive {:?} must carry explicit values for every default port",
                k
            );
            for (port, _) in &defaults {
                assert!(
                    inactive.contains_key(port),
                    "missing port {:?} for {:?}",
                    port,
                    k
                );
            }
        }
    }

    #[test]
    fn plugin_for_maps_plugins_and_stereo_flags() {
        assert_eq!(
            plugin_for(Gate),
            PluginSpec {
                ladspa_file: "gate_1410",
                label: "gate",
                node_label: "gate",
                stereo: false
            }
        );
        assert_eq!(
            plugin_for(NoiseCancel),
            PluginSpec {
                ladspa_file: "librnnoise_ladspa",
                label: "noise_suppressor_stereo",
                node_label: "rnnoise",
                stereo: true
            }
        );
        assert_eq!(
            plugin_for(Eq),
            PluginSpec {
                ladspa_file: "mbeq_1197",
                label: "mbeq",
                node_label: "eq",
                stereo: false
            }
        );
        assert_eq!(
            plugin_for(Tube),
            PluginSpec {
                ladspa_file: "tap_tubewarmth",
                label: "tap_tubewarmth",
                node_label: "tube",
                stereo: false
            }
        );
        assert_eq!(
            plugin_for(Compressor),
            PluginSpec {
                ladspa_file: "sc4_1882",
                label: "sc4",
                node_label: "comp",
                stereo: true
            }
        );

        // Stereo plugins are exactly the STEREO_PLUGINS set in audio_engine.py.
        assert!(plugin_for(NoiseCancel).stereo);
        assert!(plugin_for(Compressor).stereo);
        assert!(!plugin_for(Gate).stereo);
        assert!(!plugin_for(Eq).stereo);
        assert!(!plugin_for(Tube).stereo);
    }

    #[test]
    fn plugin_node_labels_match_python_fx_plugin_map() {
        // Exact `internal_name` column of audio_engine.py FX_PLUGIN_MAP:
        // ('gate',  'gate',  'gate_1410',        'gate'),
        // ('noise_cancel', 'rnnoise', 'librnnoise_ladspa', 'noise_suppressor_stereo'),
        // ('eq',  'eq',  'mbeq_1197',       'mbeq'),
        // ('tube', 'tube', 'tap_tubewarmth', 'tap_tubewarmth'),
        // ('compressor', 'comp', 'sc4_1882', 'sc4').
        let expected: [(EffectKey, &str, bool); 5] = [
            (Gate, "gate", false),
            (NoiseCancel, "rnnoise", true),
            (Eq, "eq", false),
            (Tube, "tube", false),
            (Compressor, "comp", true),
        ];
        for (key, node_label, stereo) in expected {
            let spec = plugin_for(key);
            assert_eq!(spec.node_label, node_label, "{key:?} node label");
            assert_eq!(spec.stereo, stereo, "{key:?} stereo flag");
        }
    }

    #[test]
    fn effects_for_strip_input_five_output_three() {
        let input = effects_for_strip(StripType::Input);
        assert_eq!(input.len(), 5);
        let output = effects_for_strip(StripType::Output);
        assert_eq!(output, vec![Eq, Tube, Compressor]);
    }

    #[test]
    fn default_effect_states_seed_kind_specific_strips() {
        let states = default_effect_states(StripType::Output);
        assert_eq!(states.len(), 3);
        assert!(states.contains_key(&Eq));
        assert!(states.contains_key(&Tube));
        assert!(states.contains_key(&Compressor));
        assert!(!states.contains_key(&Gate));
        for state in states.values() {
            assert!(!state.active);
            assert!(!state.params.is_empty());
        }
    }

    #[test]
    fn stereo_ports_match_python_stereo_plugins() {
        // STEREO_PLUGINS in audio_engine.py, by plugin_file basename.
        assert_eq!(
            plugin_for(Compressor).stereo_ports(),
            Some(StereoPorts {
                in_l: "Left input",
                in_r: "Right input",
                out_l: "Left output",
                out_r: "Right output",
            })
        );
        assert_eq!(
            plugin_for(NoiseCancel).stereo_ports(),
            Some(StereoPorts {
                in_l: "Input (L)",
                in_r: "Input (R)",
                out_l: "Output (L)",
                out_r: "Output (R)",
            })
        );
        // Mono plugins have no true-stereo port set.
        assert_eq!(plugin_for(Gate).stereo_ports(), None);
        assert_eq!(plugin_for(Eq).stereo_ports(), None);
        assert_eq!(plugin_for(Tube).stereo_ports(), None);
    }

    #[test]
    fn plugin_set_membership_and_on_disk() {
        let set = PluginSet::from_files(&["mbeq_1197", "sc4_1882"]);
        assert!(set.contains(plugin_for(Eq)));
        assert!(set.contains(plugin_for(Compressor)));
        assert!(!set.contains(plugin_for(Gate)));
        assert!(!set.contains(plugin_for(NoiseCancel)));

        // On the headless container all four LADSPA plugins are installed and
        // librnnoise_ladspa is not; on_disk() must mirror the Python probe.
        let disk = PluginSet::on_disk();
        assert_eq!(
            disk.contains(plugin_for(Eq)),
            std::path::Path::new("/usr/lib/ladspa/mbeq_1197.so").exists()
        );
    }

    #[test]
    fn plugin_so_path_matches_python() {
        assert_eq!(plugin_so_path("mbeq_1197"), "/usr/lib/ladspa/mbeq_1197.so");
        assert_eq!(plugin_for(Eq).so_path(), "/usr/lib/ladspa/mbeq_1197.so");
    }

    #[test]
    fn tables_match_python_constants_semantically() {
        // Spot-check a few exact port names/values copied from strip_model.py.
        assert_eq!(default_params(Gate)["LF key filter (Hz)"], 33.6);
        assert_eq!(default_params(Gate)["HF key filter (Hz)"], 23520.0);
        assert_eq!(default_params(Gate)["Decay (ms)"], 2001.0);
        assert_eq!(default_params(NoiseCancel)["VAD Threshold (%)"], 74.25);
        assert_eq!(
            default_params(NoiseCancel)["Retroactive VAD Grace (ms)"],
            100.0
        );
        assert_eq!(default_params(Tube)["Drive"], 2.575);
        assert_eq!(default_params(Tube)["Tape--Tube Blend"], 10.0);
        assert_eq!(default_params(Compressor)["Knee radius (dB)"], 3.25);
        assert_eq!(default_params(Compressor)["Makeup gain (dB)"], 0.0);
    }
}
