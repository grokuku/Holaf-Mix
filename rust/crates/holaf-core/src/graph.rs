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

/// Does the strip have at least one **applicable** effect enabled?
///
/// Mirrors Python `_has_active_fx`: only the effects that exist for the strip
/// kind count (outputs have no gate / noise_cancel), so an enabled-but-
/// inapplicable effect never forces a chain.
pub fn has_active_fx(strip: &Strip) -> bool {
    EffectKey::for_strip(strip.kind)
        .iter()
        .any(|k| strip.effects.get(k).map(|s| s.active).unwrap_or(false))
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
    if !has_active_fx(strip) {
        return false;
    }
    let applicable = EffectKey::for_strip(strip.kind);
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

/// PulseAudio/PipeWire name of a sink node's monitor stream, `<node>.monitor`.
///
/// The `pipewire-pulse` server synthesises one monitor source per sink under
/// that exact name (verified live: `pactl list short sources` shows
/// `Holaf_Strip_<uid>.monitor` for every null-sink the daemon creates), so the
/// name is derived rather than looked up in the PipeWire registry (monitors
/// are pulse-level objects, absent from `pw-dump`).
pub fn monitor_source_name(node_name: &str) -> String {
    format!("{node_name}.monitor")
}

/// The **source name the Python VU-meter engine must record from** to display
/// `strip`'s level (spec §6: metering stays Python-local — sounddevice opens a
/// capture stream on `PULSE_SOURCE=<this name>` — the daemon only names the
/// sources).
///
/// | case | source to record |
/// |---|---|
/// | input, physical | `<device_name>` (PipeWire exposes the source node itself) |
/// | input, virtual | `Holaf_Strip_<uid>.monitor` (apps play into that null-sink) |
/// | output, physical | `<device_name>.monitor` |
/// | output, virtual | `Holaf_Strip_<uid>.monitor` |
///
/// # Why monitors rather than the FX chain's virtual source
///
/// The Python engine used to meter an input strip with a live chain on
/// `output.Holaf_FX_<uid>` (the chain's playback stream, an `Audio/Source`).
/// That node is a **passive** stream: it only runs while the strip is routed
/// somewhere, and capturing it is not portable — measured on PipeWire 0.3.65 +
/// WirePlumber 0.4.13, a capture stream on that node opens, links, and reads
/// pure zeros (with the strip audibly playing through its chain), while the
/// null-sink monitor of the *same* signal reads it correctly.  Monitors are
/// synthesised by `pipewire-pulse` for every sink, are always running, and are
/// exactly what a strip's VU bar should show (the level of the signal on that
/// strip, pre-FX — like the input meter of a mixing desk).
///
/// Output strips need no special case: with a chain the chain's playback
/// stream drives the strip's own master node, so its monitor is post-FX.
pub fn meter_source_name(strip: &Strip) -> String {
    match (strip.kind, strip.device_name.as_deref().filter(|d| !d.is_empty())) {
        // Physical input: PipeWire exposes the device node itself as a
        // recordable source (same convention as the Python engine).
        (StripType::Input, Some(device)) => device.to_string(),
        (_, Some(device)) => monitor_source_name(device),
        (_, None) => monitor_source_name(&strip_node_name(&strip.uid)),
    }
}

/// Every node name a strip can present as the **source** end of a route.
///
/// A route between two strips is a link between one of the source's output
/// nodes and one of the target's input nodes — and *which* node depends on the
/// FX chain state: a virtual strip is `Holaf_Strip_<uid>` (its monitor ports)
/// without a chain and `output.Holaf_FX_<uid>` with one.  A chain created or
/// unloaded while the route stays would otherwise leave the previous link
/// behind: the signal would then take **both** paths (doubled level, comb
/// filtering).  Callers therefore always tear down every candidate pair
/// before (re-)creating the link they want.
pub fn route_source_candidates(strip: &Strip) -> Vec<String> {
    let raw = match strip.device_name.as_deref().filter(|d| !d.is_empty()) {
        Some(device) => device.to_string(),
        None => strip_node_name(&strip.uid),
    };
    let mut names = vec![raw];
    let processed = fx_output_node_name(&strip.uid);
    if !names.contains(&processed) {
        names.push(processed);
    }
    names
}

/// Every node name a strip can present as the **target** end of a route
/// (see [`route_source_candidates`]).
pub fn route_target_candidates(strip: &Strip) -> Vec<String> {
    let raw = match strip.device_name.as_deref().filter(|d| !d.is_empty()) {
        Some(device) => device.to_string(),
        None => strip_node_name(&strip.uid),
    };
    let mut names = vec![raw];
    let processed = fx_input_node_name(&strip.uid);
    if !names.contains(&processed) {
        names.push(processed);
    }
    names
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
    fn route_candidates_cover_raw_and_chain_endpoints() {
        let virt_in = Strip::with_uid("vi", "Desktop", StripType::Input);
        assert_eq!(
            route_source_candidates(&virt_in),
            vec!["Holaf_Strip_vi", "output.Holaf_FX_vi"]
        );
        assert_eq!(
            route_target_candidates(&virt_in),
            vec!["Holaf_Strip_vi", "input.Holaf_FX_vi"]
        );
        // Physical strips address the device node in both states.
        let mut mic = Strip::with_uid("pi", "Mic", StripType::Input);
        mic.device_name = Some("alsa_input.usb-mic".into());
        assert_eq!(
            route_source_candidates(&mic),
            vec!["alsa_input.usb-mic", "output.Holaf_FX_pi"]
        );
        // Outputs mirror the same pair (raw sink vs chain capture sink).
        let out = Strip::with_uid("vo", "Out", StripType::Output);
        assert_eq!(
            route_target_candidates(&out),
            vec!["Holaf_Strip_vo", "input.Holaf_FX_vo"]
        );
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

    #[test]
    fn has_active_fx_only_counts_applicable_effects() {
        // Output gate is not part of the output chain.
        let out_gate = strip_with_effects(StripType::Output, &[(EffectKey::Gate, true)]);
        assert!(!has_active_fx(&out_gate));
        let out_eq = strip_with_effects(StripType::Output, &[(EffectKey::Eq, true)]);
        assert!(has_active_fx(&out_eq));
        // An inapplicable active effect plus an applicable inactive one → false.
        let mixed = strip_with_effects(
            StripType::Output,
            &[(EffectKey::Gate, true), (EffectKey::Eq, false)],
        );
        assert!(!has_active_fx(&mixed));
    }

    /// The meter table documented on [`meter_source_name`] — the exact source
    /// names Python's `MeteringEngine` will record from (`PULSE_SOURCE`).
    #[test]
    fn meter_source_names_match_the_python_engine() {
        let mut phys_in = Strip::with_uid("pi", "Mic", StripType::Input);
        phys_in.device_name = Some("alsa_input.usb-mic.analog-stereo".into());
        let mut phys_out = Strip::with_uid("po", "Phones", StripType::Output);
        phys_out.device_name = Some("alsa_output.pci.analog-stereo".into());
        let virt_in = Strip::with_uid("vi", "Desktop", StripType::Input);
        let virt_out = Strip::with_uid("vo", "Virtual", StripType::Output);

        // Physical input → the device source itself.
        assert_eq!(
            meter_source_name(&phys_in).as_str(),
            "alsa_input.usb-mic.analog-stereo"
        );
        // Virtual input → the null-sink monitor (apps play into that sink).
        assert_eq!(
            meter_source_name(&virt_in).as_str(),
            "Holaf_Strip_vi.monitor"
        );
        // Outputs → the monitor of the strip's master node.
        assert_eq!(
            meter_source_name(&phys_out).as_str(),
            "alsa_output.pci.analog-stereo.monitor"
        );
        assert_eq!(
            meter_source_name(&virt_out).as_str(),
            "Holaf_Strip_vo.monitor"
        );
        // The mapping is independent of the FX chain: for outputs the chain
        // plays into the very same node, and for inputs the bar shows the
        // signal on the strip rather than the (passive, unroutable) chain
        // source.
        let mut in_with_fx = virt_in.clone();
        in_with_fx.effects.get_mut(&EffectKey::Eq).unwrap().active = true;
        assert_eq!(meter_source_name(&in_with_fx), meter_source_name(&virt_in));
        // An empty device_name is virtual (same convention as is_physical).
        let mut blank = Strip::with_uid("b", "B", StripType::Output);
        blank.device_name = Some(String::new());
        assert_eq!(meter_source_name(&blank), "Holaf_Strip_b.monitor");
    }

}
