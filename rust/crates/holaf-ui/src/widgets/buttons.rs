//! The button family: FX toggles, routing chips, MONO/MUTE, MIDI, "+".
//!
//! Each widget paints into a **caller-provided rect** (`*_in`) or allocates its
//! exact Qt size itself (the allocating wrappers).  All of them return the
//! [`egui::Response`]; the *meaning* of a click belongs to the view (see
//! `views::strip`), which is what keeps these widgets free of model state.
//!
//! The `*_in` form exists because the strip lays its rows out itself (a strip
//! is a hand-placed column of fixed heights, like the Qt `QVBoxLayout`): a
//! widget that allocated its own space would break the row rhythm.
//!
//! Geometry, colours and label behaviour mirror `strip_widget.py` /
//! `main_window._create_section`:
//!
//! | widget            | Qt source                              | size      |
//! |-------------------|----------------------------------------|-----------|
//! | FX toggle         | `btn.setFixedSize(22, 18)`             | 22 × 18   |
//! | routing chip      | `btn.setFixedHeight(20)`, full width   | flex × 20 |
//! | MONO              | `btn_mono.setFixedWidth(45)`           | 45 × 20   |
//! | MUTE              | fills the rest of the row              | flex × 20 |
//! | MIDI              | `btn_midi.setFixedHeight(20)`          | flex × 20 |
//! | "+" add strip     | `btn_add.setFixedSize(30, 30)`         | 30 × 30   |

use std::hash::Hash;

use egui::{Align2, FontId, Rect, Response, Sense, Ui, Vec2};

use crate::theme::paint;
use crate::theme::{HexColor, Theme};

/// How one button looks: background, label colour, radius, font size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ButtonLook {
    pub bg: HexColor,
    pub fg: HexColor,
    pub radius: f32,
    pub font_size: f32,
    /// Border colour (`border: 1px solid #444` on the inactive FX buttons).
    pub border: Option<HexColor>,
    /// Background on hover, when the stylesheet names one
    /// (`:hover { background-color: … }`).  `None` falls back to a slight
    /// lightening of [`bg`](Self::bg), for buttons Qt gives no hover rule.
    pub bg_hover: Option<HexColor>,
    /// Label colour on hover (`:hover { color: … }`).
    pub fg_hover: Option<HexColor>,
    /// Draw the label twice, one hair apart, to fake Qt's `font-weight: bold`
    /// (egui's bundled font has a single weight).
    pub bold_extra: f32,
}

impl ButtonLook {
    pub fn new(bg: HexColor, fg: HexColor) -> Self {
        Self {
            bg,
            fg,
            radius: 3.0,
            font_size: 9.0,
            border: None,
            bg_hover: None,
            fg_hover: None,
            bold_extra: 0.0,
        }
    }

    pub fn with_radius(mut self, radius: f32) -> Self {
        self.radius = radius;
        self
    }

    pub fn with_font_size(mut self, size: f32) -> Self {
        self.font_size = size;
        self
    }

    pub fn with_border(mut self, border: HexColor) -> Self {
        self.border = Some(border);
        self
    }

    /// Explicit hover colours (`:hover { background-color: …; color: … }`).
    pub fn with_hover(mut self, bg: HexColor, fg: HexColor) -> Self {
        self.bg_hover = Some(bg);
        self.fg_hover = Some(fg);
        self
    }

    pub fn bold(mut self, extra: f32) -> Self {
        self.bold_extra = extra;
        self
    }
}

