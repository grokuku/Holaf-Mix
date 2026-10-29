//! Pure graph-planning helpers used by the J2 reconciler.
//!
//! All functions here are **pure**: they take [`Strip`]/[`PluginSet`] values
//! and return names or booleans — no PipeWire, no daemon, fully unit-testable.
//! The names mirror the Python `audio_engine.py` node naming exactly:
//!
//! | Object | Node name |
//! |---|---|
//! | strip null-sink / virtual node | `Holaf_Strip_<uid>` |
//! | filter-chain module node | `Holaf_FX_<uid>` |
//! | chain capture stream | `input.Holaf_FX_<uid>` |
//! | chain playback stream | `output.Holaf_FX_<uid>` |
//!
//! The reconciler (holaf-pw) uses these to diff the *desired* graph (from the
//! applied strips) against the *live* registry and to decide whether a strip
//! needs a filter-chain (only when at least one *applicable* effect is active
//! **and** its plugins are installed — mirroring Python `_has_active_fx` +
//! `_build_fx_graph` returning an empty string).

use crate::fx::{self, PluginSet};
use crate::model::{EffectKey, Strip, StripType};

/// The null-sink node name of a virtual strip, `Holaf_Strip_<uid>`.
pub fn strip_node_name(uid: &str) -> String {
    format!("Holaf_Strip_{uid}")
}

/// The base filter-chain node name of a strip, `Holaf_FX_<uid>`.
pub fn fx_node_name(uid: &str) -> String {
    format!("Holaf_FX_{uid}")
}

/// The chain capture stream node, `input.Holaf_FX_<uid>` — the target input
/// strips route *into* when the target has an FX chain.
pub fn fx_input_node_name(uid: &str) -> String {
    format!("input.{}", fx_node_name(uid))
}

/// The chain playback stream node, `output.Holaf_FX_<uid>` — the "effective
/// source" other strips listen to when the source strip has an FX chain.
pub fn fx_output_node_name(uid: &str) -> String {
    format!("output.{}", fx_node_name(uid))
}

/// Does the strip want a filter-chain?
///
/// Mirrors Python: the chain is built only when at least one **applicable**
/// effect is active **and** the graph is non-empty (at least one of the
/// active chain's plugins is available on disk).  With no active effect the
/// strip routes directly (monitor ports / raw node) and no virtual
/// Audio/Source is exported — that is what prevents WirePlumber from
/// auto-connecting microphones to speakers.
pub fn wants_fx_chain(strip: &Strip, avail: &PluginSet) -> bool {
    let applicable = EffectKey::for_strip(strip.kind);
    let any_active = applicable
        .iter()
        .any(|k| strip.effects.get(k).map(|s| s.active).unwrap_or(false));
    if !any_active {
        return false;
    }
    // At least one available plugin must be usable by this strip kind, else
    // the SPA graph builder would return "" (Python returns None → no chain).
    applicable
        .iter()
        .any(|k| avail.contains(fx::plugin_for(*k)))
}

/// True when the strip is backed by a physical device (`device_name` set):
/// input strips capture from that source, output strips play to that sink.
///
/// Virtual strips (no `device_name`) get a null-sink node named
/// [`strip_node_name`].
pub fn is_physical(strip: &Strip) -> bool {
    strip
        .device_name
        .as_deref()
        .map(|d| !d.is_empty())
        .unwrap_or(false)
}

/// The role of a strip's FX chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainFlavour {
    /// Input strip chain: capture follows the source (or null-sink monitor),
    /// playback exports a virtual `Audio/Source`.
    Input,
    /// Output strip chain: capture presents a virtual `Audio/Sink`, playback
    /// drives the sink/device (`target.object`).
    Output,
}

