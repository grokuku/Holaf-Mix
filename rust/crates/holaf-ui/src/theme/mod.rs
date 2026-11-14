//! Serialisable theme model for the Rust UI.
//!
//! The Python/PySide6 interface hard-codes its palette in widget
//! `setStyleSheet(...)` calls — roughly forty colour literals scattered over
//! `src/ui/main_window.py` and `src/ui/widgets/strip_widget.py`, none of them
//! reachable from user configuration.  The Rust port replaces that with a
//! **data** model:
//!
//! * [`ThemeFile`] — one JSON file on disk (`~/.config/holaf-mix/theme.json`),
//!   holding a named theme plus per-strip overrides keyed by strip `uid`.
//! * [`Theme`] — the resolved design tokens: colours, backgrounds, dimensions,
//!   typography, section and per-strip-kind styles.
//! * [`resolve`] (`crate::theme::resolve`) — the *pure* function turning a strip
//!   descriptor into a [`ResolvedStripStyle`](resolve::ResolvedStripStyle).
//!   Painting code never reads the theme directly for a strip: it asks the
//!   resolver, so override/fallback logic is testable without a UI.
//!
//! Every number in here was measured on the PySide6 build (see
//! `docs/ui-inventory.md`), so the default theme — [`Theme::holaf_dark`] —
//! reproduces the current look exactly while staying editable.
//!
//! # Example
//!
//! ```rust
//! use holaf_ui::theme::{ThemeFile, Fill, HexColor, StripKind};
//! use holaf_ui::theme::resolve::{StripDescriptor, resolve};
//!
//! let file = ThemeFile::holaf_dark();
//! let strip = StripDescriptor::new("uid-1", StripKind::Output, None);
//! let style = resolve(&file, &strip);
//! // An output strip with no device is a virtual bus → purple gradient.
//! assert_eq!(style.kind, StripKind::Bus);
//! assert!(matches!(style.body, Fill::VerticalGradient { .. }));
//! assert_eq!(style.border, HexColor::parse("#555").unwrap());
//! ```

pub mod images;
pub mod paint;
pub mod resolve;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use resolve::{resolve, ResolvedStripStyle, StripDescriptor, StripKind};

// ---------------------------------------------------------------------------
// HexColor — the colour primitive
// ---------------------------------------------------------------------------

/// An sRGB colour with straight (non-premultiplied) alpha, stored as `[r,g,b,a]`.
///
/// Serialised as a CSS-ish hex string so theme files stay hand-editable:
/// `"#3daee9"` (opaque), `"#3daee980"` (50 % alpha) or the short `"#fff"`.
/// The `#` is optional on parse.  Alpha is omitted from the output whenever it
/// is 255, which keeps generated themes readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HexColor(pub [u8; 4]);

/// One hex digit as a nibble (`const fn`, so [`HexColor::from_static`] works in
/// a `const` context).
const fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("invalid hex digit"),
    }
}

impl HexColor {
    /// Build an opaque colour from 8-bit sRGB components.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self([r, g, b, 255])
    }

    /// Build a colour with an explicit 8-bit alpha.
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self([r, g, b, a])
    }

    /// Parse `"#rgb"`, `"#rgba"`, `"#rrggbb"`, `"#rrggbbaa"` (or the same
    /// without the leading `#`).  Returns `None` for anything else — a typo in
    /// a theme file must be reported, not silently replaced by black.
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.trim().trim_start_matches('#');
        let nibble = |c: u8| -> Option<u8> {
            match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            }
        };
        let bytes = hex.as_bytes();
        let mut out = [0u8; 4];
        match bytes.len() {
            3 | 4 => {
                for (i, &c) in bytes.iter().enumerate() {
                    let v = nibble(c)?;
                    out[i] = v * 17; // 0xF → 0xFF
                }
                if bytes.len() == 3 {
                    out[3] = 255;
                }
            }
            6 | 8 => {
                for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
                    out[i] = nibble(pair[0])? * 16 + nibble(pair[1])?;
                }
                if bytes.len() == 6 {
                    out[3] = 255;
                }
            }
            _ => return None,
        }
        Some(Self(out))
    }

    /// Same as [`HexColor::parse`] but usable in a `const` context (the built-in
    /// theme's defaults); panics on a malformed literal, which is a programming
    /// error rather than user input.
    pub const fn from_static(text: &'static str) -> Self {
        let bytes = text.as_bytes();
        // Only "#rrggbb" is supported here; `parse` handles every other form.
        assert!(
            bytes.len() == 7 && bytes[0] == b'#',
            "from_static expects \"#rrggbb\""
        );
        Self([
            hex_nibble(bytes[1]) * 16 + hex_nibble(bytes[2]),
            hex_nibble(bytes[3]) * 16 + hex_nibble(bytes[4]),
            hex_nibble(bytes[5]) * 16 + hex_nibble(bytes[6]),
            255,
        ])
    }

    pub const fn r(self) -> u8 {
        self.0[0]
    }

    pub const fn g(self) -> u8 {
        self.0[1]
    }

    pub const fn b(self) -> u8 {
        self.0[2]
    }

    pub const fn a(self) -> u8 {
        self.0[3]
    }

    /// Same colour with a different alpha (0–255).
    pub const fn with_alpha(self, a: u8) -> Self {
        Self([self.0[0], self.0[1], self.0[2], a])
    }

    /// Same colour with its alpha multiplied by `factor` (clamped to 0–255).
    /// Used to apply `body_opacity` in one place.
    pub fn scale_alpha(self, factor: f32) -> Self {
        let a = (f32::from(self.0[3]) * factor).clamp(0.0, 255.0);
        self.with_alpha(a.round() as u8)
    }

    /// Linear interpolation towards `other` in sRGB space, `t` in `0..=1`.
    /// Gamma-space blending matches what the GPU does with `Color32` (which is
    /// what egui/epaint uses), so a gradient painted here looks like the same
    /// gradient painted by epaint.
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| -> u8 {
            (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
        };
        Self([
            mix(self.0[0], other.0[0]),
            mix(self.0[1], other.0[1]),
            mix(self.0[2], other.0[2]),
            mix(self.0[3], other.0[3]),
        ])
    }

    /// egui colour.  Straight alpha in, straight alpha out
    /// (`Color32::from_rgba_unmultiplied` does the premultiplication epaint
    /// expects in vertex colours).
    pub fn color32(self) -> egui::Color32 {
        egui::Color32::from_rgba_unmultiplied(self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

impl std::fmt::Display for HexColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0[3] == 255 {
            write!(f, "#{:02x}{:02x}{:02x}", self.0[0], self.0[1], self.0[2])
        } else {
            write!(
                f,
                "#{:02x}{:02x}{:02x}{:02x}",
                self.0[0], self.0[1], self.0[2], self.0[3]
            )
        }
    }
}

