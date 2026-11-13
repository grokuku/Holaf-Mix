//! Pure graph-planning helpers used by the J2 reconciler.
//!
//! All functions here are **pure**: they take [`Strip`]/[`PluginSet`] values
//! and return names or booleans — no PipeWire, no daemon, fully unit-testable.
//!
//! # Node naming: stable bus names vs internal ids
//!
//! | Object | Node name | Stable? | Who uses it |
//! |---|---|---|---|
//! | strip bus (a virtual strip's null-sink) | `Holaf_Bus_<Label>` | **yes** — derived from the *label* | other programs (`pactl`, browsers, `pw-link`, JACK clients…) |
//! | filter-chain module node | `Holaf_FX_<uid>` | no — internal | reconciler only |
//! | chain capture stream | `input.Holaf_FX_<uid>` | no — internal | reconciler only |
//! | chain playback stream | `output.Holaf_FX_<uid>` | no — internal | reconciler only |
//!
//! Only the **bus** name is a public contract.  It used to be
//! `Holaf_Strip_<uid>`, which made it unusable as one: deleting a strip in the
//! UI and creating it again mints a new uid, so every external program that had
//! pinned the old name silently lost its bus.  The bus name is therefore
//! derived from the user-visible **label**:
//!
//! ```text
//! "Mix Out"  → Holaf_Bus_Mix_Out
//! "Casque USB" → Holaf_Bus_Casque_USB
//! ```
//!
//! Two strips with the same label get a deterministic suffix in snapshot order
//! (`Holaf_Bus_Mix_Out`, `Holaf_Bus_Mix_Out_2`, …), and a name is *sticky*: a
//! strip keeps the name it already has for as long as its label still slugs to
//! the same base, so deleting one of two same-label strips never renames the
//! survivor (see [`NodeNames::plan`]).
//!
//! Because the name follows the label, the daemon can no longer recover the uid
//! by parsing it.  Ownership is therefore decided by the **naming plan**
//! ([`NodeNames`]): names are collision-free inside a plan, so
//! `node.name == plan[uid]` means "this node is that strip's bus", and any
//! `Holaf_Bus_*` / `Holaf_Strip_*` node outside the plan is debris to destroy
//! (renamed strip, removed strip, node from an older build).  The FX chain names
//! keep the uid because they are internal: no external program is expected to
//! target them, and keeping the uid there ties a chain unambiguously to its
//! strip.
//!
//! # Why `holaf.uid` is *not* what discovery uses
//!
//! Each bus also carries [`UID_PROP`] / [`KIND_PROP`] / [`LABEL_PROP`].  They are
//! **informational**: `pw-dump` and `pw-cli info <node>` show them, which makes
//! an inspection of the graph self-explanatory.
//!
//! They cannot drive discovery, and that is a PipeWire property, not a choice:
//! the properties a client receives in a *registry global* are a **fixed key
//! subset** (`node.name`, `node.description`, `media.class`, `factory.id`,
//! `object.serial`, …), while arbitrary keys only show up in the node **info**
//! (i.e. after binding a proxy to the node).  Measured on PipeWire 0.3.65: a
//! daemon-created bus reports exactly those five keys in its registry global,
//! custom keys included in `pw-dump`'s output and absent from the registry
//! event — so a reconciler that trusted `holaf.uid` would recreate every bus on
//! every apply.  Making PipeWire export extra global keys requires
//! `pw_global_update_keys`, which only a *server-side module* can call.
//!
//! The reconciler (holaf-pw) uses these to diff the *desired* graph (from the
//! applied strips) against the *live* registry and to decide whether a strip
//! needs a filter-chain (only when at least one *applicable* effect is active
//! **and** its plugins are installed — mirroring Python `_has_active_fx` +
//! `_build_fx_graph` returning an empty string).

use std::collections::{HashMap, HashSet};

use crate::fx::{self, PluginSet};
use crate::model::{EffectKey, Strip, StripType};

/// Prefix of every virtual **bus** node — the handle other programs target.
pub const BUS_PREFIX: &str = "Holaf_Bus_";

/// Prefix of the null-sinks created by older builds (and by the legacy Python
/// engine), `Holaf_Strip_<uid>`.  Never created any more; swept on sight.
pub const LEGACY_STRIP_PREFIX: &str = "Holaf_Strip_";

