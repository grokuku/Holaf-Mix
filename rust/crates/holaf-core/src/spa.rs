//! Safe SPA-JSON formatting for LADSPA filter-chain configurations.
//!
//! This module carries:
//!
//! - [`format_params`] — `_format_params` from `src/backend/audio_engine.py`
//!   (control block of one plugin node);
//! - [`build_graph`] / [`build_graph_with`] — `_build_fx_graph` (the whole
//!   `filter.graph` of a strip: nodes, serial links, input/output ports);
//! - [`build_props`] / [`build_props_with`] and [`control_pairs`] /
//!   [`control_pairs_with`] — the **flat** `Props` payload used by
//!   `Node::set_param(Props)` for hot-reload (J1 correction of the nested
//!   Python `_build_fx_props`, which is a silent no-op on PipeWire).
//!
//! J0 stubbed the two graph/props builders; J2 implements them faithfully on
//! top of the golden fixtures captured from the Python pure functions (see
//! `tests/spa_builders_golden.rs` and `tools/export_fixtures.py`).
//!
//! # Fidelity notes (documented divergences from the Python legacy engine)
//!
//! - **Props shape is flat.**  Python `_build_fx_props` emits a per-node
//!   nested struct `{ "eq_L" = { <port> = v … } … }`.  J1 phase 2 proved this
//!   is ignored by `module-filter-chain` on 0.3.65 *and* 1.2.x: only the flat
//!   `{ "params" = [ "eq_L:<port>" v … ] }` struct is read.  `build_props`
//!   emits the flat form (module emission order: plugin chain order, mono
//!   plugins `<label>_L` then `<label>_R`, ports in control order).
//! - **Chain scope is per strip kind.**  Python `_build_fx_graph` ignores
//!   `strip.kind` and would also place a bypassed `gate` on an *output* chain
//!   whenever `gate_1410.so` is installed.  holaf-core restricts the chain to
//!   [`EffectKey::for_strip`] (input = gate…compressor, output = eq/tube/comp
//!   per rewrite spec §5).  The output golden restricts availability so both
//!   engines agree.
//! - **Plugin availability is explicit** ([`fx::PluginSet`]); the runtime
//!   default is [`fx::PluginSet::on_disk`], mirroring the Python
//!   `os.path.exists` probe.
//! - **Float formatting is Rust's** (shortest round-trip `f32`, no trailing
//!   `.0`).  The fixture exporter emits the same textual form, so graph/props
//!   strings compare byte-for-byte (see `tools/export_fixtures.py`).

use indexmap::IndexMap;

use crate::fx::{self, PluginSet};
use crate::model::{EffectKey, Strip};

/// Format an ordered control-port dict as an SPA-JSON control block.
///
/// Produces `{ "key" = value "key2" = value2 }` — **all** keys are wrapped in
/// double quotes, exactly like the Python implementation
/// (`f'"{k}" = {v}'`).
///
/// Rules (mirroring `_format_params`):
///
/// - Keys containing `'='` are **excluded**.  PipeWire's SPA-JSON parser uses
///   `=` as its key/value separator; a quoted key that still contains `=` can
///   make the parser reject the *entire* control block, silently discarding
///   every control value for that node.  The only affected key today is the
///   gate plugin's `"Output select (-1 = key listen, 0 = gate, 1 = bypass)"`,
///   whose LADSPA default (0 = gate mode) is the correct value anyway.
/// - Values are formatted with the shortest decimal representation that
///   round-trips to the same `f32` (Rust's float `Display`, ryu-style).  This
///   avoids noise such as `2.5750001`.
/// - Non-finite values (`NaN`, `±inf`) are replaced by `0.0` — SPA-JSON has
///   no representation for them and a NaN could poison the whole chain.
/// - An empty dict formats as `{}`.
///
/// # Documented divergence vs the Python formatter (J2 decision)
///
/// Rust `f32` Display is the shortest round-trip decimal (ryu-style) and
/// prints **no** trailing `.0` for integral floats: `23520.0` → `"23520"`.
/// The legacy Python `_format_params` (`audio_engine.py`) interpolates the
/// float repr and prints `"23520.0"`.  This is purely textual — PipeWire's
/// SPA-JSON parser accepts both forms — and is deliberately **not** changed
/// here.  The fixtures exported by `tools/export_fixtures.py` are generated
/// with the same Rust-style text so the comparison is byte-exact.
pub fn format_params(params: &IndexMap<String, f32>) -> String {
    if params.is_empty() {
        return "{}".to_string();
    }
    let items: Vec<String> = params
        .iter()
        .filter(|(key, _)| !key.contains('='))
        .map(|(key, value)| format!("{:?} = {}", key, format_value(*value)))
        .collect();
    if items.is_empty() {
        return "{}".to_string();
    }
    format!("{{ {} }}", items.join(" "))
}