impl Serialize for HexColor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "invalid colour {text:?} — expected #rgb, #rrggbb or #rrggbbaa"
            ))
        })
    }
}

// Convenience constructors for the many `HexColor::from_static` defaults.
macro_rules! hex {
    ($s:literal) => {
        HexColor::from_static($s)
    };
}

// ---------------------------------------------------------------------------
// Fill / ImageSpec — how a rectangle gets painted
// ---------------------------------------------------------------------------

/// Fill of a rectangle: either a flat colour or a top→bottom gradient.
///
/// Tagged by `type` in JSON so a theme file reads unambiguously:
///
/// ```json
/// { "type": "solid", "color": "#232323" }
/// { "type": "vertical_gradient", "top": "#444444", "bottom": "#3daee9" }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Fill {
    Solid { color: HexColor },
    VerticalGradient { top: HexColor, bottom: HexColor },
}

impl Fill {
    pub const fn solid(color: HexColor) -> Self {
        Self::Solid { color }
    }

    pub const fn vertical(top: HexColor, bottom: HexColor) -> Self {
        Self::VerticalGradient { top, bottom }
    }

    /// Colour at vertical position `t` (`0.0` = top, `1.0` = bottom).
    pub fn at(&self, t: f32) -> HexColor {
        match *self {
            Fill::Solid { color } => color,
            Fill::VerticalGradient { top, bottom } => top.lerp(bottom, t),
        }
    }

    /// Colour used for a single flat swatch (rail background, border of a
    /// gradient body…): the top colour of a gradient, the colour of a solid.
    pub fn flat(&self) -> HexColor {
        match *self {
            Fill::Solid { color } => color,
            Fill::VerticalGradient { top, .. } => top,
        }
    }

    /// Multiply the alpha of every stop — how `body_opacity` is applied.
    pub fn scale_alpha(self, factor: f32) -> Self {
        match self {
            Fill::Solid { color } => Fill::Solid {
                color: color.scale_alpha(factor),
            },
            Fill::VerticalGradient { top, bottom } => Fill::VerticalGradient {
                top: top.scale_alpha(factor),
                bottom: bottom.scale_alpha(factor),
            },
        }
    }

    pub fn is_gradient(&self) -> bool {
        matches!(self, Fill::VerticalGradient { .. })
    }
}

impl Default for Fill {
    fn default() -> Self {
        Fill::Solid {
            color: hex!("#000000"),
        }
    }
}

/// How an image is mapped onto the strip rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// Scale to cover the rect, cropping the overflow (keeps aspect ratio).
    Cover,
    /// Scale to fit inside the rect, leaving background visible (keeps ratio).
    Contain,
    /// Repeat the image at its natural size.
    Tile,
    /// Stretch to the rect, ignoring the aspect ratio.
    #[default]
    Stretch,
}

/// A decorative image applied to an element (strip body, window backdrop…).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageSpec {
    /// Path, absolute or relative to the theme file's directory.
    pub path: String,
    pub fit: Fit,
    /// 0.0–1.0 multiplier on the image's alpha.
    pub opacity_milli: u16,
    /// Optional multiply tint (e.g. to keep a photo inside the palette).
    pub tint: Option<HexColor>,
}

impl ImageSpec {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }

    /// Opacity in `0.0..=1.0`.  Stored as an integer so the whole theme stays
    /// `Eq`/`Hash` and JSON stays diff-friendly.
    pub fn opacity(&self) -> f32 {
        f32::from(self.opacity_milli) / 1000.0
    }

    pub fn with_opacity(self, opacity: f32) -> Self {
        Self {
            opacity_milli: (opacity.clamp(0.0, 1.0) * 1000.0).round() as u16,
            ..self
        }
    }
}