/// The look of an FX toggle in its two states (`_update_fx_button_style`):
///
/// ```css
/// active   { background-color: #2ecc71; color: white; font-weight: bold; }
/// inactive { background-color: #333; color: #666; border: 1px solid #444; }
/// inactive:hover { background-color: #444; color: #999; }
/// ```
pub fn fx_look(active: bool, theme: &Theme) -> ButtonLook {
    let c = &theme.colors;
    if active {
        ButtonLook::new(c.accent, c.accent_text)
            .with_font_size(theme.typography.tiny)
            .with_radius(2.0)
            .bold(theme.typography.bold_extra)
    } else {
        ButtonLook::new(c.fx_inactive_bg, c.fx_inactive_text)
            .with_font_size(theme.typography.tiny)
            .with_radius(2.0)
            .with_border(c.combo_border)
            .with_hover(c.fx_inactive_hover_bg, c.fx_inactive_hover_text)
    }
}

/// The look of a routing chip in its two states (`set_routing_targets`):
///
/// ```css
///           { background-color: #333; color: #888; border: 1px solid #444; }
/// :checked  { background-color: #4caf50; color: white; border: 1px solid #4caf50; }
/// :hover:!checked { background-color: #444; color: white; }
/// ```
pub fn route_look(active: bool, theme: &Theme) -> ButtonLook {
    let c = &theme.colors;
    if active {
        ButtonLook::new(c.route_active, c.widget_text)
            .with_font_size(theme.typography.small)
            .with_border(c.route_active)
    } else {
        ButtonLook::new(c.route_inactive, c.route_inactive_text)
            .with_font_size(theme.typography.small)
            .with_border(c.combo_border)
            .with_hover(c.route_hover_bg, c.route_hover_text)
    }
}

/// Draw `text` centred in `rect`, optionally twice (fake bold).
fn paint_label(painter: &egui::Painter, rect: Rect, text: &str, look: &ButtonLook) {
    if text.is_empty() {
        return;
    }
    let font = FontId::proportional(look.font_size.max(1.0));
    let color = look.fg.color32();
    let center = rect.center();
    painter.text(center, Align2::CENTER_CENTER, text, font.clone(), color);
    if look.bold_extra > 0.0 {
        painter.text(
            center + Vec2::new(look.bold_extra, 0.0),
            Align2::CENTER_CENTER,
            text,
            font,
            color,
        );
    }
}

/// Paint a button into an already-allocated rect (no layout side effect).
///
/// `salt` disambiguates the interaction id: strips all have a "MONO" button at
/// the same y, so the label alone is not unique — the strip `uid` is passed in.
pub fn button_in(
    ui: &Ui,
    salt: impl Hash + std::fmt::Debug,
    rect: Rect,
    text: &str,
    look: &ButtonLook,
) -> Response {
    // `Id::new` only needs `Hash` (unlike `Id::with`, which also wants
    // `Debug`), and the position keeps every button of a row distinct.
    let id = egui::Id::unique((
        "holaf-button",
        text,
        rect.min.x.to_bits(),
        rect.min.y.to_bits(),
        egui::Id::unique(salt),
    ));
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter().with_clip_rect(rect);
    let hovered = response.hovered();
    // An explicit `:hover` rule wins; otherwise lighten the background just
    // enough to signal interactivity (Qt gives no hover style to MONO/MUTE).
    let mut look = *look;
    if hovered {
        look.bg = look
            .bg_hover
            .unwrap_or_else(|| look.bg.lerp(HexColor::rgb(255, 255, 255), 0.08));
        look.fg = look.fg_hover.unwrap_or(look.fg);
    }
    paint::fill_rect(&painter, rect, crate::theme::Fill::solid(look.bg), look.radius);
    if let Some(border) = look.border {
        paint::stroke_rect(&painter, rect, look.radius, border);
    }
    paint_label(&painter, rect, text, &look);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// Allocate `size` on the current row and paint a button in it.
fn button_alloc(ui: &mut Ui, text: &str, size: Vec2, look: &ButtonLook) -> Response {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    button_in(ui, text, rect, text, look)
}

// ---------------------------------------------------------------------------
// FX toggles
// ---------------------------------------------------------------------------

/// One FX toggle (`GT` `RN` `EQ` `TB` `CP`) in the theme's fixed size.
pub fn fx_button(ui: &mut Ui, salt: impl Hash + std::fmt::Debug, label: &str, active: bool, theme: &Theme) -> Response {
    let size = Vec2::new(
        theme.dimensions.fx_button_width,
        theme.dimensions.fx_button_height,
    );
    fx_button_sized(ui, salt, label, active, size, theme)
}

/// One FX toggle at an explicit size (the strip shrinks the row to fit).
pub fn fx_button_sized(
    ui: &mut Ui,
    salt: impl Hash + std::fmt::Debug,
    label: &str,
    active: bool,
    size: Vec2,
    theme: &Theme,
) -> Response {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    fx_button_in(ui, salt, rect, label, active, theme)
}

/// One FX toggle into an explicit rect (strip layout path).
pub fn fx_button_in(
    ui: &Ui,
    salt: impl Hash + std::fmt::Debug,
    rect: Rect,
    label: &str,
    active: bool,
    theme: &Theme,
) -> Response {
    button_in(ui, salt, rect, label, &fx_look(active, theme))
}

// ---------------------------------------------------------------------------
// Routing chips
// ---------------------------------------------------------------------------

/// A routing chip that fills the available width.
pub fn route_button(ui: &mut Ui, salt: impl Hash + std::fmt::Debug, label: &str, active: bool, theme: &Theme) -> Response {
    let width = ui.available_width().max(20.0);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(width, theme.dimensions.route_button_height),
        Sense::hover(),
    );
    route_button_in(ui, salt, rect, label, active, theme)
}