/// Shortest round-trip decimal for an `f32`; non-finite → `"0"`.
fn format_value(value: f32) -> String {
    if !value.is_finite() {
        // SPA-JSON cannot represent NaN/inf; 0.0 is the safe neutral value.
        "0".to_string()
    } else {
        format!("{value}")
    }
}

// ---------------------------------------------------------------------------
// Chain model shared by build_graph and build_props.
//
// Python always includes every *available* plugin of the chain catalogue in
// the graph (inactive ones with neutral BYPASS values), so a later toggle can
// hot-reload via set-param without a restart.  We reproduce that: an effect
// is in the chain when (a) it applies to the strip kind and (b) its plugin is
// available.  Active effects use the stored user params; inactive or absent
// ones use `DEFAULT_EFFECT_PARAMS ⊕ BYPASS_PARAMS` so every control port gets
// an explicit value (set-param replaces rather than merges).
// ---------------------------------------------------------------------------

/// One effect selected for a chain: its key and the exact control-port map to
/// format (active → user params, otherwise defaults+bypass).
fn chain_effects(strip: &Strip, avail: &PluginSet) -> Vec<(EffectKey, IndexMap<String, f32>)> {
    let mut chain = Vec::new();
    for key in EffectKey::for_strip(strip.kind) {
        if !avail.contains(fx::plugin_for(key)) {
            continue;
        }
        let ctrl = match strip.effects.get(&key) {
            Some(state) if state.active => state.params.clone(),
            // Missing keys behave like an inactive effect (Python `_get_fx_data`
            // returns `(False, {})` for absent keys).
            _ => fx::effective_params(key, false),
        };
        chain.push((key, ctrl));
    }
    chain
}