/// Registry property carrying the uid of the strip a bus node belongs to.
///
/// **Informational**: it is visible in `pw-dump` / `pw-cli info <node>`, which
/// makes an inspection readable, but it is *not* what the reconciler uses to
/// recognise its nodes — PipeWire only exports a fixed key subset in the
/// registry globals a client sees (see the module docs).  Ownership is decided
/// by [`NodeNames`].
pub const UID_PROP: &str = "holaf.uid";

/// Registry property tagging what a node of ours is (see [`KIND_BUS`]).
pub const KIND_PROP: &str = "holaf.kind";

/// [`KIND_PROP`] value of a strip's virtual bus node.
pub const KIND_BUS: &str = "bus";

/// Registry property carrying the user label of the strip a bus belongs to
/// (purely informational: it makes `pw-dump` / `pw-cli` output readable).
pub const LABEL_PROP: &str = "holaf.label";

/// `Holaf_Strip_<uid>` — the name older builds and the Python engine gave a
/// virtual strip's null-sink.
///
/// Kept **only** for cleanup: the reconciler destroys such nodes on sight so an
/// upgraded daemon never leaves a stale bus behind next to the new one.
pub fn legacy_strip_node_name(uid: &str) -> String {
    format!("{LEGACY_STRIP_PREFIX}{uid}")
}

/// Longest label slug we allow in a node name.
///
/// PipeWire truncates nothing, but a pathological label would produce a name
/// no human wants to type into `pactl` — and some tools reject very long object
/// names.
const MAX_SLUG_LEN: usize = 48;

/// Slug fallback when a label contains nothing usable.
const SLUG_FALLBACK: &str = "Bus";

/// Fold the accented Latin letters a French UI produces onto ASCII
/// (`Périphérique` → `Peripherique`), so a node name stays typeable.
fn fold_ascii(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'A',
        'ç' => 'c',
        'Ç' => 'C',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'È' | 'É' | 'Ê' | 'Ë' => 'E',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'Ì' | 'Í' | 'Î' | 'Ï' => 'I',
        'ñ' => 'n',
        'Ñ' => 'N',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => 'O',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'Ù' | 'Ú' | 'Û' | 'Ü' => 'U',
        'ý' | 'ÿ' => 'y',
        'ß' => 's',
        'œ' => 'o',
        'æ' => 'a',
        other => other,
    }
}

/// Slug of a label, safe in a PipeWire `node.name`: ASCII alphanumerics, `-`
/// and `_`, everything else folded to a single `_`.
///
/// The label's case is **kept** (`"Mix Out"` → `"Mix_Out"`): the name is a
/// user-facing handle, and `Holaf_Bus_Mix_Out` reads better than
/// `holaf_bus_mix_out` in `pactl list short sinks`.  A label with nothing
/// usable falls back to `"Bus"` (never an empty node name).
pub fn slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for c in label.chars() {
        let c = fold_ascii(c);
        if c.is_ascii_alphanumeric() || c == '-' {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    let cut: String = trimmed.chars().take(MAX_SLUG_LEN).collect();
    let cut = cut.trim_end_matches('_');
    if cut.is_empty() {
        SLUG_FALLBACK.to_string()
    } else {
        cut.to_string()
    }
}

/// Public node name of a strip's bus: `Holaf_Bus_<slug>` for `ordinal == 1`,
/// `Holaf_Bus_<slug>_<ordinal>` otherwise (collision suffix, see
/// [`NodeNames::plan`]).
pub fn bus_node_name(label: &str, ordinal: usize) -> String {
    let slug = slug(label);
    if ordinal <= 1 {
        format!("{BUS_PREFIX}{slug}")
    } else {
        format!("{BUS_PREFIX}{slug}_{ordinal}")
    }
}

/// Whether `name` is the bus name of `label` (with or without a collision
/// suffix) — used to decide that a strip may keep its current node name.
fn is_bus_name_of(name: &str, label: &str) -> bool {
    let base = bus_node_name(label, 1);
    match name.strip_prefix(&base) {
        None => false,
        Some(rest) => {
            rest.is_empty()
                || (rest.starts_with('_')
                    && rest[1..].chars().next().map(|c| c != '0').unwrap_or(false)
                    && rest[1..].chars().all(|c| c.is_ascii_digit()))
        }
    }
}

/// The immutable naming plan of one snapshot: `uid → bus node name`.
///
/// Built once per `apply_snapshot` (see [`NodeNames::plan`]) and then used for
/// *every* name the reconciler derives — master node, meter source, route
/// endpoints, volume target.  One plan for the whole snapshot is what makes
/// the volume push, the FX chain anchor, the routes and the metering agree on
/// the same name (they used to recompute it independently).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeNames {
    names: HashMap<String, String>,
}

