//! Pure SPA-JSON builders for the filter-chain module and its hot-reload
//! `Props` payload.
//!
//! # Superseded (J2)
//!
//! The generic graph/props builders now live in **holaf-core**
//! ([`holaf_core::spa`] — `build_graph` / `build_props` / `control_pairs`)
//! and are golden-tested against Python fixtures.  The EQ
//! helpers below reproduce, *for the EQ-only chain used by the J1 spike*, the
//! exact strings that `audio_engine.py` builds with `_build_fx_graph` /
//! `_create_fx_chain`.  Port names come from
//! [`holaf_core::fx::default_params`] (the real LADSPA names, order
//! preserved).  They are retained as a compact EQ reference and their unit
//! tests pin the J1 spike output.
//!
//! Note the one deliberate difference: [`eq_control_pairs`] interleaves the
//! channels per band (`eq_L:port1`, `eq_R:port1`, …), while the canonical
//! flat order chosen by holaf-core (and emitted by `module-filter-chain`)
//! groups per node (`eq_L:port1…eq_L:portN`, `eq_R:…`).  Both resolve
//! correctly in `set_control_value` (by name); the spike now uses the
//! holaf-core grouping.
//!
//! # Scope note (spike vs full chain)
//!
//! The Python engine always instantiates *every* plugin of the strip
//! (gate → eq → tube → …; inactive ones with neutral BYPASS values) so later
//! toggles can hot-reload.  J1 phase 2 only needs **one simple chain** — we
//! build the dual-mono MBEQ (`eq_L`/`eq_R`) alone.  Lifting the generic
//! multi-plugin graph/props builder into `holaf-core` with golden fixtures is
//! J2 work (see `holaf-core/src/spa.rs`).

use holaf_core::fx;
use holaf_core::model::EffectKey;
use holaf_core::spa::format_params;
use indexmap::IndexMap;

/// LADSPA plugin directory (same as the Python `LADSPA_PATH`).
pub const LADSPA_PATH: &str = "/usr/lib/ladspa";

/// Absolute path of a LADSPA `.so` given its file basename.
pub fn plugin_path(plugin_file: &str) -> String {
    format!("{LADSPA_PATH}/{plugin_file}.so")
}

/// Ordered control dict of the EQ (15 flat bands).  Values can be overridden
/// afterwards by the caller to test non-flat settings.
pub fn eq_params() -> IndexMap<String, f32> {
    fx::default_params(EffectKey::Eq)
}

/// SPA-JSON control block of an EQ node (`{ "50Hz gain (low shelving)" = 0 …
/// }`), via [`holaf_core::spa::format_params`].
pub fn eq_control_block(params: &IndexMap<String, f32>) -> String {
    format_params(params)
}

/// Build the `filter.graph` SPA-JSON for a **dual-mono** EQ chain, exactly as
/// Python's `_build_fx_graph` would for a chain containing only EQ (active):
///
/// ```text
/// { nodes = [ { type = ladspa name = "eq_L" plugin = "<mbeq>" label = "mbeq"
///              control = {…} }
///             { type = ladspa name = "eq_R" … } ]
///   links = [  ]
///   inputs = [ "eq_L:Input", "eq_R:Input" ]
///   outputs = [ "eq_L:Output", "eq_R:Output" ] }
/// ```
pub fn build_eq_graph(params: &IndexMap<String, f32>) -> String {
    let ctrl = eq_control_block(params);
    let p = plugin_path("mbeq_1197");
    let node = |name: &str| {
        format!(
            "{{ type = ladspa name = \"{name}\" plugin = \"{p}\" label = \"mbeq\" control = {ctrl} }}"
        )
    };
    let nodes = format!("{} {}", node("eq_L"), node("eq_R"));
    format!(
        "{{ nodes = [ {nodes} ] links = [  ] inputs = [ \"eq_L:Input\", \"eq_R:Input\" ] outputs = [ \"eq_L:Output\", \"eq_R:Output\" ] }}"
    )
}