/// Build the `filter.graph` SPA-JSON of a strip given an explicit plugin
/// availability set — see [`build_graph`] for the runtime flavour.
///
/// Fidelity contract (byte-for-byte vs Python `_build_fx_graph` + the
/// Rust-style float formatter of `tools/export_fixtures.py`):
///
/// ```text
/// { nodes = [ … ] links = [ … ] inputs = [ "…", "…" ] outputs = [ "…", "…" ] }
/// ```
///
/// Mono plugins (gate/mbeq/tap_tubewarmth) are instantiated twice as dual-mono
/// nodes `<label>_L` / `<label>_R` with `Input` / `Output` ports; true-stereo
/// plugins (sc4_1882, librnnoise_ladspa) once as a single node with their own
/// port names ([`fx::PluginSpec::stereo_ports`]).  Consecutive effects are
/// chained in series (`prev:Output` → `next:Input`, L and R independently).
pub fn build_graph_with(strip: &Strip, include_controls: bool, avail: &PluginSet) -> String {
    let chain = chain_effects(strip, avail);
    if chain.is_empty() {
        // Python returns the empty string when no plugin is available.
        return String::new();
    }

    struct Ports {
        in_l: String,
        in_r: String,
        out_l: String,
        out_r: String,
    }

    let mut nodes: Vec<String> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    let mut ports: Vec<Ports> = Vec::with_capacity(chain.len());

    for (i, (key, ctrl)) in chain.iter().enumerate() {
        let spec = fx::plugin_for(*key);
        let ctrl_str = format_params(ctrl);
        // `control = {…}` is omitted when include_controls is off or the block
        // is empty — exactly like Python's `control_str` computation.
        let control_suffix = if include_controls && ctrl_str != "{}" {
            format!(" control = {ctrl_str}")
        } else {
            String::new()
        };

        let node = |name: &str| {
            format!(
                "{{ type = ladspa name = \"{name}\" plugin = \"{}\" label = \"{}\"{control_suffix} }}",
                spec.so_path(),
                spec.label
            )
        };

        let this = match spec.stereo_ports() {
            Some(sp) => {
                let name = spec.node_label;
                nodes.push(node(name));
                Ports {
                    in_l: format!("{name}:{}", sp.in_l),
                    in_r: format!("{name}:{}", sp.in_r),
                    out_l: format!("{name}:{}", sp.out_l),
                    out_r: format!("{name}:{}", sp.out_r),
                }
            }
            None => {
                let left = format!("{}_L", spec.node_label);
                let right = format!("{}_R", spec.node_label);
                nodes.push(node(&left));
                nodes.push(node(&right));
                Ports {
                    in_l: format!("{left}:Input"),
                    in_r: format!("{right}:Input"),
                    out_l: format!("{left}:Output"),
                    out_r: format!("{right}:Output"),
                }
            }
        };

        if i > 0 {
            let prev = &ports[i - 1];
            links.push(format!(
                "{{ output = \"{}\" input = \"{}\" }}",
                prev.out_l, this.in_l
            ));
            links.push(format!(
                "{{ output = \"{}\" input = \"{}\" }}",
                prev.out_r, this.in_r
            ));
        }
        ports.push(this);
    }

    let first = &ports[0];
    let last = &ports[ports.len() - 1];
    format!(
        "{{ nodes = [ {} ] links = [ {} ] inputs = [ \"{}\", \"{}\" ] outputs = [ \"{}\", \"{}\" ] }}",
        nodes.join(" "),
        links.join(" "),
        first.in_l,
        first.in_r,
        last.out_l,
        last.out_r
    )
}

/// Build the `filter.graph` SPA-JSON of a strip using the plugins actually
/// installed on disk (like the Python `os.path.exists` probe at build time).
///
/// Emits `{ nodes = … links = … inputs = … outputs = … }`.  All *available*
/// effects of the strip kind are included, inactive ones with neutral BYPASS
/// values, so later toggles can hot-reload via `set-param`.  Empty string when
/// no plugin is available.
pub fn build_graph(strip: &Strip, include_controls: bool) -> String {
    build_graph_with(strip, include_controls, &PluginSet::on_disk())
}

/// Ordered `("<node>:<control port>", value)` pairs of a strip for the flat
/// `Props.params` struct, given an explicit plugin availability set.
///
/// Module emission order (the order `module-filter-chain` itself uses and the
/// canonical fixture order): effects in chain order; for each mono plugin the
/// whole `_L` control block first then the `_R` block; a stereo plugin a single
/// block; ports in control-port order.  `'='` keys are filtered (same rule as
/// [`format_params`]); an effect whose every key is filtered is skipped
/// (Python `continue` when the formatted block is `{}`).
pub fn control_pairs_with(strip: &Strip, avail: &PluginSet) -> Vec<(String, f32)> {
    let mut pairs: Vec<(String, f32)> = Vec::new();
    for (key, ctrl) in chain_effects(strip, avail) {
        let spec = fx::plugin_for(key);
        let ctrl: Vec<(&String, &f32)> = ctrl.iter().filter(|(p, _)| !p.contains('=')).collect();
        if ctrl.is_empty() {
            continue;
        }
        match spec.stereo_ports() {
            Some(_) => {
                for (port, value) in &ctrl {
                    pairs.push((format!("{}:{}", spec.node_label, port), **value));
                }
            }
            None => {
                for twin in [
                    format!("{}_L", spec.node_label),
                    format!("{}_R", spec.node_label),
                ] {
                    for (port, value) in &ctrl {
                        pairs.push((format!("{twin}:{port}"), **value));
                    }
                }
            }
        }
    }
    pairs
}