impl Default for ImageSpec {
    fn default() -> Self {
        Self {
            path: String::new(),
            fit: Fit::default(),
            opacity_milli: 1000,
            tint: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Theme tokens
// ---------------------------------------------------------------------------

/// Every palette entry the widgets use.  Grouped by role rather than by widget
/// so a theme can recolour all FX buttons, all routing buttons, etc. at once.
///
/// **The reference is the PySide stylesheets**, not a screenshot of the app:
/// every value below is the literal the Qt code passes to `setStyleSheet`
/// (`src/ui/widgets/strip_widget.py`, `src/ui/main_window.py`).  The comment on
/// each field names the rule it comes from, so a value can be re-checked
/// instead of trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeColors {
    // -- accents & states -------------------------------------------------
    /// Active FX button background — `_update_fx_button_style`, `#2ecc71`.
    pub accent: HexColor,
    /// Active FX button label — same rule, `color: white`.
    pub accent_text: HexColor,
    /// Inactive FX button background — `#333333`.
    pub fx_inactive_bg: HexColor,
    /// Inactive FX button label — `color: #666`.
    pub fx_inactive_text: HexColor,
    /// Inactive FX button, hover background — `:hover { background-color: #444 }`.
    pub fx_inactive_hover_bg: HexColor,
    /// Inactive FX button, hover label — `:hover { color: #999 }`.
    pub fx_inactive_hover_text: HexColor,
    /// Active input→output routing chip (`:checked`, `#4caf50`).
    pub route_active: HexColor,
    /// Idle routing chip background (`#333333`).
    pub route_inactive: HexColor,
    /// Idle routing chip label (`color: #888`).
    pub route_inactive_text: HexColor,
    /// Idle routing chip, hover — `:hover:!checked { background-color: #444 }`.
    pub route_hover_bg: HexColor,
    /// Idle routing chip, hover label (`color: white`).
    pub route_hover_text: HexColor,
    /// MUTE engaged — `_update_mute_style`, `#ff4444`.
    pub mute: HexColor,
    /// MUTE idle label — `color: white` (the background is [`widget_bg`]).
    ///
    /// [`widget_bg`]: Self::widget_bg
    pub mute_text_idle: HexColor,
    /// MONO engaged — `_update_mono_style`, `#3daee9` (the input strip blue).
    pub mono: HexColor,
    /// MONO idle label — `color: #888` (Qt uses a dimmer label than MUTE).
    pub mono_text_idle: HexColor,
    /// MIDI button background, idle (`#333333`).
    pub midi_bg: HexColor,
    /// MIDI button label, idle (`color: #888`).
    pub midi_text: HexColor,
    /// MIDI button, hover background (`:hover { background-color: #444 }`).
    pub midi_hover_bg: HexColor,
    /// MIDI button, hover label (`color: white`).
    pub midi_hover_text: HexColor,
    /// MIDI learn: button background *and* strip border/gradient stop
    /// (`#f39c12`).
    pub learning: HexColor,

    // -- text -------------------------------------------------------------
    /// Strip title, section title, button labels (`color: white`).
    pub text_primary: HexColor,
    /// Captions: "SOURCE IN", "DEVICE OUT", "No Outputs", "BUS MASTER"
    /// (`color: #aaa`).
    pub text_secondary: HexColor,
    /// Dimmed text (`color: #666`, the idle FX label — see
    /// [`fx_inactive_text`](Self::fx_inactive_text)).
    pub text_muted: HexColor,
    /// "VIRTUAL BUS" caption — `_refresh_device_ui_state`, `#dcd0ff`, bold.
    pub bus_caption: HexColor,
    /// "Default Sink" checkbox label — `QCheckBox { color: #ccc }`.
    pub checkbox_text: HexColor,

    // -- surfaces ---------------------------------------------------------
    /// Strip border (`border: 2px solid #555`).
    pub border: HexColor,
    /// Idle generic button background (`#444`: the round "+", idle MONO/MUTE).
    pub widget_bg: HexColor,
    /// Idle generic button background on hover (`#666`: the round "+").
    pub widget_bg_hover: HexColor,
    /// Generic button label (`color: white`).
    pub widget_text: HexColor,
    /// "SELECT APPS" button background (`_init_ui`, `background-color: #555`).
    pub apps_button_bg: HexColor,
    /// "SELECT APPS" button on hover (`background-color: #777`).
    pub apps_button_bg_hover: HexColor,
    /// Device combo background (`_style_combo`, `background-color: #222`).
    pub combo_bg: HexColor,
    /// Device combo border (`border: 1px solid #444`).
    pub combo_border: HexColor,
    /// Translucent plate behind the routing chips area — PySide
    /// `rgba(0,0,0,0.3)`.
    pub routing_area: HexColor,
    /// The × delete glyph (`color: #888`).
    pub delete: HexColor,
    /// The × delete glyph on hover (`color: #ff5555`).
    pub delete_hover: HexColor,

    // -- fader & meters ---------------------------------------------------
    /// Fader groove background (`QSlider::sub-page:vertical { background: #222 }`).
    pub rail_bg: HexColor,
    /// Fader groove "loaded" part, below the handle (`add-page: #555`).
    pub rail_loaded: HexColor,
    /// Fader handle (`QSlider::handle:vertical { background: white }`).
    pub handle: HexColor,
    /// VU background (`VUMeterWidget.bg_color`, `#222`).
    pub vu_bg: HexColor,
    /// VU lowest band (green, `setColorAt(1.0, #33ff33)`).
    pub vu_low: HexColor,
    /// VU middle band (yellow, `setColorAt(0.2, #ffff33)`).
    pub vu_mid: HexColor,
    /// VU top band (red, `setColorAt(0.0, #ff3333)`).
    pub vu_high: HexColor,
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self {
            accent: hex!("#2ecc71"),
            accent_text: hex!("#ffffff"),
            fx_inactive_bg: hex!("#333333"),
            fx_inactive_text: hex!("#666666"),
            fx_inactive_hover_bg: hex!("#444444"),
            fx_inactive_hover_text: hex!("#999999"),
            route_active: hex!("#4caf50"),
            route_inactive: hex!("#333333"),
            route_inactive_text: hex!("#888888"),
            route_hover_bg: hex!("#444444"),
            route_hover_text: hex!("#ffffff"),
            mute: hex!("#ff4444"),
            mute_text_idle: hex!("#ffffff"),
            mono: hex!("#3daee9"),
            mono_text_idle: hex!("#888888"),
            midi_bg: hex!("#333333"),
            midi_text: hex!("#888888"),
            midi_hover_bg: hex!("#444444"),
            midi_hover_text: hex!("#ffffff"),
            learning: hex!("#f39c12"),
            text_primary: hex!("#ffffff"),
            text_secondary: hex!("#aaaaaa"),
            text_muted: hex!("#666666"),
            bus_caption: hex!("#dcd0ff"),
            checkbox_text: hex!("#cccccc"),
            border: hex!("#555555"),
            widget_bg: hex!("#444444"),
            widget_bg_hover: hex!("#666666"),
            widget_text: hex!("#ffffff"),
            apps_button_bg: hex!("#555555"),
            apps_button_bg_hover: hex!("#777777"),
            combo_bg: hex!("#222222"),
            combo_border: hex!("#444444"),
            routing_area: HexColor::rgba(0, 0, 0, 77), // rgba(0,0,0,0.3)
            delete: hex!("#888888"),
            delete_hover: hex!("#ff5555"),
            rail_bg: hex!("#222222"),
            rail_loaded: hex!("#555555"),
            handle: hex!("#ffffff"),
            vu_bg: hex!("#222222"),
            vu_low: hex!("#33ff33"),
            vu_mid: hex!("#ffff33"),
            vu_high: hex!("#ff3333"),
        }
    }
}

/// Page-level backgrounds: the window, and the body opacity every strip body is
/// multiplied by.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Backgrounds {
    /// Window backdrop (#232323).
    ///
    /// The Qt window has **no** background of its own: its two section widgets
    /// cover the whole central area (`main_layout.setContentsMargins(0,0,0,0)`),
    /// so this value is only visible if a theme leaves a gap.  `#232323` sits
    /// between the two section greys, so a gap reads as one more neutral step.
    pub window: Fill,
    /// Multiplier applied to each strip body's alpha.
    ///
    /// ```text
    /// 1.0 keeps the stylesheet colours exact — a strip is an *opaque*
    ///     gradient in Qt; the sections never show through it.
    /// ```
    /// Lowering it is a theme author's choice (translucent panes over the
    /// window); the default must not dilute the strip colours, which is what
    /// the first version of this theme did at 0.82.
    pub body_opacity: f32,
    /// Optional decorative image painted behind everything.
    pub image: Option<ImageSpec>,
}

impl Default for Backgrounds {
    fn default() -> Self {
        Self {
            window: Fill::solid(hex!("#232323")),
            body_opacity: 1.0,
            image: None,
        }
    }
}

