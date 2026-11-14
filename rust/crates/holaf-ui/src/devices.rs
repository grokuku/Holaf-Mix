//! Device identity: which entry of `device_list` a strip's `device_name`
//! refers to — and what to do when the exact name is gone.
//!
//! # The problem
//!
//! A strip stores `device_name`, i.e. PipeWire's `node.name`:
//!
//! ```text
//! alsa_output.pci-0000_00_1f.3.analog-stereo
//! bluez_output.AC_80_0B_1F_22_33_44.a2dp-sink
//! ```
//!
//! Only the **profile** suffix (`analog-stereo`, `a2dp-sink`) and the vendor
//! head (`alsa_output`, `bluez_output`) are stable.  The middle segment is the
//! card/instance identity and it changes when the machine reboots with a
//! different PCI address or when the device is re-plugged (`usb-…`).  The Qt UI
//! simply showed `<name> (Not Found)` and left the strip dead until the user
//! re-selected the device; the Rust UI tries harder:
//!
//! | rank | rule | `MatchKind` |
//! |---|---|---|
//! | 1 | `device.name == device_name` | [`MatchKind::Name`] |
//! | 2 | name equal ignoring case | [`MatchKind::Name`] |
//! | 3 | `device.description` equals the stored **description** (the strip was bound by description) | [`MatchKind::Description`] |
//! | 4 | best scored candidate — vendor head, profile suffix, identity segment — unique and above [`MIN_SCORE`] | [`MatchKind::Approximate`] |
//! | 5 | nothing | (no match → the UI shows `(Not Found)`) |
//!
//! Rule 4 refuses to guess: two equally good candidates mean "no match", never
//! "pick the first one" (routing a strip to the wrong speakers is worse than
//! showing it as missing).
//!
//! Everything here is pure — `&[Strip]` + `&[DeviceInfo]` in, repairs out — so
//! the whole matching policy is unit-tested without PipeWire.

use holaf_core::model::{Strip, StripType};

use crate::engine_json::DeviceInfo;

/// Smallest score an [`MatchKind::Approximate`] match may have.
///
/// Vendor head (2) + profile suffix (2) is the minimum meaningful resemblance:
/// `alsa_output.X.analog-stereo` → `alsa_output.Y.analog-stereo`.
pub const MIN_SCORE: u32 = 4;

/// How a device was matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// The stored name is still the device's `node.name`.
    Name,
    /// Matched through the engine's `node.description` (identity survived an
    /// instance-id change and the engine publishes descriptions).
    Description,
    /// Matched through name structure; `score` says how sure we are.
    Approximate { score: u32 },
}

impl MatchKind {
    /// Is this a match we should warn about?  (Anything that is not the exact
    /// stored name — the user's config is about to be rewritten.)
    pub fn is_approximate(self) -> bool {
        !matches!(self, MatchKind::Name)
    }
}

/// One resolved device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceMatch {
    /// `node.name` of the device that should be used from now on.
    pub name: String,
    pub kind: MatchKind,
}

/// Resolve `wanted` (a strip's `device_name`) against the engine's devices.
///
/// `description` is the *stored* description when the config kept one (it does
/// not today — [`crate::config`] preserves whatever keys it finds, so a future
/// binding will carry it); pass `None` when unknown, and rule 3 is skipped.
pub fn resolve(
    wanted: &str,
    description: Option<&str>,
    devices: &[DeviceInfo],
) -> Option<DeviceMatch> {
    let wanted = wanted.trim();
    if wanted.is_empty() || devices.is_empty() {
        return None;
    }

    // 1./2. the name is still valid (exact, then case-insensitive: PipeWire
    // names are lowercase but a hand-edited config need not be).
    if let Some(device) = devices.iter().find(|device| device.name == wanted) {
        return Some(DeviceMatch {
            name: device.name.clone(),
            kind: MatchKind::Name,
        });
    }
    let wanted_lower = wanted.to_lowercase();
    if let Some(device) = devices
        .iter()
        .find(|device| device.name.to_lowercase() == wanted_lower)
    {
        return Some(DeviceMatch {
            name: device.name.clone(),
            kind: MatchKind::Name,
        });
    }

    // 3. the strip was bound by description: the description is the stable
    // half of the identity, so an exact description hit is a *good* match.
    if let Some(description) = description.map(str::trim).filter(|d| !d.is_empty()) {
        let description_lower = description.to_lowercase();
        if let Some(device) = devices.iter().find(|device| {
            device
                .description
                .as_deref()
                .is_some_and(|candidate| candidate.trim().to_lowercase() == description_lower)
        }) {
            return Some(DeviceMatch {
                name: device.name.clone(),
                kind: MatchKind::Description,
            });
        }
    }

    // 3b. the engine publishes a description that *is* the stored string (a
    // config written by a tool that binds by description, e.g. the Python UI
    // when `node.name` is missing).
    if let Some(device) = devices.iter().find(|device| {
        device
            .description
            .as_deref()
            .is_some_and(|candidate| candidate.trim().to_lowercase() == wanted_lower)
    }) {
        return Some(DeviceMatch {
            name: device.name.clone(),
            kind: MatchKind::Description,
        });
    }

    // 4. structural match, refusing to guess on a tie.
    let mut best: Option<(u32, &DeviceInfo)> = None;
    let mut tie = false;
    for device in devices {
        let score = name_score(wanted, &device.name);
        if score < MIN_SCORE {
            continue;
        }
        match best {
            Some((best_score, _)) if score == best_score => tie = true,
            Some((best_score, _)) if score < best_score => {}
            _ => {
                best = Some((score, device));
                tie = false;
            }
        }
    }
    match best {
        Some((score, device)) if !tie => Some(DeviceMatch {
            name: device.name.clone(),
            kind: MatchKind::Approximate { score },
        }),
        _ => None,
    }
}