/// A routing chip into an explicit rect.
pub fn route_button_in(
    ui: &Ui,
    salt: impl Hash + std::fmt::Debug,
    rect: Rect,
    label: &str,
    active: bool,
    theme: &Theme,
) -> Response {
    button_in(ui, salt, rect, label, &route_look(active, theme))
}

// ---------------------------------------------------------------------------
// MONO / MUTE
// ---------------------------------------------------------------------------

/// MONO / MUTE: a two-state button whose label swaps when engaged
/// (`btn_mute.setText("MUTED")`).
///
/// `idle_text` is per button because Qt words them differently: MUTE keeps a
/// white label (`color: white`) while MONO dims to `#888`.
#[allow(clippy::too_many_arguments)]
pub fn state_button(
    ui: &mut Ui,
    salt: impl Hash + std::fmt::Debug,
    label: &str,
    engaged_label: &str,
    engaged: bool,
    engaged_color: HexColor,
    idle_text: HexColor,
    size: Vec2,
    theme: &Theme,
) -> Response {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    state_button_in(
        ui,
        salt,
        rect,
        label,
        engaged_label,
        engaged,
        engaged_color,
        idle_text,
        theme,
    )
}

/// MONO / MUTE into an explicit rect.
#[allow(clippy::too_many_arguments)]
pub fn state_button_in(
    ui: &Ui,
    salt: impl Hash + std::fmt::Debug,
    rect: Rect,
    label: &str,
    engaged_label: &str,
    engaged: bool,
    engaged_color: HexColor,
    idle_text: HexColor,
    theme: &Theme,
) -> Response {
    let c = &theme.colors;
    // Engaged: `background-color: <accent>; color: white; font-weight: bold`.
    // Idle: `background-color: #444; color: white|#888`.
    let look = if engaged {
        ButtonLook::new(engaged_color, c.widget_text)
            .with_font_size(theme.typography.small)
            .bold(theme.typography.bold_extra)
    } else {
        ButtonLook::new(c.widget_bg, idle_text).with_font_size(theme.typography.small)
    };
    let text = if engaged { engaged_label } else { label };
    button_in(ui, salt, rect, text, &look)
}

// ---------------------------------------------------------------------------
// MIDI
// ---------------------------------------------------------------------------