/// Fixed geometry, in logical points.  Every value comes from the PySide
/// widgets (`setFixedWidth(100)`, `setFixedSize(22, 18)`, …) so the Rust port
/// lays out identically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dimensions {
    /// `StripWidget.setFixedWidth(100)`.
    pub strip_width: f32,
    /// `content_layout.setSpacing(10)` in `_create_section`.
    pub strip_gap: f32,
    /// `border-radius: 5px` on the strip frame.
    pub corner_radius: f32,
    /// `layout.setContentsMargins(5, 5, 5, 10)` inside a strip.
    pub strip_padding: f32,
    /// Bottom margin of the strip content box (larger than the other three in
    /// PySide: 10 px).
    pub strip_padding_bottom: f32,
    /// `layout.setSpacing(5)` inside a strip.
    pub strip_spacing: f32,
    /// `content_layout.setContentsMargins(10, 10, 10, 10)`.
    pub section_padding: f32,
    /// `MIN_SECTION_WIDTH = 160` in `_adjust_window_size`.
    pub section_min_width: f32,
    /// `btn_add.setFixedSize(30, 30)`.
    pub add_button_size: f32,
    /// `btn.setFixedSize(22, 18)` for FX buttons.
    pub fx_button_width: f32,
    pub fx_button_height: f32,
    /// `btn.setFixedHeight(20)` for routing chips.
    pub route_button_height: f32,
    /// `btn_midi.setFixedHeight(20)`.
    pub midi_button_height: f32,
    /// `btn_mono.setFixedWidth(45)`.
    pub control_button_width: f32,
    /// `VUMeterWidget.setFixedWidth(6)`.
    pub vu_width: f32,
    /// `QSlider::groove { width: 6px }`.
    pub rail_width: f32,
    /// `QSlider::handle { height: 14px; margin: 0 -4px }` → 14 px wide.
    pub handle_size: f32,
    /// Width reserved for the fader (groove + handle overhang).
    pub fader_width: f32,
    /// Height of the fader + VU block.
    pub fader_height: f32,
    /// Device combo height (`font-size: 9px` + 2 px padding ⇒ ~18).
    pub combo_height: f32,
    /// "Default Sink" checkbox row height.
    pub checkbox_height: f32,
    /// Default strip height, and the window's starting size.
    pub strip_min_height: f32,
    pub window_width: f32,
    pub window_height: f32,
    /// Width of one glyph box in the FX button row (padding inside the 22 px).
    pub button_text_padding: f32,
}

impl Default for Dimensions {
    fn default() -> Self {
        Self {
            strip_width: 100.0,
            strip_gap: 10.0,
            corner_radius: 5.0,
            strip_padding: 5.0,
            strip_padding_bottom: 10.0,
            strip_spacing: 5.0,
            section_padding: 10.0,
            section_min_width: 160.0,
            add_button_size: 30.0,
            fx_button_width: 22.0,
            fx_button_height: 18.0,
            route_button_height: 20.0,
            midi_button_height: 20.0,
            control_button_width: 45.0,
            vu_width: 6.0,
            rail_width: 6.0,
            handle_size: 14.0,
            fader_width: 20.0,
            fader_height: 150.0,
            combo_height: 18.0,
            checkbox_height: 14.0,
            strip_min_height: 430.0,
            // The Qt app saves ~793 × 595 of geometry for the seven default
            // strips; round numbers here so the capture and the real window
            // agree.
            window_width: 800.0,
            window_height: 600.0,
            button_text_padding: 2.0,
        }
    }
}

/// Font sizes, in points.  `font-size: 8px`…`15px` in the PySide stylesheets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Typography {
    /// Optional font family override (resolved through `egui`'s fonts).  `None`
    /// keeps egui's bundled default.
    pub font_family: Option<String>,
    /// "SOURCE IN", FX button glyphs (8 px).
    pub tiny: f32,
    /// "DEVICE OUT", "BUS MASTER", combo text, routing chips, MONO/MUTE
    /// (9 px).
    pub small: f32,
    /// MIDI button (10 px) — the only row wider than `small` below `body`.
    pub midi: f32,
    /// Strip label (11 px, bold).
    pub body: f32,
    /// Section title — `label.setStyleSheet("font-weight: bold; color: white;
    /// font-size: 14px;")`.
    pub section_title: f32,
    /// Window / dialog heading, "+" button, × delete glyph (15 px).
    pub heading: f32,
    /// Multiplier applied to `section_title` and strip labels to fake the
    /// `font-weight: bold` the PySide stylesheets use (egui has no bold face
    /// for the bundled font, so we slightly overdraw instead).
    pub bold_extra: f32,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            font_family: None,
            tiny: 8.0,
            small: 9.0,
            midi: 10.0,
            body: 11.0,
            section_title: 14.0,
            heading: 15.0,
            bold_extra: 0.35,
        }
    }
}

/// One named section ("INPUTS" / "OUTPUTS"): its title, its backdrop and the
/// colour of the title text.
///
/// # The section backdrop is **neutral**
///
/// `_create_section` gives each column a flat, *unsaturated* grey:
/// `#2b2b2b` for INPUTS, `#1e1e1e` for OUTPUTS (`_create_section("INPUTS",
/// "#2b2b2b", …)`).  The two greys are there to separate the columns, nothing
/// more: **all the colour of the window comes from the strips** (their
/// `#444 → accent` gradient), never from the section behind them.
///
/// An earlier version of this theme painted the sections blue/red; that was a
/// misreading of a screenshot and is what
/// `section_backgrounds_are_neutral` now forbids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SectionStyle {
    pub title: String,
    pub background: HexColor,
    pub title_color: HexColor,
}

impl Default for SectionStyle {
    fn default() -> Self {
        Self {
            title: String::new(),
            background: hex!("#2b2b2b"),
            title_color: hex!("#ffffff"),
        }
    }
}

/// The two column styles of the main window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SectionStyles {
    pub input: SectionStyle,
    pub output: SectionStyle,
}

impl Default for SectionStyles {
    fn default() -> Self {
        Self {
            input: SectionStyle {
                title: "INPUTS".to_string(),
                // `_create_section("INPUTS", "#2b2b2b", StripType.INPUT)`.
                background: hex!("#2b2b2b"),
                title_color: hex!("#ffffff"),
            },
            output: SectionStyle {
                title: "OUTPUTS".to_string(),
                // `_create_section("OUTPUTS", "#1e1e1e", StripType.OUTPUT)`.
                background: hex!("#1e1e1e"),
                title_color: hex!("#ffffff"),
            },
        }
    }
}

/// Per-strip-kind style: the body fill, the border and an optional image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripKindStyle {
    pub body: Fill,
    pub border: HexColor,
    pub image: Option<ImageSpec>,
}

impl StripKindStyle {
    fn new(body: Fill) -> Self {
        Self {
            body,
            border: hex!("#555555"),
            image: None,
        }
    }
}

impl Default for StripKindStyle {
    fn default() -> Self {
        Self::new(Fill::default())
    }
}