impl ChainFlavour {
    pub fn of(strip: &Strip) -> Self {
        match strip.kind {
            StripType::Input => ChainFlavour::Input,
            StripType::Output => ChainFlavour::Output,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx;
    use crate::model::{EffectKey, EffectState, StripType};

    fn strip_with_effects(kind: StripType, effects: &[(EffectKey, bool)]) -> Strip {
        let mut strip = Strip::new("uid-1", kind);
        for (k, active) in effects {
            strip
                .effects
                .insert(*k, EffectState::inactive_with_defaults(*k));
            strip.effects.get_mut(k).unwrap().active = *active;
        }
        strip
    }

    fn avail(files: &[&'static str]) -> PluginSet {
        PluginSet::from_files(files)
    }

    #[test]
    fn node_names_match_python_audio_engine() {
        assert_eq!(strip_node_name("abc"), "Holaf_Strip_abc");
        assert_eq!(fx_node_name("abc"), "Holaf_FX_abc");
        assert_eq!(fx_input_node_name("abc"), "input.Holaf_FX_abc");
        assert_eq!(fx_output_node_name("abc"), "output.Holaf_FX_abc");
    }

    #[test]
    fn wants_chain_only_when_applicable_effect_active_and_plugin_available() {
        // Output EQ active, mbeq installed → chain.
        let out_eq = strip_with_effects(StripType::Output, &[(EffectKey::Eq, true)]);
        assert!(wants_fx_chain(&out_eq, &avail(&["mbeq_1197"])));
        // Output gate is NOT applicable (output chain is eq/tube/comp) — even
        // with gate_1410 installed, an active gate on an output must not force
        // a chain (holaf-core restricts the chain to the strip kind).
        let out_gate = strip_with_effects(StripType::Output, &[(EffectKey::Gate, true)]);
        assert!(!wants_fx_chain(&out_gate, &avail(&["gate_1410"])));
        // Input gate active → applicable → chain.
        let in_gate = strip_with_effects(StripType::Input, &[(EffectKey::Gate, true)]);
        assert!(wants_fx_chain(&in_gate, &avail(&["gate_1410"])));
        // Input noise_cancel active + mbeq installed → chain: the graph keeps
        // every *available* applicable plugin (here the inactive dual-mono EQ)
        // even when the active plugin (rnnoise) is absent — Python does the
        // same (inactive effects are always in the graph).
        let in_nc = strip_with_effects(StripType::Input, &[(EffectKey::NoiseCancel, true)]);
        assert!(wants_fx_chain(&in_nc, &avail(&["mbeq_1197"])));
        // …but with rnnoise installed the active plugin is itself usable.
        assert!(wants_fx_chain(&in_nc, &avail(&["mbeq_1197", "librnnoise_ladspa"])));
        // No chain when *no* applicable plugin is available (graph empty).
        assert!(!wants_fx_chain(&in_nc, &avail(&[])));
        // All inactive → no chain.
        let out_flat = strip_with_effects(
            StripType::Output,
            &[(EffectKey::Eq, false), (EffectKey::Tube, false)],
        );
        assert!(!wants_fx_chain(&out_flat, &avail(&["mbeq_1197"])));
    }

    #[test]
    fn physical_detection_uses_device_name() {
        let mut physical = Strip::with_uid("p", "Mic", StripType::Input);
        physical.device_name = Some("alsa_input.usb.analog-stereo".into());
        assert!(is_physical(&physical));

        let virtual_ = Strip::with_uid("v", "Web", StripType::Input);
        assert!(!is_physical(&virtual_));
        // Empty string treated as virtual (same convention as migration).
        let mut blank = Strip::with_uid("b", "B", StripType::Output);
        blank.device_name = Some(String::new());
        assert!(!is_physical(&blank));
    }

    #[test]
    fn chain_flavour_follows_strip_kind() {
        let input = Strip::with_uid("i", "I", StripType::Input);
        assert_eq!(ChainFlavour::of(&input), ChainFlavour::Input);
        let output = Strip::with_uid("o", "O", StripType::Output);
        assert_eq!(ChainFlavour::of(&output), ChainFlavour::Output);
    }

    #[test]
    fn wants_chain_empty_when_no_plugin_available() {
        let out_eq = strip_with_effects(StripType::Output, &[(EffectKey::Eq, true)]);
        assert!(!wants_fx_chain(&out_eq, &avail(&[])));
        // fx default params sanity (import used).
        assert_eq!(fx::default_params(EffectKey::Eq).len(), 15);
    }
}