/// MIDI button (`background-color: #333; color: #888`), orange while learning.
pub fn midi_button(ui: &mut Ui, salt: impl Hash + std::fmt::Debug, learning: bool, theme: &Theme) -> Response {
    let width = ui.available_width().max(20.0);
    let size = Vec2::new(width, theme.dimensions.midi_button_height);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    midi_button_in(ui, salt, rect, learning, theme)
}

/// MIDI button into an explicit rect.
///
/// ```css
///           { background-color: #333; color: #888; border: none; font-size: 10px; }
/// :hover    { background-color: #444; color: white; }
/// learning  { background-color: #f39c12; color: black; font-weight: bold; }
/// ```
pub fn midi_button_in(ui: &Ui, salt: impl Hash + std::fmt::Debug, rect: Rect, learning: bool, theme: &Theme) -> Response {
    let c = &theme.colors;
    let look = if learning {
        ButtonLook::new(c.learning, HexColor::rgb(0, 0, 0))
            .with_font_size(theme.typography.midi)
            .bold(theme.typography.bold_extra)
    } else {
        ButtonLook::new(c.midi_bg, c.midi_text)
            .with_font_size(theme.typography.midi)
            .with_radius(0.0)
            .with_hover(c.midi_hover_bg, c.midi_hover_text)
    };
    let text = if learning { "LEARNING..." } else { "MIDI" };
    button_in(ui, salt, rect, text, &look)
}

// ---------------------------------------------------------------------------
// Add / utility buttons
// ---------------------------------------------------------------------------