/// The three visual kinds of strip body (input, physical output, virtual bus),
/// plus the MIDI-learning body.
///
/// # Why the gradient stops are `#444 → accent`
///
/// `StripWidget._update_base_style` paints
/// `qlineargradient(x1:0, y1:0, x2:0, y2:1, stop:0 #444, stop:1 <accent>)`:
/// Qt's `y1:0 → y2:1` runs **top → bottom**, so a strip is a *dark, desaturated*
/// grey at its top which becomes the kind's colour towards its bottom.  Verified
/// twice, because it is easy to read a screenshot the wrong way round:
///
/// * offscreen rendering of the real stylesheet (`QFrame::grab()` + pixel
///   sampling) gives `#444647` at the top and `#3dace6` near the bottom of a
///   394 px input strip;
/// * the same samples in a capture of the running app:
///   `#454446` at the top of the *Mix Out* strip, `#9457ad` near its bottom.
///
/// A strip therefore *desaturates towards the top*.  The bottom of the widget
/// still reads grey because the last two rows are the grey MONO/MUTE (`#444`)
/// and MIDI (`#333`) buttons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripKindStyles {
    /// Input strips: `#444 → #3daee9` (blue).
    pub input: StripKindStyle,
    /// Physical output strips: `#444 → #e93d3d` (red).
    pub output: StripKindStyle,
    /// Virtual outputs (bus): `#444 → #9b59b6` (purple).
    pub bus: StripKindStyle,
    /// MIDI learning: `#444 → #f39c12` (orange) and an orange border.
    pub learning: StripKindStyle,
}

impl Default for StripKindStyles {
    fn default() -> Self {
        Self {
            // `_update_base_style`: the gradient always starts on `#444`.
            input: StripKindStyle::new(Fill::vertical(hex!("#444444"), hex!("#3daee9"))),
            output: StripKindStyle::new(Fill::vertical(hex!("#444444"), hex!("#e93d3d"))),
            bus: StripKindStyle::new(Fill::vertical(hex!("#444444"), hex!("#9b59b6"))),
            learning: StripKindStyle {
                body: Fill::vertical(hex!("#444444"), hex!("#f39c12")),
                // `border: 2px solid #f39c12` while learning, `#555` otherwise.
                border: hex!("#f39c12"),
                image: None,
            },
        }
    }
}

/// Field-by-field override for one strip, keyed by `uid` in
/// [`ThemeFile::strips`].  Every field is optional: only the ones set are
/// applied, everything else falls back to the kind style.  `None` fields are
/// omitted from JSON, so a theme file only shows what it actually changed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StripOverride {
    /// Replaces the whole body fill of the kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Fill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border: Option<HexColor>,
    /// Overrides `backgrounds.body_opacity` for this strip only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opacity_milli: Option<u16>,
    /// Decorative image (replaces the kind's image).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSpec>,
    /// Accent for this strip (its active FX buttons and routing chips).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<HexColor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_primary: Option<HexColor>,
}

impl StripOverride {
    /// Opacity as `0.0..=1.0`, when set.
    pub fn opacity(&self) -> Option<f32> {
        self.opacity_milli.map(|m| f32::from(m) / 1000.0)
    }

    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity_milli = Some((opacity.clamp(0.0, 1.0) * 1000.0).round() as u16);
        self
    }
}

/// The design tokens themselves, without the file wrapper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub colors: ThemeColors,
    pub backgrounds: Backgrounds,
    pub dimensions: Dimensions,
    pub typography: Typography,
    pub sections: SectionStyles,
    pub strips: StripKindStyles,
}

impl Default for Theme {
    fn default() -> Self {
        Self::holaf_dark()
    }
}

impl Theme {
    /// The measured Holaf-Mix dark theme — the default, byte-identical to what
    /// the PySide6 build renders.  Every colour comes from a stylesheet literal
    /// in `src/ui/main_window.py` or `src/ui/widgets/strip_widget.py`; the two
    /// values Qt does not have (`backgrounds.window`, `backgrounds.body_opacity`)
    /// are documented as such at their definition.
    pub fn holaf_dark() -> Self {
        Self {
            colors: ThemeColors::default(),
            backgrounds: Backgrounds::default(),
            dimensions: Dimensions::default(),
            typography: Typography::default(),
            sections: SectionStyles::default(),
            strips: StripKindStyles::default(),
        }
    }

    /// Style of a strip kind, ignoring per-strip overrides.
    pub fn kind_style(&self, kind: StripKind) -> &StripKindStyle {
        match kind {
            StripKind::Input => &self.strips.input,
            StripKind::Output => &self.strips.output,
            StripKind::Bus => &self.strips.bus,
        }
    }

    /// Style of a strip while it waits for a MIDI control (`set_learning(true)`
    /// repaints the body orange and the border orange, whatever the kind).
    pub fn learning_style(&self) -> &StripKindStyle {
        &self.strips.learning
    }

    /// Backdrop of a section.
    pub fn section_style(&self, kind: StripKind) -> &SectionStyle {
        match kind {
            StripKind::Input => &self.sections.input,
            // A bus lives in the OUTPUTS column.
            StripKind::Output | StripKind::Bus => &self.sections.output,
        }
    }
}

/// A theme file: [`Theme`] + identity + per-strip overrides.
///
/// ```json
/// {
///   "name": "holaf-dark",
///   "version": 1,
///   "theme": { "colors": { … }, … },
///   "strips": {
///     "uid-mic": { "body": { "type": "solid", "color": "#1b2b1f" } }
///   }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeFile {
    /// Human-readable name, shown in a future theme picker.
    pub name: String,
    /// Format version, so a future loader can migrate old files.
    pub version: u32,
    pub theme: Theme,
    /// Per-strip overrides keyed by strip `uid` (insertion order preserved so
    /// a saved file keeps the user's order).
    pub strips: IndexMap<String, StripOverride>,
}

/// Current theme-file format version.
pub const THEME_FORMAT_VERSION: u32 = 1;

impl Default for ThemeFile {
    fn default() -> Self {
        Self::holaf_dark()
    }
}

impl ThemeFile {
    /// The built-in theme, with no per-strip overrides.
    pub fn holaf_dark() -> Self {
        Self {
            name: "holaf-dark".to_string(),
            version: THEME_FORMAT_VERSION,
            theme: Theme::holaf_dark(),
            strips: IndexMap::new(),
        }
    }

    /// Override for a strip, if any.
    pub fn strip_override(&self, uid: &str) -> Option<&StripOverride> {
        self.strips.get(uid)
    }

    /// Parse a theme file, rejecting a format version we cannot understand.
    ///
    /// A future version is an error rather than a best-effort load: silently
    /// ignoring fields the user wrote would look like the theme "half works".
    pub fn from_json(text: &str) -> Result<Self, ThemeError> {
        let file: ThemeFile =
            serde_json::from_str(text).map_err(|e| ThemeError::Parse(e.to_string()))?;
        if file.version > THEME_FORMAT_VERSION {
            return Err(ThemeError::UnsupportedVersion {
                found: file.version,
                supported: THEME_FORMAT_VERSION,
            });
        }
        Ok(file)
    }