/// Structural similarity of two PipeWire node names (0 = nothing in common).
///
/// * equal vendor head (`alsa_output`) → +2;
/// * equal profile suffix (`analog-stereo`) → +2;
/// * equal identity segment (`pci-0000_00_1f.3`, `usb-Kingston_…`) → +3;
/// * every other segment in common → +1 (capped at +2, so a long shared
///   prefix cannot outvote the identity).
pub fn name_score(a: &str, b: &str) -> u32 {
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();
    if a_lower == b_lower {
        return u32::MAX; // handled by the exact rules; keep this total
    }
    let left = split(&a_lower);
    let right = split(&b_lower);
    if left.is_empty() || right.is_empty() {
        return 0;
    }
    let mut score = 0;
    if left[0] == right[0] {
        score += 2;
    }
    if left.len() > 1 && right.len() > 1 && left[left.len() - 1] == right[right.len() - 1] {
        score += 2;
    }
    if let (Some(a_id), Some(b_id)) = (identity_segment(&left), identity_segment(&right)) {
        if a_id == b_id {
            score += 3;
        }
    }
    let shared = left
        .iter()
        .filter(|segment| right.iter().any(|other| other == *segment))
        .count()
        .saturating_sub(2);
    score + (shared as u32).min(2)
}

fn split(name: &str) -> Vec<&str> {
    name.split('.').filter(|part| !part.is_empty()).collect()
}

/// The segment that names the *instance* of a device, when there is one:
/// `pci-…`, `usb-…`, `platform-…`, `bluez` MACs, `AC_…`.
fn identity_segment<'a>(segments: &[&'a str]) -> Option<&'a str> {
    if segments.len() < 2 {
        return None;
    }
    let candidate = segments[1];
    let looks_like_instance = candidate.starts_with("pci-")
        || candidate.starts_with("usb-")
        || candidate.starts_with("platform-")
        || candidate.starts_with("AC_")
        || candidate.contains('-')
        || candidate.contains('_');
    looks_like_instance.then_some(candidate)
}

/// A device binding the UI could not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRepair {
    pub uid: String,
    pub label: String,
    /// What the strip stored.
    pub declared: String,
    /// What it should store now.
    pub resolved: String,
    pub kind: MatchKind,
}

/// Re-bind every strip whose `device_name` still designates a device, and
/// report the ones whose device is gone.
///
/// Returns `(repairs, missing)`:
///
/// * `repairs` — strips that should adopt a new `node.name` (the resolved one);
/// * `missing` — `(uid, label, declared)` for strips whose device matches
///   nothing (they keep their `device_name`, and the view shows
///   `(Not Found)` exactly like Qt did).
///
/// Only outputs are resolved against sinks and inputs against sources: a strip
/// bound to the wrong media class is "missing", never "approximate".
pub fn reconcile(
    strips: &[Strip],
    devices: &[DeviceInfo],
) -> (Vec<DeviceRepair>, Vec<(String, String, String)>) {
    let mut repairs = Vec::new();
    let mut missing = Vec::new();
    if devices.is_empty() {
        // No authoritative list: every binding is "unknown", not "missing".
        return (repairs, missing);
    }
    for strip in strips {
        let Some(declared) = strip.device_name.as_deref() else {
            continue; // a virtual bus has nothing to resolve
        };
        let declared = declared.trim();
        if declared.is_empty() {
            continue;
        }
        let candidates: Vec<DeviceInfo> = devices
            .iter()
            .filter(|device| match strip.kind {
                StripType::Input => device.is_source(),
                StripType::Output => device.is_sink(),
            })
            .cloned()
            .collect();
        match resolve(declared, None, &candidates) {
            Some(found) if found.name != declared => repairs.push(DeviceRepair {
                uid: strip.uid.clone(),
                label: strip.label.clone(),
                declared: declared.to_string(),
                resolved: found.name,
                kind: found.kind,
            }),
            Some(_) => {}
            None => missing.push((strip.uid.clone(), strip.label.clone(), declared.to_string())),
        }
    }
    (repairs, missing)
}