/// The round "+" that adds a strip (`border-radius: 15px` on a 30 × 30 box).
pub fn round_add_button(ui: &mut Ui, salt: impl Hash + std::fmt::Debug, theme: &Theme) -> Response {
    let size = theme.dimensions.add_button_size;
    let c = &theme.colors;
    let id = egui::Id::unique(("holaf-add", egui::Id::unique(salt)));
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter().with_clip_rect(rect);
    let hovered = response.hovered();
    let bg = if hovered {
        c.widget_bg_hover
    } else {
        c.widget_bg
    };
    painter.circle_filled(rect.center(), size / 2.0, bg.color32());
    // `font-weight: bold` on a single glyph: same fake-bold trick as the rest.
    for dx in [0.0, theme.typography.bold_extra] {
        painter.text(
            rect.center() + Vec2::new(dx, 0.0),
            Align2::CENTER_CENTER,
            "+",
            FontId::proportional(theme.typography.heading),
            c.widget_text.color32(),
        );
    }
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// A flat utility button ("SELECT APPS", `background-color: #555`, hover
/// `#777`).
pub fn flat_button(ui: &mut Ui, label: &str, size: Vec2, theme: &Theme) -> Response {
    let c = &theme.colors;
    let look = ButtonLook::new(c.apps_button_bg, c.widget_text)
        .with_font_size(theme.typography.small)
        .with_radius(3.0)
        .with_hover(c.apps_button_bg_hover, c.widget_text);
    button_alloc(ui, label, size, &look)
}

/// A device selector that only *displays* the current device.
///
/// The Python side uses a `QComboBox`; the device list needs the engine bridge,
/// so this paints the same closed box (`background: #222; border: 1px solid
/// #444`) plus a caret and returns a response for the future picker.
pub fn combo_display(ui: &mut Ui, text: &str, theme: &Theme) -> Response {
    let d = &theme.dimensions;
    let c = &theme.colors;
    let width = ui.available_width().max(20.0);
    let id = ui.next_auto_id();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, d.combo_height), Sense::hover());
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter().with_clip_rect(rect);
    let hovered = response.hovered();
    let bg = if hovered { c.widget_bg } else { c.combo_bg };
    let radius = (d.corner_radius - 2.0).max(0.0);
    paint::fill_rect(&painter, rect, crate::theme::Fill::solid(bg), radius);
    paint::stroke_rect(&painter, rect, radius, c.combo_border);

    // Elide by width (the Qt side truncates at 15 chars then adds "...").
    let font = FontId::proportional(theme.typography.small);
    let shown = elide(&painter, text, &font, rect.width() - 12.0);
    painter.text(
        egui::pos2(rect.left() + 4.0, rect.center().y),
        Align2::LEFT_CENTER,
        shown,
        font,
        c.text_primary.color32(),
    );
    // Caret (▾) drawn as a triangle: no glyph needed, no font dependency.
    let caret = egui::pos2(rect.right() - 6.0, rect.center().y);
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(caret.x - 3.0, caret.y - 1.5),
            egui::pos2(caret.x + 3.0, caret.y - 1.5),
            egui::pos2(caret.x, caret.y + 2.0),
        ],
        c.text_secondary.color32(),
        egui::Stroke::NONE,
    ));
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// Truncate `text` with an ellipsis so it fits `max_width`, measured with
/// egui's own layout (exact, not an estimate).
pub fn elide(painter: &egui::Painter, text: &str, font: &FontId, max_width: f32) -> String {
    if max_width <= 0.0 {
        return String::new();
    }
    let full = painter.layout_no_wrap(text.to_string(), font.clone(), egui::Color32::WHITE);
    if full.size().x <= max_width {
        return text.to_string();
    }
    for (idx, _) in text.char_indices().rev() {
        let candidate = format!("{}…", &text[..idx]);
        let galley = painter.layout_no_wrap(candidate.clone(), font.clone(), egui::Color32::WHITE);
        if galley.size().x <= max_width {
            return candidate;
        }
    }
    "…".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::holaf_dark()
    }

    #[test]
    fn fx_active_style_is_the_pyside_green() {
        let t = theme();
        let look = fx_look(true, &t);
        assert_eq!(look.bg, HexColor::parse("#2ecc71").unwrap());
        assert_eq!(look.fg, HexColor::parse("#ffffff").unwrap());
        assert_eq!(look.font_size, 8.0);
        assert_eq!(look.radius, 2.0);
        assert_eq!(look.border, None);
        // Qt gives the active button no hover rule: the fallback lightening
        // applies, which is why `bg_hover` stays `None`.
        assert_eq!(look.bg_hover, None);
        assert!(look.bold_extra > 0.0);
    }

    #[test]
    fn fx_inactive_style_has_a_border_and_a_hover() {
        let t = theme();
        let look = fx_look(false, &t);
        assert_eq!(look.bg, HexColor::parse("#333333").unwrap());
        assert_eq!(look.fg, HexColor::parse("#666666").unwrap());
        assert_eq!(look.border, Some(HexColor::parse("#444444").unwrap()));
        assert_eq!(look.bg_hover, Some(HexColor::parse("#444444").unwrap()));
        assert_eq!(look.fg_hover, Some(HexColor::parse("#999999").unwrap()));
        assert_eq!(look.bold_extra, 0.0);
    }

    #[test]
    fn route_looks_use_the_green_and_the_dark_grey() {
        let t = theme();
        let on = route_look(true, &t);
        assert_eq!(on.bg, HexColor::parse("#4caf50").unwrap());
        assert_eq!(on.fg, HexColor::parse("#ffffff").unwrap());
        assert_eq!(on.border, Some(HexColor::parse("#4caf50").unwrap()));
        let off = route_look(false, &t);
        assert_eq!(off.bg, HexColor::parse("#333333").unwrap());
        assert_eq!(off.fg, HexColor::parse("#888888").unwrap());
        assert_eq!(off.border, Some(HexColor::parse("#444444").unwrap()));
        // `:hover:!checked { background-color: #444; color: white; }`.
        assert_eq!(off.bg_hover, Some(HexColor::parse("#444444").unwrap()));
        assert_eq!(off.fg_hover, Some(HexColor::parse("#ffffff").unwrap()));
    }

    /// MONO keeps a dimmed label while MUTE keeps a white one — the two
    /// stylesheets differ by one word (`#888` vs `white`) and the port must
    /// not collapse them into a single "idle" colour.
    #[test]
    fn mono_and_mute_idle_labels_differ() {
        let t = theme();
        assert_eq!(t.colors.mono_text_idle, HexColor::parse("#888888").unwrap());
        assert_eq!(t.colors.mute_text_idle, HexColor::parse("#ffffff").unwrap());
        assert_eq!(t.colors.widget_bg, HexColor::parse("#444444").unwrap());
        assert_eq!(t.colors.mute, HexColor::parse("#ff4444").unwrap());
    }

    #[test]
    fn midi_button_uses_its_own_font_size_and_hover() {
        let t = theme();
        let c = &t.colors;
        // idle: `background-color: #333; color: #888; font-size: 10px`
        let idle = ButtonLook::new(c.midi_bg, c.midi_text)
            .with_font_size(t.typography.midi)
            .with_radius(0.0)
            .with_hover(c.midi_hover_bg, c.midi_hover_text);
        assert_eq!(idle.font_size, 10.0);
        assert_eq!(idle.radius, 0.0);
        assert_eq!(idle.bg_hover, Some(HexColor::parse("#444444").unwrap()));
        assert_eq!(idle.fg_hover, Some(HexColor::parse("#ffffff").unwrap()));
    }

    #[test]
    fn button_look_builders_are_chainable() {
        let look = ButtonLook::new(HexColor::rgb(1, 1, 1), HexColor::rgb(2, 2, 2))
            .with_radius(4.0)
            .with_font_size(12.0)
            .with_border(HexColor::rgb(3, 3, 3))
            .with_hover(HexColor::rgb(4, 4, 4), HexColor::rgb(5, 5, 5))
            .bold(0.4);
        assert_eq!(look.radius, 4.0);
        assert_eq!(look.font_size, 12.0);
        assert_eq!(look.border, Some(HexColor::rgb(3, 3, 3)));
        assert_eq!(look.bg_hover, Some(HexColor::rgb(4, 4, 4)));
        assert_eq!(look.fg_hover, Some(HexColor::rgb(5, 5, 5)));
        assert_eq!(look.bold_extra, 0.4);
    }

    #[test]
    fn default_button_look_is_neutral() {
        let look = ButtonLook::new(HexColor::rgb(0, 0, 0), HexColor::rgb(255, 255, 255));
        assert_eq!(look.radius, 3.0);
        assert_eq!(look.font_size, 9.0);
        assert_eq!(look.border, None);
        assert_eq!(look.bg_hover, None);
        assert_eq!(look.fg_hover, None);
        assert_eq!(look.bold_extra, 0.0);
    }

    #[test]
    fn elide_shortens_long_text_and_keeps_short_text() {
        let ctx = crate::test_support::headless_context();
        let painter = ctx.debug_painter();
        let font = FontId::proportional(9.0);
        assert_eq!(elide(&painter, "HP", &font, 100.0), "HP");
        let long = "alsa_output.pci-0000_00_1f.3.analog-stereo";
        let shortened = elide(&painter, long, &font, 40.0);
        assert!(shortened.ends_with('…'), "{shortened}");
        assert!(shortened.chars().count() < long.chars().count());
        assert_eq!(elide(&painter, long, &font, 0.0), "");
    }

    #[test]
    fn theme_sizes_match_the_qt_stylesheets() {
        let t = theme();
        assert_eq!(t.dimensions.fx_button_width, 22.0);
        assert_eq!(t.dimensions.fx_button_height, 18.0);
        assert_eq!(t.dimensions.route_button_height, 20.0);
        assert_eq!(t.dimensions.midi_button_height, 20.0);
        assert_eq!(t.dimensions.control_button_width, 45.0);
        assert_eq!(t.dimensions.add_button_size, 30.0);
        assert_eq!(t.dimensions.combo_height, 18.0);
        assert_eq!(t.dimensions.checkbox_height, 14.0);
    }
}
