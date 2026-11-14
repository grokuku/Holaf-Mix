//! Pure resolution: strip descriptor + theme → the exact style to paint.
//!
//! This module exists so the interesting part of theming (which value wins:
//! per-strip override or per-kind default?) is a **pure function** unit-tested
//! without an `egui::Context`.  Painting code (`crate::views::strip`) calls
//! [`resolve`] once per strip per frame and never re-implements the precedence
//! rules.
//!
//! Precedence, highest first:
//!
//! 1. [`StripOverride`](crate::theme::StripOverride) for this `uid`;
//! 2. the per-kind style ([`StripKindStyles`](crate::theme::StripKindStyles));
//! 3. the built-in default ([`Theme::holaf_dark`](crate::theme::Theme::holaf_dark)).
//!
//! The kind itself is derived: an *output* strip whose `device_name` is
//! `None` (or empty) is a **virtual bus**, and gets the purple body — exactly
//! what `StripWidget._update_base_style` does in Python.
//!
//! A strip that is waiting for a MIDI control (`set_learning(true)`) is not a
//! fourth kind: [`StripDescriptor::learning`] swaps the kind style for
//! [`Theme::learning_style`](crate::theme::Theme::learning_style), so the frame
//! turns orange and the kind comes back untouched when the learn stops.

use holaf_core::model::{Strip, StripType};

use super::{Fill, HexColor, ImageSpec, StripOverride, ThemeFile};

/// The visual kind of a strip, after deriving `Bus` from the device state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StripKind {
    /// Captures from apps/a source (`StripType::Input`).
    Input,
    /// Plays to a physical device (`StripType::Output` with a device).
    Output,
    /// A virtual output, i.e. the strip's own sink/bus (`StripType::Output`
    /// without a device).  Purple in the PySide UI.
    Bus,
}

impl StripKind {
    /// Derive the visual kind from the model's declared kind + device.
    ///
    /// `Some("")` is normalised to "no device": an empty device name is what
    /// the Python combo box yields for its "Virtual Sink (Bus)" entry, so
    /// treating it as a physical device would paint a bus red.
    pub fn from_parts(declared: StripType, device_name: Option<&str>) -> Self {
        match declared {
            StripType::Input => StripKind::Input,
            StripType::Output => match device_name {
                Some(name) if !name.trim().is_empty() => StripKind::Output,
                _ => StripKind::Bus,
            },
        }
    }

    /// Derive from a core model strip.
    pub fn from_strip(strip: &Strip) -> Self {
        Self::from_parts(strip.kind, strip.device_name.as_deref())
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StripKind::Input => "input",
            StripKind::Output => "output",
            StripKind::Bus => "bus",
        }
    }

    /// The declared kind a theme author would write for this visual kind.
    pub fn declared(self) -> StripType {
        match self {
            StripKind::Input => StripType::Input,
            StripKind::Output | StripKind::Bus => StripType::Output,
        }
    }
}

impl std::fmt::Display for StripKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<StripKind> for StripType {
    fn from(kind: StripKind) -> Self {
        kind.declared()
    }
}

/// What the resolver needs to know about a strip.  A borrow-only view so the
/// resolver stays allocation-free and can be called inside the paint loop.
#[derive(Debug, Clone, Copy)]
pub struct StripDescriptor<'a> {
    /// Persistent strip id — the key of the per-strip overrides.
    pub uid: &'a str,
    /// Kind as stored in `config.json`.
    pub declared: StripType,
    /// PipeWire node name of the device; `None`/empty ⇒ virtual (bus).
    pub device_name: Option<&'a str>,
    /// `true` while the strip waits for a MIDI control (`set_learning(true)`):
    /// the body turns orange and the border turns orange, whatever the kind.
    pub learning: bool,
}

impl<'a> StripDescriptor<'a> {
    /// `declared` accepts either [`StripType`] or [`StripKind`] (a `Bus` is
    /// stored as an output without device, so the conversion is lossless for
    /// the resolver's purposes).
    pub fn new(uid: &'a str, declared: impl Into<StripType>, device_name: Option<&'a str>) -> Self {
        Self {
            uid,
            declared: declared.into(),
            device_name,
            learning: false,
        }
    }

    /// Borrow a core model strip.
    pub fn from_strip(strip: &'a Strip) -> Self {
        Self {
            uid: &strip.uid,
            declared: strip.kind,
            device_name: strip.device_name.as_deref(),
            learning: false,
        }
    }