impl NodeNames {
    /// Assign a bus name to every strip.
    ///
    /// Two rules, in this order:
    ///
    /// 1. **Sticky** — a strip whose current name still slugs to the same base
    ///    as its (new) label keeps that exact name, suffix included.  Deleting
    ///    one of two same-label strips therefore never renames the survivor
    ///    (a rename would destroy and recreate the node, cutting the audio of
    ///    every program attached to it).
    /// 2. **First free** — every other strip gets `Holaf_Bus_<slug>`, then
    ///    `_2`, `_3`, … in snapshot order, so the assignment is deterministic
    ///    for a fresh start (the names a user sees after restarting the app are
    ///    the same as before).
    pub fn plan<'a, I>(strips: I, previous: &NodeNames) -> NodeNames
    where
        I: IntoIterator<Item = &'a Strip>,
    {
        let strips: Vec<&Strip> = strips.into_iter().collect();
        let mut names: HashMap<String, String> = HashMap::new();
        let mut used: HashSet<String> = HashSet::new();

        for strip in &strips {
            if strip.uid.is_empty() {
                continue;
            }
            let Some(previous_name) = previous.names.get(&strip.uid) else {
                continue;
            };
            if is_bus_name_of(previous_name, &strip.label) && !used.contains(previous_name) {
                used.insert(previous_name.clone());
                names.insert(strip.uid.clone(), previous_name.clone());
            }
        }

        for strip in &strips {
            if strip.uid.is_empty() || names.contains_key(&strip.uid) {
                continue;
            }
            let mut ordinal = 1usize;
            loop {
                let candidate = bus_node_name(&strip.label, ordinal);
                if !used.contains(&candidate) {
                    used.insert(candidate.clone());
                    names.insert(strip.uid.clone(), candidate);
                    break;
                }
                ordinal += 1;
            }
        }

        NodeNames { names }
    }

    /// Planned name of a strip, `None` when the plan does not know it.
    pub fn get(&self, uid: &str) -> Option<&str> {
        self.names.get(uid).map(String::as_str)
    }

    /// Planned name of a strip, falling back to the suffix-free name of its
    /// label (a strip created outside a snapshot — e.g. by the fake backend —
    /// still gets a predictable name instead of an empty one).
    pub fn bus_name(&self, strip: &Strip) -> String {
        match self.get(&strip.uid) {
            Some(name) => name.to_string(),
            None => bus_node_name(&strip.label, 1),
        }
    }

    /// The node a strip's audio crosses — see [`master_node_name`].
    pub fn master_node(&self, strip: &Strip) -> String {
        master_node_name(strip, &self.bus_name(strip))
    }

    /// The source the VU meters record — see [`meter_source_name`].
    pub fn meter_source(&self, strip: &Strip) -> String {
        meter_source_name(strip, &self.bus_name(strip))
    }

    /// Route source candidates — see [`route_source_candidates`].
    pub fn route_source_candidates(&self, strip: &Strip) -> Vec<String> {
        route_source_candidates(strip, &self.bus_name(strip))
    }