/// Ordered `("<node>:<port>", value)` pairs using the on-disk plugin set.
pub fn control_pairs(strip: &Strip) -> Vec<(String, f32)> {
    control_pairs_with(strip, &PluginSet::on_disk())
}

/// Build the **flat** Props SPA-JSON of a strip for a given plugin set.
///
/// ```text
/// { "params" = [ "eq_L:50Hz gain (low shelving)" 0 … "comp:RMS/peak" 0 … ] }
/// ```
///
/// This is the J1-corrected shape (`module-filter-chain` only reads
/// `Props.params`); the nested per-node shape the legacy Python
/// `_build_fx_props` emits is a silent no-op on 0.3.65 and 1.2.x.  Empty
/// string when no plugin is available.
pub fn build_props_with(strip: &Strip, avail: &PluginSet) -> String {
    let pairs = control_pairs_with(strip, avail);
    if pairs.is_empty() {
        return String::new();
    }
    let items: Vec<String> = pairs
        .iter()
        .map(|(name, value)| format!("{:?} {}", name, format_value(*value)))
        .collect();
    format!("{{ \"params\" = [ {} ] }}", items.join(" "))
}

/// Build the flat Props SPA-JSON using the plugins installed on disk.
pub fn build_props(strip: &Strip) -> String {
    build_props_with(strip, &PluginSet::on_disk())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx;
    use crate::model::{EffectKey::*, StripType};

    fn map(pairs: &[(&str, f32)]) -> IndexMap<String, f32> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    /// Input strip whose `effects` map only carries the given (key → state)
    /// entries — mirroring a fixture case.
    fn strip_with(kind: StripType, effects: &[(EffectKey, bool, IndexMap<String, f32>)]) -> Strip {
        let mut strip = Strip::new("fixture", kind);
        strip.effects.clear();
        for (key, active, params) in effects {
            strip.effects.insert(
                *key,
                crate::model::EffectState {
                    active: *active,
                    params: params.clone(),
                },
            );
        }
        strip
    }

    fn avail(files: &[&'static str]) -> PluginSet {
        PluginSet::from_files(files)
    }

    #[test]
    fn formats_all_keys_quoted() {
        // Python: '{ "Dry Mix" = 1.0 }' — Rust uses shortest float Display,
        // so integral floats print without a trailing ".0".
        assert_eq!(
            format_params(&map(&[("Dry Mix", 1.0)])),
            r#"{ "Dry Mix" = 1 }"#
        );
        assert_eq!(
            format_params(&map(&[("key with space", -70.0), ("simple", 23520.0)])),
            r#"{ "key with space" = -70 "simple" = 23520 }"#
        );
    }

    #[test]
    fn excludes_keys_containing_equals() {
        assert_eq!(
            format_params(&map(&[("a=b", 1.0), ("ok", 2.0)])),
            r#"{ "ok" = 2 }"#
        );
        // A dict whose only key contains '=' formats as empty braces, never a
        // broken partial block.
        assert_eq!(
            format_params(&map(&[(
                "Output select (-1 = key listen, 0 = gate, 1 = bypass)",
                1.0
            )])),
            "{}"
        );
    }

    #[test]
    fn empty_map_is_empty_braces() {
        assert_eq!(format_params(&IndexMap::new()), "{}");
    }

    #[test]
    fn floats_format_without_noise() {
        // Round-trip shortest representation: no 2.5750001.
        assert_eq!(
            format_params(&map(&[("Drive", 2.575)])),
            r#"{ "Drive" = 2.575 }"#
        );
        assert_eq!(
            format_params(&map(&[("Attack (ms)", 250.008)])),
            r#"{ "Attack (ms)" = 250.008 }"#
        );
        assert_eq!(format_params(&map(&[("v", 0.1)])), r#"{ "v" = 0.1 }"#);
        assert_eq!(format_params(&map(&[("v", -90.0)])), r#"{ "v" = -90 }"#);
    }

    #[test]
    fn non_finite_values_replaced_by_zero() {
        assert_eq!(format_params(&map(&[("x", f32::NAN)])), r#"{ "x" = 0 }"#);
        assert_eq!(
            format_params(&map(&[("x", f32::INFINITY)])),
            r#"{ "x" = 0 }"#
        );
        assert_eq!(
            format_params(&map(&[("x", f32::NEG_INFINITY)])),
            r#"{ "x" = 0 }"#
        );
        // Finite neighbours stay untouched.
        assert_eq!(
            format_params(&map(&[("a", f32::NAN), ("b", 3.5)])),
            r#"{ "a" = 0 "b" = 3.5 }"#
        );
    }

    #[test]
    fn preserves_insertion_order() {
        let p = map(&[
            ("LF key filter (Hz)", 33.6),
            ("HF key filter (Hz)", 23520.0),
            ("Threshold (dB)", -70.0),
        ]);
        assert_eq!(
            format_params(&p),
            r#"{ "LF key filter (Hz)" = 33.6 "HF key filter (Hz)" = 23520 "Threshold (dB)" = -70 }"#
        );
    }

    #[test]
    fn matches_python_gate_format_example() {
        // Port of the Python _format_params docstring example.
        let p = map(&[("Thresh", -30.0)]);
        assert_eq!(format_params(&p), r#"{ "Thresh" = -30 }"#);
    }

    // ------------------------------------------------------------------
    // build_graph structure tests (independent of disk / fixtures)
    // ------------------------------------------------------------------

    #[test]
    fn eq_active_builds_dual_mono_two_sections() {
        // eq only (input strip), active with flat defaults.
        let strip = strip_with(StripType::Input, &[(Eq, true, fx::default_params(Eq))]);
        let graph = build_graph_with(&strip, true, &avail(&["mbeq_1197"]));

        // Two mono sections: eq_L then eq_R, same plugin & label.
        assert!(graph.starts_with(
            "{ nodes = [ { type = ladspa name = \"eq_L\" plugin = \"/usr/lib/ladspa/mbeq_1197.so\" label = \"mbeq\" control = "
        ));
        assert!(graph.contains("name = \"eq_R\""));
        assert_eq!(graph.matches("type = ladspa").count(), 2);
        assert!(graph.contains("links = [  ]"));
        assert!(graph.contains("inputs = [ \"eq_L:Input\", \"eq_R:Input\" ]"));
        assert!(graph.contains("outputs = [ \"eq_L:Output\", \"eq_R:Output\" ]"));
        // Every default band is present in the control block.
        for (port, _) in fx::default_params(Eq) {
            assert!(
                graph.contains(&format!("{port:?} = 0")),
                "missing band {port:?}"
            );
        }
    }

    #[test]
    fn compressor_stereo_is_a_single_section() {
        let strip = strip_with(
            StripType::Input,
            &[(Compressor, true, fx::default_params(Compressor))],
        );
        let graph = build_graph_with(&strip, true, &avail(&["sc4_1882"]));

        assert_eq!(graph.matches("type = ladspa").count(), 1);
        assert!(graph.contains("name = \"comp\""));
        // Stereo port names come from STEREO_PLUGINS, not _L/_R twins.
        assert!(!graph.contains("comp_L"));
        assert!(!graph.contains("comp_R"));
        assert!(graph.contains("inputs = [ \"comp:Left input\", \"comp:Right input\" ]"));
        assert!(graph.contains("outputs = [ \"comp:Left output\", \"comp:Right output\" ]"));
        assert!(graph.contains("links = [  ]"));
    }

    #[test]
    fn multi_effect_chain_links_previous_output_to_next_input_in_series() {
        // gate + eq chained in series, each dual-mono L/R.
        let strip = strip_with(
            StripType::Input,
            &[
                (Gate, false, fx::default_params(Gate)), // inactive -> bypassed
                (Eq, true, fx::default_params(Eq)),
            ],
        );
        let graph = build_graph_with(&strip, true, &avail(&["gate_1410", "mbeq_1197"]));

        assert_eq!(graph.matches("type = ladspa").count(), 4); // gate_L/R + eq_L/R
                                                               // Serial links L and R.
        assert!(graph.contains("links = [ { output = \"gate_L:Output\" input = \"eq_L:Input\" } { output = \"gate_R:Output\" input = \"eq_R:Input\" } ]"));
        // Chain inputs/outputs come from the first / last effect.
        assert!(graph.contains("inputs = [ \"gate_L:Input\", \"gate_R:Input\" ]"));
        assert!(graph.contains("outputs = [ \"eq_L:Output\", \"eq_R:Output\" ]"));
        // Inactive gate control values are the BYPASS ones (always-open).
        assert!(graph.contains("\"Threshold (dB)\" = -100"));
        assert!(graph.contains("\"Range (dB)\" = 0"));
    }

    #[test]
    fn chain_mixes_dual_mono_and_stereo_nodes() {
        // eq (dual-mono) followed by compressor (single stereo node).
        let strip = strip_with(
            StripType::Input,
            &[
                (Eq, true, fx::default_params(Eq)),
                (Compressor, true, fx::default_params(Compressor)),
            ],
        );
        let graph = build_graph_with(&strip, true, &avail(&["mbeq_1197", "sc4_1882"]));

        assert_eq!(graph.matches("type = ladspa").count(), 3); // eq_L eq_R comp
        assert!(graph.contains("links = [ { output = \"eq_L:Output\" input = \"comp:Left input\" } { output = \"eq_R:Output\" input = \"comp:Right input\" } ]"));
        assert!(graph.contains("outputs = [ \"comp:Left output\", \"comp:Right output\" ]"));
    }

    #[test]
    fn inactive_eq_uses_neutral_all_flat_controls() {
        // Inactive effects ignore stored user params (Python re-merges
        // defaults+bypass); for eq that is all-zero.
        let mut user = fx::default_params(Eq);
        user.insert("100Hz gain".into(), -9.0); // must be IGNORED when inactive
        let strip = strip_with(StripType::Input, &[(Eq, false, user)]);
        let graph = build_graph_with(&strip, true, &avail(&["mbeq_1197"]));
        assert!(
            !graph.contains("-9"),
            "inactive user param leaked into graph"
        );
        assert!(graph.contains("\"100Hz gain\" = 0"));
    }

    #[test]
    fn empty_chain_when_no_plugin_available() {
        let strip = strip_with(
            StripType::Input,
            &[(NoiseCancel, true, fx::default_params(NoiseCancel))],
        );
        // librnnoise_ladspa not installed / not enabled -> plugin missing => ignored.
        assert_eq!(build_graph_with(&strip, true, &avail(&[])), "");
        assert_eq!(build_props_with(&strip, &avail(&[])), "");
        assert!(control_pairs_with(&strip, &avail(&[])).is_empty());
    }

    #[test]
    fn rnnoise_single_stereo_node_when_available() {
        let strip = strip_with(
            StripType::Input,
            &[(NoiseCancel, true, fx::default_params(NoiseCancel))],
        );
        let graph = build_graph_with(&strip, true, &avail(&["librnnoise_ladspa"]));
        assert_eq!(graph.matches("type = ladspa").count(), 1);
        assert!(graph.contains("name = \"rnnoise\""));
        assert!(graph.contains("label = \"noise_suppressor_stereo\""));
        assert!(graph.contains("inputs = [ \"rnnoise:Input (L)\", \"rnnoise:Input (R)\" ]"));
        assert!(graph.contains("outputs = [ \"rnnoise:Output (L)\", \"rnnoise:Output (R)\" ]"));
    }

    #[test]
    fn include_controls_false_omits_control_blocks() {
        let strip = strip_with(StripType::Input, &[(Eq, true, fx::default_params(Eq))]);
        let graph = build_graph_with(&strip, false, &avail(&["mbeq_1197"]));
        assert!(!graph.contains("control ="));
        assert!(graph.contains("label = \"mbeq\" }"));
        assert!(graph.contains("inputs = [ \"eq_L:Input\", \"eq_R:Input\" ]"));
    }

    // ------------------------------------------------------------------
    // build_props / control_pairs structure tests
    // ------------------------------------------------------------------

    #[test]
    fn props_are_flat_params_struct_with_node_prefixes() {
        let strip = strip_with(StripType::Input, &[(Eq, true, fx::default_params(Eq))]);
        let props = build_props_with(&strip, &avail(&["mbeq_1197"]));
        assert!(props.starts_with("{ \"params\" = [ "), "got: {props}");
        assert!(props.ends_with(" ] }"));
        // 15 bands x 2 channels, eq_L block before eq_R block.
        assert!(props.contains("\"eq_L:50Hz gain (low shelving)\" 0"));
        assert!(props.contains("\"eq_R:50Hz gain (low shelving)\" 0"));
        assert!(props.contains("\"eq_L:20000Hz gain\" 0"));
        assert!(props.contains("\"eq_R:20000Hz gain\" 0"));
        // The eq_L block must precede the eq_R block (module emission order).
        let l = props.find("\"eq_L:50Hz gain (low shelving)\"").unwrap();
        let r = props.find("\"eq_R:50Hz gain (low shelving)\"").unwrap();
        assert!(l < r);
        let pairs = control_pairs_with(&strip, &avail(&["mbeq_1197"]));
        assert_eq!(pairs.len(), 30);
        assert!(pairs
            .iter()
            .all(|(n, _)| n.starts_with("eq_L:") || n.starts_with("eq_R:")));
    }

    #[test]
    fn props_values_are_shortest_f32_and_equals_filtered() {
        // A gate active with an '='-containing key: that key must never appear
        // and all remaining ports must be explicit.
        let mut gate = fx::default_params(Gate);
        gate.insert(
            "Output select (-1 = key listen, 0 = gate, 1 = bypass)".into(),
            0.0,
        );
        let strip = strip_with(StripType::Input, &[(Gate, true, gate)]);
        let props = build_props_with(&strip, &avail(&["gate_1410"]));
        assert!(
            !props.contains("Output select"),
            "'=' key leaked into props"
        );
        assert!(props.contains("\"gate_L:Threshold (dB)\" -70"));
        assert!(props.contains("\"gate_R:Threshold (dB)\" -70"));
        assert!(props.contains("\"gate_L:HF key filter (Hz)\" 23520"));
        assert!(props.contains("\"gate_R:HF key filter (Hz)\" 23520"));
    }

    #[test]
    fn non_finite_props_values_are_replaced_by_zero() {
        let mut eq = fx::default_params(Eq);
        eq.insert("100Hz gain".into(), f32::NAN);
        let strip = strip_with(StripType::Input, &[(Eq, true, eq)]);
        let props = build_props_with(&strip, &avail(&["mbeq_1197"]));
        assert!(
            props.contains("\"eq_L:100Hz gain\" 0"),
            "NaN must format as 0"
        );
        assert!(!props.contains("NaN"));
    }
}