    /// Pretty-printed JSON (stable field order: `IndexMap` + struct order).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("theme is always serialisable")
    }

    /// Read a theme file from disk.
    pub fn load(path: &std::path::Path) -> Result<Self, ThemeError> {
        let text = std::fs::read_to_string(path).map_err(|e| ThemeError::Io {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Self::from_json(&text)
    }

    /// Write the theme as JSON, creating the parent directory if needed.
    pub fn save(&self, path: &std::path::Path) -> Result<(), ThemeError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| ThemeError::Io {
                    path: parent.display().to_string(),
                    reason: e.to_string(),
                })?;
            }
        }
        std::fs::write(path, self.to_json()).map_err(|e| ThemeError::Io {
            path: path.display().to_string(),
            reason: e.to_string(),
        })
    }
}

/// Errors from loading/validating a theme file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeError {
    /// The JSON did not match the schema.
    Parse(String),
    /// `version` is newer than this build understands.
    UnsupportedVersion { found: u32, supported: u32 },
    /// The file could not be read/written.
    Io { path: String, reason: String },
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeError::Parse(reason) => write!(f, "invalid theme file: {reason}"),
            ThemeError::UnsupportedVersion { found, supported } => write!(
                f,
                "theme format version {found} is newer than the supported {supported}"
            ),
            ThemeError::Io { path, reason } => write!(f, "{path}: {reason}"),
        }
    }
}