    /// Route target candidates — see [`route_target_candidates`].
    pub fn route_target_candidates(&self, strip: &Strip) -> Vec<String> {
        route_target_candidates(strip, &self.bus_name(strip))
    }
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

/// The single node a strip's audio crosses — its **master node**: the physical
/// device for a physical strip, the strip's bus (`Holaf_Bus_<Label>`) otherwise.
///
/// `bus` is the strip's planned bus name ([`NodeNames::bus_name`]) and is
/// ignored for a physical strip.
///
/// Three call sites derive it and they must never diverge:
///
/// 1. volume / mute are pushed to it (`Props.channelVolumes`);
/// 2. an FX chain is **anchored** to it — an output chain's playback stream
///    drives it (`target.object`), an input chain's capture stream reads it;
/// 3. it is the endpoint of the *intra-strip* link
///    (`output.Holaf_FX_<uid>` → master / master → `input.Holaf_FX_<uid>`).
///
/// It is a pure function of the strip (plus the snapshot's naming plan) on
/// purpose: the reconciler must be able to resolve it *before* the strip has
/// been published in its own state map — that lookup was empty during the first
/// `apply_snapshot`, so no strip's volume was ever applied at startup.
pub fn master_node_name(strip: &Strip, bus: &str) -> String {
    match strip.device_name.as_deref().filter(|d| !d.is_empty()) {
        Some(device) => device.to_string(),
        None => bus.to_string(),
    }
}

/// PulseAudio/PipeWire name of a sink node's monitor stream, `<node>.monitor`.
///
/// The `pipewire-pulse` server synthesises one monitor source per sink under
/// that exact name (verified live: `pactl list short sources` shows
/// `<bus>.monitor` for every null-sink the daemon creates), so the
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
/// | input, virtual | `<bus>.monitor` (apps play into that null-sink) |
/// | output, physical | `<device_name>.monitor` |
/// | output, virtual | `<bus>.monitor` |
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
pub fn meter_source_name(strip: &Strip, bus: &str) -> String {
    match (strip.kind, strip.device_name.as_deref().filter(|d| !d.is_empty())) {
        // Physical input: PipeWire exposes the device node itself as a
        // recordable source (same convention as the Python engine).
        (StripType::Input, Some(device)) => device.to_string(),
        (_, Some(device)) => monitor_source_name(device),
        (_, None) => monitor_source_name(bus),
    }
}

/// Every node name a strip can present as the **source** end of a route.
///
/// A route between two strips is a link between one of the source's output
/// nodes and one of the target's input nodes — and *which* node depends on the
/// FX chain state: a virtual strip is its bus (its monitor ports) without a
/// chain and `output.Holaf_FX_<uid>` with one.  A chain created or
/// unloaded while the route stays would otherwise leave the previous link
/// behind: the signal would then take **both** paths (doubled level, comb
/// filtering).  Callers therefore always tear down every candidate pair
/// before (re-)creating the link they want.
pub fn route_source_candidates(strip: &Strip, bus: &str) -> Vec<String> {
    let raw = match strip.device_name.as_deref().filter(|d| !d.is_empty()) {
        Some(device) => device.to_string(),
        None => bus.to_string(),
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
pub fn route_target_candidates(strip: &Strip, bus: &str) -> Vec<String> {
    let raw = match strip.device_name.as_deref().filter(|d| !d.is_empty()) {
        Some(device) => device.to_string(),
        None => bus.to_string(),
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
    fn fx_names_keep_the_uid_and_bus_names_follow_the_label() {
        // Internal names (never targeted from outside) keep the uid.
        assert_eq!(legacy_strip_node_name("abc"), "Holaf_Strip_abc");
        assert_eq!(fx_node_name("abc"), "Holaf_FX_abc");
        assert_eq!(fx_input_node_name("abc"), "input.Holaf_FX_abc");
        assert_eq!(fx_output_node_name("abc"), "output.Holaf_FX_abc");
        // The public bus name is derived from the label, not the uid.
        assert_eq!(bus_node_name("Mix Out", 1), "Holaf_Bus_Mix_Out");
        assert_eq!(bus_node_name("Mix Out", 2), "Holaf_Bus_Mix_Out_2");
        assert_eq!(slug("Casque USB"), "Casque_USB");
        assert_eq!(slug("Périphérique 1"), "Peripherique_1");
        assert_eq!(slug("  spaced   out  "), "spaced_out");
        assert_eq!(slug("a/b:c"), "a_b_c");
        assert_eq!(slug("line-1"), "line-1", "dashes stay (pactl-friendly)");
        assert_eq!(slug("!!!"), "Bus", "a hopeless label never yields ''");
        assert_eq!(slug(""), "Bus");
        assert!(slug(&"x".repeat(200)).len() <= 48);
    }

    /// Two strips with the same label must never fight over one node name, and
    /// the suffix must be deterministic (snapshot order) **and** sticky: the
    /// survivor of a deletion keeps its `_2` instead of being renamed (a rename
    /// destroys/recreates the node and cuts every program attached to it).
    #[test]
    fn naming_plan_is_unique_deterministic_and_sticky() {
        let a = Strip::with_uid("a", "Mix Out", StripType::Output);
        let b = Strip::with_uid("b", "Mix Out", StripType::Output);
        let c = Strip::with_uid("c", "Mix Out", StripType::Output);
        let d = Strip::with_uid("d", "Casque", StripType::Output);

        let plan = NodeNames::plan([&a, &b, &c, &d], &NodeNames::default());
        assert_eq!(plan.get("a"), Some("Holaf_Bus_Mix_Out"));
        assert_eq!(plan.get("b"), Some("Holaf_Bus_Mix_Out_2"));
        assert_eq!(plan.get("c"), Some("Holaf_Bus_Mix_Out_3"));
        assert_eq!(plan.get("d"), Some("Holaf_Bus_Casque"));

        // Same input, same plan (restarting the app yields the same names).
        let again = NodeNames::plan([&a, &b, &c, &d], &NodeNames::default());
        assert_eq!(plan, again);

        // Stickiness: drop the first of the three same-label strips — the
        // survivors keep the names they were published under.
        let after = NodeNames::plan([&b, &c, &d], &plan);
        assert_eq!(after.get("b"), Some("Holaf_Bus_Mix_Out_2"));
        assert_eq!(after.get("c"), Some("Holaf_Bus_Mix_Out_3"));
        assert_eq!(after.get("a"), None, "a removed strip holds no name");

        // A fresh strip reusing a free slot takes it (no unbounded growth).
        let e = Strip::with_uid("e", "Mix Out", StripType::Output);
        let with_new = NodeNames::plan([&b, &c, &e], &after);
        assert_eq!(with_new.get("e"), Some("Holaf_Bus_Mix_Out"));

        // A label change re-slugs → the strip gets a brand-new name (the
        // reconciler destroys the old node and creates the new one).
        let mut renamed = b.clone();
        renamed.label = "Mix In".into();
        let relabelled = NodeNames::plan([&renamed, &c], &after);
        assert_eq!(relabelled.get("b"), Some("Holaf_Bus_Mix_In"));
        assert_eq!(relabelled.get("c"), Some("Holaf_Bus_Mix_Out_3"));

        // Pure whitespace/punctuation edits do not move the bus: the slug is
        // unchanged, so the node keeps its name (no audio cut for a cosmetic
        // edit).  The label's *case* is part of the name, by design: what the
        // user reads in `pactl` is what the UI shows.
        let mut respaced = d.clone();
        respaced.label = " Casque  ".into();
        let respaced_plan = NodeNames::plan([&respaced], &plan);
        assert_eq!(respaced_plan.get("d"), Some("Holaf_Bus_Casque"));
        let mut recased = d.clone();
        recased.label = "CASQUE".into();
        assert_eq!(
            NodeNames::plan([&recased], &plan).get("d"),
            Some("Holaf_Bus_CASQUE")
        );
    }

    #[test]
    fn plan_names_drive_master_meter_and_routes() {
        let mut out = Strip::with_uid("o1", "Mix Out", StripType::Output);
        let plan = NodeNames::plan([&out], &NodeNames::default());
        assert_eq!(plan.master_node(&out), "Holaf_Bus_Mix_Out");
        assert_eq!(plan.meter_source(&out), "Holaf_Bus_Mix_Out.monitor");
        assert_eq!(
            plan.route_source_candidates(&out),
            vec!["Holaf_Bus_Mix_Out", "output.Holaf_FX_o1"]
        );
        assert_eq!(
            plan.route_target_candidates(&out),
            vec!["Holaf_Bus_Mix_Out", "input.Holaf_FX_o1"]
        );
        // The plan is irrelevant for a physical strip's master node.
        out.device_name = Some("alsa_output.usb-dac".into());
        assert_eq!(plan.master_node(&out), "alsa_output.usb-dac");
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
            route_source_candidates(&virt_in, &bus_node_name(&virt_in.label, 1)),
            vec!["Holaf_Bus_Desktop", "output.Holaf_FX_vi"]
        );
        assert_eq!(
            route_target_candidates(&virt_in, &bus_node_name(&virt_in.label, 1)),
            vec!["Holaf_Bus_Desktop", "input.Holaf_FX_vi"]
        );
        // Physical strips address the device node in both states.
        let mut mic = Strip::with_uid("pi", "Mic", StripType::Input);
        mic.device_name = Some("alsa_input.usb-mic".into());
        assert_eq!(
            route_source_candidates(&mic, &bus_node_name(&mic.label, 1)),
            vec!["alsa_input.usb-mic", "output.Holaf_FX_pi"]
        );
        // Outputs mirror the same pair (raw sink vs chain capture sink).
        let out = Strip::with_uid("vo", "Out", StripType::Output);
        assert_eq!(
            route_target_candidates(&out, &bus_node_name(&out.label, 1)),
            vec!["Holaf_Bus_Out", "input.Holaf_FX_vo"]
        );
    }

    #[test]
    fn master_node_name_is_device_or_strip_sink() {
        // Virtual strips own a null-sink.
        let virt = Strip::with_uid("v1", "Web", StripType::Input);
        assert_eq!(
            master_node_name(&virt, &bus_node_name(&virt.label, 1)),
            "Holaf_Bus_Web"
        );
        // Physical strips address the device node in both kinds (an output
        // chain drives the device, an input chain reads it).
        let mut phys_out = Strip::with_uid("p1", "Casque", StripType::Output);
        phys_out.device_name = Some("alsa_output.usb-headset".into());
        assert_eq!(
            master_node_name(&phys_out, &bus_node_name(&phys_out.label, 1)),
            "alsa_output.usb-headset"
        );
        let mut phys_in = Strip::with_uid("p2", "Mic", StripType::Input);
        phys_in.device_name = Some("alsa_input.usb-mic".into());
        assert_eq!(
            master_node_name(&phys_in, &bus_node_name(&phys_in.label, 1)),
            "alsa_input.usb-mic"
        );
        // An empty device_name means virtual (same convention as is_physical)
        // — never an empty node name.
        let mut blank = Strip::with_uid("b1", "B", StripType::Output);
        blank.device_name = Some(String::new());
        assert_eq!(
            master_node_name(&blank, &bus_node_name(&blank.label, 1)),
            "Holaf_Bus_B"
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

        let bus = |s: &Strip| bus_node_name(&s.label, 1);

        // Physical input → the device source itself.
        assert_eq!(
            meter_source_name(&phys_in, &bus(&phys_in)).as_str(),
            "alsa_input.usb-mic.analog-stereo"
        );
        // Virtual input → the bus monitor (apps play into that null-sink).
        assert_eq!(
            meter_source_name(&virt_in, &bus(&virt_in)).as_str(),
            "Holaf_Bus_Desktop.monitor"
        );
        // Outputs → the monitor of the strip's master node.
        assert_eq!(
            meter_source_name(&phys_out, &bus(&phys_out)).as_str(),
            "alsa_output.pci.analog-stereo.monitor"
        );
        assert_eq!(
            meter_source_name(&virt_out, &bus(&virt_out)).as_str(),
            "Holaf_Bus_Virtual.monitor"
        );
        // The mapping is independent of the FX chain: for outputs the chain
        // plays into the very same node, and for inputs the bar shows the
        // signal on the strip rather than the (passive, unroutable) chain
        // source.
        let mut in_with_fx = virt_in.clone();
        in_with_fx.effects.get_mut(&EffectKey::Eq).unwrap().active = true;
        assert_eq!(
            meter_source_name(&in_with_fx, &bus(&in_with_fx)),
            meter_source_name(&virt_in, &bus(&virt_in))
        );
        // An empty device_name is virtual (same convention as is_physical).
        let mut blank = Strip::with_uid("b", "B", StripType::Output);
        blank.device_name = Some(String::new());
        assert_eq!(
            meter_source_name(&blank, &bus(&blank)),
            "Holaf_Bus_B.monitor"
        );
    }

}