/// Assemble the full `load-module` argument string for the filter-chain
/// module, mirroring Python `_create_fx_chain`:
///
/// ```text
/// { node.name = "…" node.description = "…" media.name = "…"
///   filter.graph = {…}
///   capture.props = { … } playback.props = { … } }
/// ```
///
/// `is_output` selects the *capture/playback* flavour used by the Python
/// engine (see `_create_fx_chain`):
///
/// - output strip: capture is an `Audio/Sink` (`media.role = comms`) that
///   input strips route into; playback drives the master device
///   (`target.object`).
/// - input strip: capture follows the source (`target.object` on capture);
///   playback is an `Audio/Source` virtual source.
///
/// The anti-WirePlumber props (`node.passive`, `stream.dont-remix`,
/// `audio.channels=2`, `audio.position=[ FL, FR ]`) are reproduced exactly.
pub fn build_filter_chain_config(
    node_name: &str,
    description: &str,
    graph: &str,
    is_output: bool,
    target_object: Option<&str>,
) -> String {
    let target = target_object
        .map(|t| format!(" target.object = \"{t}\""))
        .unwrap_or_default();

    let (capture_props, playback_props) = if is_output {
        (
            // Output: capture stream presents itself as a virtual sink
            // (receives routed audio), role comms so WirePlumber doesn't
            // treat it as a regular media sink.
            "media.class = Audio/Sink node.passive = true stream.dont-remix = true audio.channels = 2 audio.position = [ FL, FR ] media.role = comms".to_string(),
            // Output: playback drives the physical/virtual device.
            format!("node.passive = true stream.dont-remix = true audio.channels = 2 audio.position = [ FL, FR ]{target}"),
        )
    } else {
        (
            // Input: capture follows the external source.
            format!("node.passive = true stream.dont-remix = true audio.channels = 2 audio.position = [ FL, FR ]{target}"),
            // Input: playback exports a virtual Audio/Source.
            "media.class = Audio/Source node.passive = true stream.dont-remix = true audio.channels = 2 audio.position = [ FL, FR ]".to_string(),
        )
    };

    format!(
        "{{ node.name = \"{node_name}\" node.description = \"{description}\" media.name = \"{description}\" \
         filter.graph = {graph} \
         capture.props = {{ {capture_props} }} \
         playback.props = {{ {playback_props} }} }}"
    )
}

/// Flatten EQ control params into the `("eq_L:<port>", value)` /
/// `("eq_R:<port>", value)` pairs that the filter-chain's `Props` *params*
/// struct expects (`set_control_value` resolves `"<node>:<port>"`).
///
/// Every band is emitted for both channels (30 pairs) so a hot-reload never
/// leaves a port at an unintended LADSPA default.
pub fn eq_control_pairs(params: &IndexMap<String, f32>) -> Vec<(String, f32)> {
    let mut out = Vec::with_capacity(params.len() * 2);
    for (port, value) in params {
        out.push((format!("eq_L:{port}"), *value));
        out.push((format!("eq_R:{port}"), *value));
    }
    out
}

/// Build the **Python-style nested** Props string (`_build_fx_props`) for the
/// EQ-only chain:
///
/// ```text
/// { "eq_L" = { <ctrl> } "eq_R" = { <ctrl> } }
/// ```
///
/// Kept only to *document the divergence*: as discovered in J1 phase 2, on
/// PipeWire 0.3.65 (and per the 1.2.5 `module-filter-chain.c` source) this
/// shape is a **silent no-op** — `spa_json_to_pod` drops the unknown per-node
/// keys and the module only reads the flat `params` struct (see
/// [`eq_control_pairs`]).  The Rust spike therefore sends the flat form.
pub fn build_props_python_style(params: &IndexMap<String, f32>) -> String {
    let ctrl = eq_control_block(params);
    format!("{{ \"eq_L\" = {ctrl} \"eq_R\" = {ctrl} }}")
}