impl std::error::Error for ThemeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_color_parses_all_forms() {
        assert_eq!(
            HexColor::parse("#3ecc6a"),
            Some(HexColor::rgb(0x3e, 0xcc, 0x6a))
        );
        assert_eq!(
            HexColor::parse("3ecc6a"),
            Some(HexColor::rgb(0x3e, 0xcc, 0x6a))
        );
        assert_eq!(HexColor::parse("#fff"), Some(HexColor::rgb(255, 255, 255)));
        assert_eq!(
            HexColor::parse("#3ecc6a80"),
            Some(HexColor::rgba(0x3e, 0xcc, 0x6a, 0x80))
        );
        assert_eq!(
            HexColor::parse("#0008"),
            Some(HexColor::rgba(0, 0, 0, 0x88))
        );
        assert_eq!(
            HexColor::parse(" #FFF "),
            Some(HexColor::rgb(255, 255, 255))
        );
    }

    #[test]
    fn hex_color_rejects_garbage() {
        for bad in [
            "",
            "#",
            "#12",
            "#12345",
            "#1234567",
            "not-a-colour",
            "#gggggg",
        ] {
            assert_eq!(HexColor::parse(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn hex_color_formats_round_trip() {
        for text in ["#3ecc6a", "#232323", "#000000", "#ffffff"] {
            let parsed = HexColor::parse(text).expect("valid");
            assert_eq!(parsed.to_string(), text);
        }
        // Alpha is only printed when it is not opaque.
        assert_eq!(HexColor::rgba(1, 2, 3, 0x80).to_string(), "#01020380");
        assert_eq!(HexColor::rgba(1, 2, 3, 255).to_string(), "#010203");
    }

    #[test]
    fn hex_color_from_static_matches_parse() {
        assert_eq!(hex!("#2b2b2b"), HexColor::parse("#2b2b2b").unwrap());
        assert_eq!(hex!("#000000"), HexColor::rgb(0, 0, 0));
    }

    #[test]
    fn hex_color_serde_is_a_string() {
        let json = serde_json::to_string(&HexColor::rgb(0x3e, 0xcc, 0x6a)).unwrap();
        assert_eq!(json, "\"#3ecc6a\"");
        let back: HexColor = serde_json::from_str("\"#3ecc6a\"").unwrap();
        assert_eq!(back, HexColor::rgb(0x3e, 0xcc, 0x6a));
    }

    #[test]
    fn hex_color_deserialize_error_names_the_value() {
        let err = serde_json::from_str::<HexColor>("\"#zzz\"").unwrap_err();
        assert!(err.to_string().contains("#zzz"), "{err}");
    }

    #[test]
    fn hex_color_scale_alpha_and_lerp() {
        let c = HexColor::rgb(10, 20, 30);
        assert_eq!(c.scale_alpha(0.5).a(), 128);
        assert_eq!(c.scale_alpha(2.0).a(), 255);
        assert_eq!(
            c.lerp(HexColor::rgb(20, 40, 60), 0.5),
            HexColor::rgb(15, 30, 45)
        );
        assert_eq!(c.lerp(HexColor::rgb(20, 40, 60), 0.0), c);
        assert_eq!(
            c.lerp(HexColor::rgb(20, 40, 60), 1.0),
            HexColor::rgb(20, 40, 60)
        );
        // Out-of-range t is clamped, not wrapped.
        assert_eq!(c.lerp(HexColor::rgb(20, 40, 60), -1.0), c);
    }

    #[test]
    fn fill_at_interpolates_vertically() {
        let fill = Fill::vertical(HexColor::rgb(0, 0, 0), HexColor::rgb(100, 200, 40));
        assert_eq!(fill.at(0.0), HexColor::rgb(0, 0, 0));
        assert_eq!(fill.at(1.0), HexColor::rgb(100, 200, 40));
        assert_eq!(fill.at(0.5), HexColor::rgb(50, 100, 20));
        assert_eq!(
            Fill::solid(HexColor::rgb(1, 2, 3)).at(0.9),
            HexColor::rgb(1, 2, 3)
        );
    }

    #[test]
    fn fill_scale_alpha_touches_every_stop() {
        let fill = Fill::vertical(HexColor::rgb(0, 0, 0), HexColor::rgb(9, 9, 9)).scale_alpha(0.5);
        assert_eq!(fill.at(0.0).a(), 128);
        assert_eq!(fill.at(1.0).a(), 128);
    }

    #[test]
    fn default_theme_matches_the_pyside_stylesheets() {
        let t = Theme::holaf_dark();
        assert_eq!(
            t.backgrounds.window,
            Fill::solid(HexColor::parse("#232323").unwrap())
        );
        // `_create_section("INPUTS", "#2b2b2b", …)` / `"OUTPUTS", "#1e1e1e"`.
        assert_eq!(
            t.sections.input.background,
            HexColor::parse("#2b2b2b").unwrap()
        );
        assert_eq!(
            t.sections.output.background,
            HexColor::parse("#1e1e1e").unwrap()
        );
        assert_eq!(
            t.sections.input.title_color,
            HexColor::parse("#ffffff").unwrap()
        );
        assert_eq!(
            t.sections.output.title_color,
            HexColor::parse("#ffffff").unwrap()
        );
        // `_update_base_style`: `stop:0 #444` → `stop:1 <accent>`.
        assert_eq!(
            t.strips.input.body,
            Fill::vertical(
                HexColor::parse("#444444").unwrap(),
                HexColor::parse("#3daee9").unwrap()
            )
        );
        assert_eq!(
            t.strips.output.body,
            Fill::vertical(
                HexColor::parse("#444444").unwrap(),
                HexColor::parse("#e93d3d").unwrap()
            )
        );
        assert_eq!(
            t.strips.bus.body,
            Fill::vertical(
                HexColor::parse("#444444").unwrap(),
                HexColor::parse("#9b59b6").unwrap()
            )
        );
        assert_eq!(
            t.strips.learning.body,
            Fill::vertical(
                HexColor::parse("#444444").unwrap(),
                HexColor::parse("#f39c12").unwrap()
            )
        );
        assert_eq!(
            t.strips.learning.border,
            HexColor::parse("#f39c12").unwrap()
        );
        for kind in [StripKind::Input, StripKind::Output, StripKind::Bus] {
            assert_eq!(
                t.kind_style(kind).border,
                HexColor::parse("#555555").unwrap(),
                "`border: 2px solid #555` is the same for every kind"
            );
        }
        // `_update_fx_button_style`.
        assert_eq!(t.colors.accent, HexColor::parse("#2ecc71").unwrap());
        assert_eq!(t.colors.accent_text, HexColor::parse("#ffffff").unwrap());
        assert_eq!(t.colors.fx_inactive_bg, HexColor::parse("#333333").unwrap());
        assert_eq!(
            t.colors.fx_inactive_text,
            HexColor::parse("#666666").unwrap()
        );
        assert_eq!(
            t.colors.fx_inactive_hover_bg,
            HexColor::parse("#444444").unwrap()
        );
        assert_eq!(
            t.colors.fx_inactive_hover_text,
            HexColor::parse("#999999").unwrap()
        );
        // `set_routing_targets`.
        assert_eq!(t.colors.route_active, HexColor::parse("#4caf50").unwrap());
        assert_eq!(t.colors.route_inactive, HexColor::parse("#333333").unwrap());
        assert_eq!(
            t.colors.route_inactive_text,
            HexColor::parse("#888888").unwrap()
        );
        assert_eq!(t.colors.route_hover_bg, HexColor::parse("#444444").unwrap());
        assert_eq!(
            t.colors.route_hover_text,
            HexColor::parse("#ffffff").unwrap()
        );
        // `_update_mute_style` / `_update_mono_style`.
        assert_eq!(t.colors.mute, HexColor::parse("#ff4444").unwrap());
        assert_eq!(t.colors.mute_text_idle, HexColor::parse("#ffffff").unwrap());
        assert_eq!(t.colors.mono, HexColor::parse("#3daee9").unwrap());
        assert_eq!(t.colors.mono_text_idle, HexColor::parse("#888888").unwrap());
        // `btn_midi` + `set_learning`.
        assert_eq!(t.colors.midi_bg, HexColor::parse("#333333").unwrap());
        assert_eq!(t.colors.midi_text, HexColor::parse("#888888").unwrap());
        assert_eq!(t.colors.midi_hover_bg, HexColor::parse("#444444").unwrap());
        assert_eq!(
            t.colors.midi_hover_text,
            HexColor::parse("#ffffff").unwrap()
        );
        assert_eq!(t.colors.learning, HexColor::parse("#f39c12").unwrap());
        // Text roles.
        assert_eq!(t.colors.text_primary, HexColor::parse("#ffffff").unwrap());
        assert_eq!(t.colors.text_secondary, HexColor::parse("#aaaaaa").unwrap());
        assert_eq!(t.colors.text_muted, HexColor::parse("#666666").unwrap());
        assert_eq!(t.colors.bus_caption, HexColor::parse("#dcd0ff").unwrap());
        assert_eq!(t.colors.checkbox_text, HexColor::parse("#cccccc").unwrap());
        // Surfaces.
        assert_eq!(t.colors.border, HexColor::parse("#555555").unwrap());
        assert_eq!(t.colors.widget_bg, HexColor::parse("#444444").unwrap());
        assert_eq!(
            t.colors.widget_bg_hover,
            HexColor::parse("#666666").unwrap()
        );
        assert_eq!(t.colors.widget_text, HexColor::parse("#ffffff").unwrap());
        assert_eq!(t.colors.apps_button_bg, HexColor::parse("#555555").unwrap());
        assert_eq!(
            t.colors.apps_button_bg_hover,
            HexColor::parse("#777777").unwrap()
        );
        assert_eq!(t.colors.combo_bg, HexColor::parse("#222222").unwrap());
        assert_eq!(t.colors.combo_border, HexColor::parse("#444444").unwrap());
        assert_eq!(t.colors.delete, HexColor::parse("#888888").unwrap());
        assert_eq!(t.colors.delete_hover, HexColor::parse("#ff5555").unwrap());
        // Fader & VU.
        assert_eq!(t.colors.rail_bg, HexColor::parse("#222222").unwrap());
        assert_eq!(t.colors.rail_loaded, HexColor::parse("#555555").unwrap());
        assert_eq!(t.colors.handle, HexColor::parse("#ffffff").unwrap());
        assert_eq!(t.colors.vu_bg, HexColor::parse("#222222").unwrap());
        assert_eq!(t.colors.vu_low, HexColor::parse("#33ff33").unwrap());
        assert_eq!(t.colors.vu_mid, HexColor::parse("#ffff33").unwrap());
        assert_eq!(t.colors.vu_high, HexColor::parse("#ff3333").unwrap());
        // A strip is opaque in Qt: the sections never show through it.
        assert!((t.backgrounds.body_opacity - 1.0).abs() < 1e-6);
        assert_eq!(t.dimensions.strip_width, 100.0);
        assert_eq!(t.dimensions.strip_gap, 10.0);
        assert_eq!(t.dimensions.corner_radius, 5.0);
        // `font-size:` literals, in px.
        assert_eq!(t.typography.tiny, 8.0);
        assert_eq!(t.typography.small, 9.0);
        assert_eq!(t.typography.midi, 10.0);
        assert_eq!(t.typography.body, 11.0);
        assert_eq!(t.typography.section_title, 14.0);
        assert_eq!(t.typography.heading, 15.0);
    }

    /// Channel spread of a colour (`0` for a pure grey).
    fn chroma(c: HexColor) -> u8 {
        let max = c.r().max(c.g()).max(c.b());
        let min = c.r().min(c.g()).min(c.b());
        max - min
    }

    /// The regression this test exists for: the section backdrops must be
    /// **neutral greys**.  In the PySide app the two columns are `#2b2b2b` and
    /// `#1e1e1e`; all the colour comes from the strips.  A tinted section (the
    /// `#4a6b80` / `#5a3a44` pair an earlier version of this theme shipped) is
    /// a misreading of a screenshot, not a design.
    #[test]
    fn section_backgrounds_are_neutral() {
        let t = Theme::holaf_dark();
        let columns = [
            ("INPUTS", t.sections.input.background),
            ("OUTPUTS", t.sections.output.background),
            ("window", t.backgrounds.window.flat()),
        ];
        for (name, color) in columns {
            assert!(
                chroma(color) <= 2,
                "{name} background {color} is tinted (chroma {}); section \
                 backdrops and the window must stay neutral greys",
                chroma(color)
            );
        }
    }

    /// Two *distinct* greys: OUTPUTS darker than INPUTS, so the columns read as
    /// two levels without any hue being involved.
    #[test]
    fn the_two_sections_are_two_grey_levels() {
        let t = Theme::holaf_dark();
        let input = t.sections.input.background;
        let output = t.sections.output.background;
        assert_ne!(input, output, "the columns must be distinguishable");
        let luma = |c: HexColor| {
            0.299 * f32::from(c.r()) + 0.587 * f32::from(c.g()) + 0.114 * f32::from(c.b())
        };
        assert!(
            luma(input) > luma(output),
            "INPUTS ({input}) must be the lighter column, OUTPUTS ({output}) the darker"
        );
        // …and both stay dark, like the Qt app (no light grey column).
        assert!(luma(input) < 80.0 && luma(output) < 80.0);
    }

    /// The strip gradient is `#444 → accent`: desaturated *at the top*, the
    /// kind's colour at the bottom.  Documented with pixel samples in
    /// [`StripKindStyles`]; locked here so it cannot be reversed by accident.
    #[test]
    fn strip_bodies_go_from_grey_to_the_kind_accent() {
        let t = Theme::holaf_dark();
        let grey = HexColor::parse("#444444").unwrap();
        let cases = [
            (StripKind::Input, "#3daee9"),
            (StripKind::Output, "#e93d3d"),
            (StripKind::Bus, "#9b59b6"),
        ];
        for (kind, accent) in cases {
            let body = t.kind_style(kind).body;
            assert_eq!(body.at(0.0), grey, "{kind:?} must start on #444 at the top");
            assert_eq!(
                body.at(1.0),
                HexColor::parse(accent).unwrap(),
                "{kind:?} must end on its accent at the bottom"
            );
            assert_eq!(chroma(grey), 0);
        }
        // The learning body ends on the same orange as its border.
        assert_eq!(t.learning_style().body.at(1.0), t.learning_style().border);
    }

    #[test]
    fn default_theme_has_no_empty_required_field() {
        let t = Theme::holaf_dark();
        assert!(!t.sections.input.title.is_empty());
        assert!(!t.sections.output.title.is_empty());
        assert!(t.dimensions.strip_width > 0.0);
        assert!(t.dimensions.fader_height > 0.0);
        assert!(t.typography.small > 0.0);
        assert!(t.backgrounds.body_opacity > 0.0 && t.backgrounds.body_opacity <= 1.0);
        for kind in [StripKind::Input, StripKind::Output, StripKind::Bus] {
            assert!(
                t.kind_style(kind).border.a() > 0,
                "kind {kind:?} has a transparent border"
            );
        }
    }

    #[test]
    fn theme_file_round_trips_through_json() {
        let mut file = ThemeFile::holaf_dark();
        file.strips.insert(
            "uid-mic".to_string(),
            StripOverride {
                body: Some(Fill::solid(HexColor::parse("#101820").unwrap())),
                ..StripOverride::default()
            }
            .with_opacity(0.5),
        );
        let json = file.to_json();
        let back = ThemeFile::from_json(&json).expect("round trip");
        assert_eq!(back, file);
        assert_eq!(back.name, "holaf-dark");
        assert_eq!(back.version, THEME_FORMAT_VERSION);
    }

    #[test]
    fn theme_file_json_omits_unset_override_fields() {
        let mut file = ThemeFile::holaf_dark();
        file.strips
            .insert("uid-1".to_string(), StripOverride::default());
        let json = file.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let entry = parsed["strips"]["uid-1"].as_object().unwrap();
        assert!(
            entry.is_empty(),
            "unset overrides must not be written: {entry:?}"
        );
    }

    #[test]
    fn partial_theme_json_falls_back_to_defaults() {
        // A theme file only has to contain what it changes.
        let json = r##"{
            "name": "mine",
            "theme": { "colors": { "accent": "#ff0000" } }
        }"##;
        let file = ThemeFile::from_json(json).expect("partial theme");
        assert_eq!(file.theme.colors.accent, HexColor::rgb(255, 0, 0));
        // Everything else is the built-in default.
        assert_eq!(file.theme.colors.mono, ThemeColors::default().mono);
        assert_eq!(file.theme.dimensions.strip_width, 100.0);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let json = r##"{"name":"x","unknown_future_field":42,
                        "theme":{"colors":{"accent":"#010203","future":"#fff"}}}"##;
        let file = ThemeFile::from_json(json).expect("unknown fields are tolerated");
        assert_eq!(file.theme.colors.accent, HexColor::rgb(1, 2, 3));
    }

    #[test]
    fn future_version_is_rejected() {
        let json = format!(r#"{{"version":{}}}"#, THEME_FORMAT_VERSION + 1);
        let err = ThemeFile::from_json(&json).unwrap_err();
        assert!(
            matches!(err, ThemeError::UnsupportedVersion { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("newer"));
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        let err = ThemeFile::from_json("{ not json").unwrap_err();
        assert!(matches!(err, ThemeError::Parse(_)));
    }

    #[test]
    fn image_spec_opacity_is_expressed_in_permille() {
        let spec = ImageSpec::new("bg.png").with_opacity(0.25);
        assert_eq!(spec.opacity_milli, 250);
        assert!((spec.opacity() - 0.25).abs() < 1e-6);
        assert_eq!(ImageSpec::default().opacity(), 1.0);
        assert_eq!(spec.fit, Fit::Stretch);
    }

    #[test]
    fn fill_serde_is_tagged_by_type() {
        let solid = serde_json::to_string(&Fill::solid(HexColor::rgb(1, 2, 3))).unwrap();
        assert_eq!(solid, r##"{"type":"solid","color":"#010203"}"##);
        let grad = serde_json::to_string(&Fill::vertical(
            HexColor::rgb(0, 0, 0),
            HexColor::rgb(255, 255, 255),
        ))
        .unwrap();
        assert_eq!(
            grad,
            r##"{"type":"vertical_gradient","top":"#000000","bottom":"#ffffff"}"##
        );
        let back: Fill = serde_json::from_str(&grad).unwrap();
        assert!(back.is_gradient());
    }
}