    /// Same strip, in its MIDI-learning state (`StripWidget.set_learning`).
    pub fn with_learning(mut self, learning: bool) -> Self {
        self.learning = learning;
        self
    }

    /// The resolved visual kind.
    pub fn kind(&self) -> StripKind {
        StripKind::from_parts(self.declared, self.device_name)
    }
}

/// Everything the strip painter needs, precedence already applied.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedStripStyle {
    /// Resolved visual kind (input / physical output / bus).
    pub kind: StripKind,
    /// `true` when the strip is waiting for a MIDI control, so its body/border
    /// come from the *learning* style instead of the kind style.
    pub learning: bool,
    /// Body fill, with `opacity` already applied to its alpha.
    pub body: Fill,
    /// Border colour (never transparent).
    pub border: HexColor,
    /// Decorative image, if the theme (or an override) asks for one.
    pub image: Option<ImageSpec>,
    /// Accent used by this strip's active FX buttons and routing chips.
    pub accent: HexColor,
    /// Text colour on top of [`accent`](Self::accent).
    pub accent_text: HexColor,
    /// Main text colour inside the strip.
    pub text_primary: HexColor,
    /// Caption text colour ("SOURCE IN", "DEVICE OUT"…).
    pub text_secondary: HexColor,
    /// Effective body opacity, clamped to `0.0..=1.0`.
    pub opacity: f32,
    /// `true` when a per-strip override contributed at least one field.  Lets
    /// the UI (and tests) tell "themed strip" from "default strip".
    pub overridden: bool,
}