/// Build the flat `Props` SPA-JSON (documentation form of what the Rust Pod
/// encodes), matching what `pw-cli set-param … Props { … }` would take:
///
/// ```text
/// { "params" = [ "eq_L:50Hz gain (low shelving)" 4.1 … ] }
/// ```
pub fn build_props_flat(params: &IndexMap<String, f32>) -> String {
    let pairs = eq_control_pairs(params);
    let items: Vec<String> = pairs
        .iter()
        .map(|(name, v)| format!("{:?} {}", name, v))
        .collect();
    format!("{{ \"params\" = [ {} ] }}", items.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eq_params_have_15_real_band_names() {
        let p = eq_params();
        assert_eq!(p.len(), 15);
        assert_eq!(p.get_index(0).unwrap().0, "50Hz gain (low shelving)");
        assert_eq!(p.get_index(14).unwrap().0, "20000Hz gain");
        assert!(p.values().all(|v| *v == 0.0));
    }

    #[test]
    fn eq_control_block_matches_python_shape() {
        let p = eq_params();
        let block = eq_control_block(&p);
        assert!(block.starts_with("{ \"50Hz gain (low shelving)\" = 0"));
        assert!(block.ends_with('}'));
        // All 15 keys are present, quoted.
        for k in p.keys() {
            assert!(block.contains(&format!("{k:?}")), "missing {k}");
        }
    }

    #[test]
    fn graph_reference_string() {
        let p = eq_params();
        let g = build_eq_graph(&p);
        // Structural markers mirroring _build_fx_graph.
        assert!(g.starts_with("{ nodes = [ { type = ladspa name = \"eq_L\" plugin = \"/usr/lib/ladspa/mbeq_1197.so\" label = \"mbeq\" control = "));
        assert!(g.contains("name = \"eq_R\""));
        assert!(g.contains("links = [  ]"));
        assert!(g.contains("inputs = [ \"eq_L:Input\", \"eq_R:Input\" ]"));
        assert!(g.contains("outputs = [ \"eq_L:Output\", \"eq_R:Output\" ]"));
    }

    #[test]
    fn filter_chain_config_output_flavour() {
        let p = eq_params();
        let cfg = build_filter_chain_config(
            "Holaf_FX_x",
            "Holaf FX x",
            &build_eq_graph(&p),
            true,
            Some("Holaf_Strip_x"),
        );
        assert!(cfg.starts_with("{ node.name = \"Holaf_FX_x\""));
        assert!(cfg.contains("capture.props = { media.class = Audio/Sink node.passive = true stream.dont-remix = true audio.channels = 2 audio.position = [ FL, FR ] media.role = comms }"));
        assert!(cfg.contains("target.object = \"Holaf_Strip_x\""));
        assert!(cfg.ends_with(" } }"));
    }

    #[test]
    fn eq_control_pairs_covers_l_and_r_all_bands() {
        let p = eq_params();
        let pairs = eq_control_pairs(&p);
        assert_eq!(pairs.len(), 30);
        assert!(pairs
            .iter()
            .all(|(n, _)| n.starts_with("eq_L:") || n.starts_with("eq_R:")));
        assert!(pairs.iter().filter(|(n, _)| n.starts_with("eq_L:")).count() == 15);
    }

    #[test]
    fn props_python_style_is_nested_per_node() {
        let p = eq_params();
        let s = build_props_python_style(&p);
        assert_eq!(
            s,
            format!(
                "{{ \"eq_L\" = {} \"eq_R\" = {} }}",
                eq_control_block(&p),
                eq_control_block(&p)
            )
        );
    }

    #[test]
    fn props_flat_reference() {
        let mut p = eq_params();
        p.insert("50Hz gain (low shelving)".into(), 4.1);
        p.insert("10000Hz gain".into(), -8.5);
        let s = build_props_flat(&p);
        assert!(s.starts_with("{ \"params\" = ["));
        assert!(s.contains("\"eq_L:50Hz gain (low shelving)\" 4.1"));
        assert!(s.contains("\"eq_R:10000Hz gain\" -8.5"));
        assert!(s.ends_with(" ] }"));
    }
}