/// What the strip's device combo must show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePresence {
    /// No authoritative device list yet (no engine, or none received): show the
    /// stored name, never a false `(Not Found)`.
    Unknown,
    /// The stored device exists (possibly after a repair).
    Found,
    /// The stored device is gone: `(Not Found)`.
    Missing,
}

/// [`DevicePresence`] for one strip — what [`crate::views::strip`] paints.
///
/// `devices` empty ⇒ [`DevicePresence::Unknown`]: the snapshot binary and a
/// disconnected engine must not label every strip as broken.
pub fn presence(strip: &Strip, devices: &[DeviceInfo]) -> DevicePresence {
    let Some(declared) = strip.device_name.as_deref() else {
        return DevicePresence::Found; // a virtual bus is not "missing"
    };
    if devices.is_empty() {
        return DevicePresence::Unknown;
    }
    let candidates = devices.iter().filter(|device| match strip.kind {
        StripType::Input => device.is_source(),
        StripType::Output => device.is_sink(),
    });
    for device in candidates {
        if device.name == declared {
            return DevicePresence::Found;
        }
    }
    DevicePresence::Missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use holaf_core::model::StripType;

    fn device(name: &str, description: Option<&str>) -> DeviceInfo {
        DeviceInfo {
            id: 1,
            name: name.to_string(),
            media_class: "Audio/Sink".to_string(),
            description: description.map(str::to_string),
        }
    }

    fn output(uid: &str, device_name: Option<&str>) -> Strip {
        let mut strip = Strip::with_uid(uid, uid.to_uppercase(), StripType::Output);
        strip.device_name = device_name.map(str::to_string);
        strip
    }

    #[test]
    fn an_unchanged_name_is_the_only_non_approximate_match() {
        let devices = [device("alsa_output.pci-0000_00_1f.3.analog-stereo", None)];
        let found = resolve("alsa_output.pci-0000_00_1f.3.analog-stereo", None, &devices)
            .expect("exact match");
        assert_eq!(found.kind, MatchKind::Name);
        assert!(!found.kind.is_approximate());
        // Case-insensitive name equality is still a name match.
        let found = resolve("ALSA_OUTPUT.PCI-0000_00_1F.3.ANALOG-STEREO", None, &devices)
            .expect("case-insensitive match");
        assert_eq!(found.kind, MatchKind::Name);
    }

    #[test]
    fn a_renamed_node_is_found_through_its_description() {
        let devices = [
            device(
                "alsa_output.pci-0000_00_1f.3.analog-stereo",
                Some("Built-in Audio Analog Stereo"),
            ),
            device("bluez_output.AC_80_0B.1.a2dp-sink", Some("WH-1000XM4")),
        ];
        // The strip was bound by description (a config that stores one).
        let found = resolve("Built-in Audio Analog Stereo", None, &devices).expect("description");
        assert_eq!(found.name, "alsa_output.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(found.kind, MatchKind::Description);
        assert!(found.kind.is_approximate());
        // …and a *renamed* node is matched through the engine's description when
        // the caller knows the description the strip was bound with.
        let found = resolve(
            "alsa_output.pci-0000_99_99.9.analog-stereo",
            Some("Built-in Audio Analog Stereo"),
            &devices,
        )
        .expect("description match");
        assert_eq!(found.name, "alsa_output.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(found.kind, MatchKind::Description);
    }

    #[test]
    fn a_changed_instance_id_is_matched_structurally_with_a_score() {
        let devices = [device("alsa_output.usb-Kingston_2.analog-stereo", None)];
        // Same vendor head and same profile, different instance: score 2 + 2.
        let found = resolve("alsa_output.pci-0000_00_1f.3.analog-stereo", None, &devices)
            .expect("profile match");
        assert_eq!(found.kind, MatchKind::Approximate { score: 4 });
        // An identity segment that survives scores higher (2 + 2 + 3).
        let devices = [device("alsa_output.usb-Kingston_2.analog-stereo", None)];
        let found = resolve("alsa_output.usb-Kingston_1.analog-stereo", None, &devices).unwrap();
        assert_eq!(found.kind, MatchKind::Approximate { score: 4 });
    }

    #[test]
    fn two_equally_good_candidates_are_a_miss_not_a_guess() {
        let devices = [
            device("alsa_output.pci-a.analog-stereo", None),
            device("alsa_output.pci-b.analog-stereo", None),
        ];
        assert_eq!(
            resolve("alsa_output.pci-c.analog-stereo", None, &devices),
            None,
            "two candidate devices with the same vendor+profile must not be guessed"
        );
        // With a unique best candidate the match is taken.
        let devices = [
            device("alsa_output.pci-a.analog-stereo", None),
            device("bluez_output.AC.1.a2dp-sink", None),
        ];
        let found = resolve("alsa_output.pci-z.analog-stereo", None, &devices).unwrap();
        assert_eq!(found.name, "alsa_output.pci-a.analog-stereo");
    }

    #[test]
    fn an_unrelated_name_never_matches() {
        let devices = [device("alsa_output.pci-a.analog-stereo", None)];
        assert_eq!(resolve("bluez_output.AA.a2dp-sink", None, &devices), None);
        assert_eq!(resolve("", None, &devices), None);
        assert_eq!(resolve("alsa_output.pci-a.analog-stereo", None, &[]), None);
    }

    #[test]
    fn reconcile_repairs_a_renamed_device_and_reports_a_missing_one() {
        let devices = [
            device(
                "alsa_output.pci-0000_00_1f.3.analog-stereo",
                Some("Built-in Audio"),
            ),
            device("bluez_output.AC_80.1.a2dp-sink", Some("Headset")),
        ];
        let strips = [
            output("out-a", Some("alsa_output.usb-Old.analog-stereo")),
            output("out-b", Some("bluez_output.AC_80.1.a2dp-sink")),
            output("out-c", Some("usb_headset_that_is_gone")),
            output("out-bus", None),
        ];
        let (repairs, missing) = reconcile(&strips, &devices);
        // `out-a` shares the vendor head + profile with the built-in analog
        // device (and nothing else does), so it is repaired…
        assert_eq!(repairs.len(), 1, "{repairs:?}");
        assert_eq!(repairs[0].uid, "out-a");
        assert_eq!(
            repairs[0].resolved,
            "alsa_output.pci-0000_00_1f.3.analog-stereo"
        );
        assert!(repairs[0].kind.is_approximate());
        // …`out-b` is untouched, `out-c` is declared missing, the bus is neither.
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert_eq!(missing[0].0, "out-c");
        assert!(repairs.iter().all(|repair| repair.uid != "out-bus"));
    }

    #[test]
    fn presence_never_cries_wolf_without_a_device_list() {
        let strips = [output("out-a", Some("alsa_output.gone.analog-stereo"))];
        assert_eq!(presence(&strips[0], &[]), DevicePresence::Unknown);
        let devices = [device("alsa_output.here.analog-stereo", None)];
        assert_eq!(presence(&strips[0], &devices), DevicePresence::Missing);
        let mut strip = strips[0].clone();
        strip.device_name = Some("alsa_output.here.analog-stereo".to_string());
        assert_eq!(presence(&strip, &devices), DevicePresence::Found);
        // A virtual bus is always fine.
        let bus = output("out-bus", None);
        assert_eq!(presence(&bus, &devices), DevicePresence::Found);
    }

    #[test]
    fn a_source_is_never_matched_against_a_sink() {
        let mut devices = [device("alsa_input.pci.analog-stereo", None)];
        devices[0].media_class = "Audio/Sink".to_string();
        let mut input = Strip::with_uid("in-a", "In", StripType::Input);
        input.device_name = Some("alsa_input.pci.analog-stereo".to_string());
        let (repairs, missing) = reconcile(&[input.clone()], &devices);
        assert!(repairs.is_empty());
        assert_eq!(missing.len(), 1, "a sink must not satisfy an input strip");
        // The same device declared as a source resolves it.
        devices[0].media_class = "Audio/Source".to_string();
        let (repairs, missing) = reconcile(&[input], &devices);
        assert!(repairs.is_empty() && missing.is_empty());
    }
}