/// Resolve the style of one strip.  Pure: no I/O, no `egui`, no allocation.
pub fn resolve(file: &ThemeFile, strip: &StripDescriptor<'_>) -> ResolvedStripStyle {
    let theme = &file.theme;
    let kind = strip.kind();
    // While learning, Qt repaints the whole frame orange whatever the kind
    // (`_update_base_style`: `bg_color = "#f39c12"`, `border: 2px solid
    // #f39c12`) — the kind only comes back when the learn stops.
    let base = if strip.learning {
        theme.learning_style()
    } else {
        theme.kind_style(kind)
    };
    let colors = &theme.colors;
    let ov: Option<&StripOverride> = file.strip_override(strip.uid);

    // Opacity: per-strip override > theme-wide body_opacity > opaque.  Clamped
    // because `opacity_milli` is a u16 in JSON and a hand-written theme could
    // easily say 8200 instead of 820.
    let opacity = ov
        .and_then(StripOverride::opacity)
        .unwrap_or(theme.backgrounds.body_opacity)
        .clamp(0.0, 1.0);

    // Body: override replaces the whole fill (not a per-stop merge — a
    // half-overridden gradient is never what the author meant).
    let body = ov
        .and_then(|o| o.body)
        .unwrap_or(base.body)
        .scale_alpha(opacity);

    ResolvedStripStyle {
        kind,
        learning: strip.learning,
        body,
        border: ov.and_then(|o| o.border).unwrap_or(base.border),
        image: ov
            .and_then(|o| o.image.clone())
            .or_else(|| base.image.clone()),
        accent: ov.and_then(|o| o.accent).unwrap_or(colors.accent),
        accent_text: colors.accent_text,
        text_primary: ov
            .and_then(|o| o.text_primary)
            .unwrap_or(colors.text_primary),
        text_secondary: colors.text_secondary,
        opacity,
        overridden: ov.is_some_and(|o| {
            o.body.is_some()
                || o.border.is_some()
                || o.opacity_milli.is_some()
                || o.image.is_some()
                || o.accent.is_some()
                || o.text_primary.is_some()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Theme, ThemeFile};

    fn file() -> ThemeFile {
        ThemeFile::holaf_dark()
    }

    #[test]
    fn input_strip_without_override_uses_the_kind_style() {
        let f = file();
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert_eq!(style.kind, StripKind::Input);
        assert_eq!(style.body, f.theme.strips.input.body.scale_alpha(1.0));
        assert_eq!(style.border, HexColor::parse("#555555").unwrap());
        assert_eq!(style.accent, HexColor::parse("#2ecc71").unwrap());
        assert!(!style.overridden);
        assert_eq!(style.opacity, 1.0);
    }

    #[test]
    fn bus_is_detected_by_absent_device_name() {
        let f = file();
        let bus = resolve(&f, &StripDescriptor::new("u1", StripType::Output, None));
        assert_eq!(bus.kind, StripKind::Bus);
        assert_eq!(bus.body, f.theme.strips.bus.body.scale_alpha(1.0));

        // An empty device name is still "no device" (Python combo sentinel).
        let empty = resolve(&f, &StripDescriptor::new("u1", StripType::Output, Some("")));
        assert_eq!(empty.kind, StripKind::Bus);

        // A real device makes it a physical output.
        let phys = resolve(
            &f,
            &StripDescriptor::new("u1", StripType::Output, Some("alsa_output.usb")),
        );
        assert_eq!(phys.kind, StripKind::Output);
        assert_eq!(phys.body, f.theme.strips.output.body.scale_alpha(1.0));
    }

    #[test]
    fn bus_kind_is_accepted_as_a_descriptor_input() {
        let f = file();
        let style = resolve(&f, &StripDescriptor::new("u", StripKind::Bus, None));
        assert_eq!(style.kind, StripKind::Bus);
    }

    #[test]
    fn partial_override_only_replaces_the_fields_it_sets() {
        let mut f = file();
        let accent = HexColor::parse("#ff00ff").unwrap();
        f.strips.insert(
            "u1".to_string(),
            StripOverride {
                accent: Some(accent),
                ..StripOverride::default()
            },
        );
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert_eq!(style.accent, accent, "override wins");
        assert!(style.overridden);
        // The body and border still come from the kind style.
        assert_eq!(style.body, f.theme.strips.input.body.scale_alpha(1.0));
        assert_eq!(style.border, f.theme.strips.input.border);
    }

    #[test]
    fn override_body_replaces_the_gradient_without_merging_stops() {
        let mut f = file();
        let solid = Fill::solid(HexColor::parse("#101010").unwrap());
        f.strips.insert(
            "u1".to_string(),
            StripOverride {
                body: Some(solid),
                ..StripOverride::default()
            },
        );
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Output, None));
        assert!(!style.body.is_gradient());
        // Opacity is applied on top of the override body.
        assert_eq!(style.body, solid.scale_alpha(1.0));
        assert_eq!(
            style.kind,
            StripKind::Bus,
            "kind is unaffected by the override"
        );
    }

    #[test]
    fn override_falls_back_per_field() {
        let mut f = file();
        f.strips.insert(
            "u1".to_string(),
            StripOverride {
                border: Some(HexColor::rgb(1, 2, 3)),
                ..StripOverride::default()
            },
        );
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert_eq!(style.border, HexColor::rgb(1, 2, 3));
        assert_eq!(style.text_primary, f.theme.colors.text_primary);
        assert_eq!(style.accent_text, f.theme.colors.accent_text);
    }

    #[test]
    fn override_does_not_leak_to_other_strips() {
        let mut f = file();
        f.strips.insert(
            "u1".to_string(),
            StripOverride {
                body: Some(Fill::solid(HexColor::rgb(9, 9, 9))),
                ..StripOverride::default()
            },
        );
        let other = resolve(&f, &StripDescriptor::new("u2", StripType::Input, None));
        assert_eq!(other.body, f.theme.strips.input.body.scale_alpha(1.0));
        assert!(!other.overridden);
    }

    #[test]
    fn empty_override_entry_is_not_reported_as_overridden() {
        let mut f = file();
        f.strips.insert("u1".to_string(), StripOverride::default());
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert!(!style.overridden, "an empty override changes nothing");
    }

    #[test]
    fn body_opacity_is_applied_to_every_stop() {
        // The default is opaque (Qt strips are opaque); a theme that asks for
        // translucency gets it on both gradient stops.
        let mut f = file();
        f.theme.backgrounds.body_opacity = 0.5;
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert_eq!(style.body.at(0.0).a(), 128);
        assert_eq!(style.body.at(1.0).a(), 128);
        // Colours themselves are untouched by the opacity.
        assert_eq!(style.body.at(0.0).r(), 0x44);
        assert_eq!(
            style.body.at(1.0),
            HexColor::parse("#3daee9").unwrap().with_alpha(128)
        );
    }

    #[test]
    fn per_strip_opacity_overrides_the_global_one() {
        let mut f = file();
        f.theme.backgrounds.body_opacity = 0.4;
        f.strips
            .insert("u1".to_string(), StripOverride::default().with_opacity(1.0));
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert_eq!(style.opacity, 1.0);
        assert_eq!(style.body.at(0.0).a(), 255);
    }

    #[test]
    fn out_of_range_opacity_is_clamped() {
        let mut f = file();
        f.strips.insert(
            "u1".to_string(),
            StripOverride {
                // A hand-written 8200 (= 8.2) instead of 820.
                opacity_milli: Some(8200),
                ..StripOverride::default()
            },
        );
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert_eq!(style.opacity, 1.0);
        assert_eq!(style.body.at(0.0).a(), 255);
    }

    #[test]
    fn theme_wide_body_opacity_change_is_honoured() {
        let mut f = file();
        f.theme.backgrounds.body_opacity = 0.5;
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert_eq!(style.body.at(0.0).a(), 128);
    }

    #[test]
    fn image_comes_from_the_kind_then_the_override() {
        use crate::theme::{Fit, ImageSpec};
        let mut f = file();
        f.theme.strips.input.image = Some(ImageSpec::new("kind.png"));
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert_eq!(style.image.as_ref().unwrap().path, "kind.png");

        f.strips.insert(
            "u1".to_string(),
            StripOverride {
                image: Some(ImageSpec {
                    fit: Fit::Cover,
                    ..ImageSpec::new("strip.png")
                }),
                ..StripOverride::default()
            },
        );
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert_eq!(style.image.as_ref().unwrap().path, "strip.png");
        assert_eq!(style.image.as_ref().unwrap().fit, Fit::Cover);
    }

    #[test]
    fn resolve_from_a_core_strip_matches_manual_descriptor() {
        let mut strip = Strip::with_uid("uid-7", "Casque", StripType::Output);
        strip.device_name = Some("alsa_output.pci".to_string());
        let from_strip = resolve(&file(), &StripDescriptor::from_strip(&strip));
        let manual = resolve(
            &file(),
            &StripDescriptor::new("uid-7", StripType::Output, Some("alsa_output.pci")),
        );
        assert_eq!(from_strip, manual);
        assert_eq!(StripKind::from_strip(&strip), StripKind::Output);

        strip.device_name = None;
        assert_eq!(StripKind::from_strip(&strip), StripKind::Bus);
    }

    #[test]
    fn custom_theme_kind_styles_are_used() {
        let mut f = file();
        let green = Fill::solid(HexColor::rgb(0, 128, 0));
        f.theme.strips.input.body = green;
        let style = resolve(&f, &StripDescriptor::new("u1", StripType::Input, None));
        assert_eq!(style.body, green.scale_alpha(1.0));
        // Sanity: the default is not accidentally the same.
        assert_ne!(Theme::holaf_dark().strips.input.body, green);
    }

    #[test]
    fn resolved_kind_round_trips_through_declared() {
        for kind in [StripKind::Input, StripKind::Output, StripKind::Bus] {
            let d = StripDescriptor::new("u", kind, Some("dev"));
            let expect = if kind == StripKind::Input {
                StripKind::Input
            } else {
                StripKind::Output
            };
            assert_eq!(d.kind(), expect, "{kind} via a device-backed descriptor");
            assert_eq!(StripKind::from_parts(kind.declared(), None).as_str(), {
                match kind {
                    StripKind::Input => "input",
                    StripKind::Output | StripKind::Bus => "bus",
                }
            });
        }
    }

    /// `set_learning(true)` repaints the frame orange whatever the kind, and
    /// restores the kind style when the learn stops.
    #[test]
    fn a_learning_strip_is_orange_whatever_its_kind() {
        let f = file();
        let orange = HexColor::parse("#f39c12").unwrap();
        for declared in [StripType::Input, StripType::Output] {
            let style = resolve(
                &f,
                &StripDescriptor::new("u1", declared, None).with_learning(true),
            );
            assert!(style.learning);
            assert_eq!(style.body.at(1.0), orange);
            assert_eq!(style.border, orange);
            // The kind itself is untouched: it comes back when the learn stops.
            assert_eq!(style.kind, StripKind::from_parts(declared, None));
            let restored = resolve(&f, &StripDescriptor::new("u1", declared, None));
            assert!(!restored.learning);
            assert_eq!(restored.border, HexColor::parse("#555555").unwrap());
            assert_eq!(restored.body, f.theme.kind_style(restored.kind).body);
        }
    }
}
