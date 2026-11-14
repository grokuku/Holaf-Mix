//! Views: the main window and one strip.
//!
//! Views are **pure layout**: they receive an immutable strip, the theme, the
//! current meter levels, and return the list of [`UiAction`]s the user asked
//! for.  They never mutate the model and never talk to the engine — the same
//! split the PySide code gets from Qt signals, but explicit and testable.

pub mod main_window;
pub mod strip;

use egui::{Align2, FontId, Painter, Rect};

use crate::theme::HexColor;

pub use main_window::render as main_window_ui;
pub use strip::strip_ui;

/// Draw `text` centred in `rect`.
///
/// `bold_extra > 0` draws the text twice, one hair apart, to approximate the
/// `font-weight: bold` the Qt stylesheets use (egui's bundled font ships a
/// single weight — a real bold face is a TODO for the font-loading step).
pub fn text_centered(
    painter: &Painter,
    rect: Rect,
    text: &str,
    font_size: f32,
    color: HexColor,
    bold_extra: f32,
) {
    if text.is_empty() || rect.width() <= 0.0 {
        return;
    }
    let font = FontId::proportional(font_size.max(1.0));
    let color32 = color.color32();
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        font.clone(),
        color32,
    );
    if bold_extra > 0.0 {
        painter.text(
            rect.center() + egui::vec2(bold_extra, 0.0),
            Align2::CENTER_CENTER,
            text,
            font,
            color32,
        );
    }
}

/// Draw `text` left-aligned at `rect.left_center() + (4, 0)`, elided to fit.
pub fn text_elided_left(
    painter: &Painter,
    rect: Rect,
    text: &str,
    font_size: f32,
    color: HexColor,
    max_width: f32,
) {
    let font = FontId::proportional(font_size.max(1.0));
    let shown = crate::widgets::buttons::elide(painter, text, &font, max_width);
    painter.text(
        egui::pos2(rect.left() + 4.0, rect.center().y),
        Align2::LEFT_CENTER,
        shown,
        font,
        color.color32(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn text_helpers_accept_degenerate_input() {
        // These are painter helpers; with no painter we only check the early
        // returns do not panic on empty content. `Painter` needs a Context but
        // no renderer, so this is safe headless.
        let ctx = crate::test_support::headless_context();
        let painter = ctx.debug_painter();
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(0.0, 0.0));
        let theme = Theme::holaf_dark();
        text_centered(
            &painter,
            rect,
            "INPUTS",
            13.5,
            theme.colors.text_primary,
            0.35,
        );
        text_elided_left(
            &painter,
            rect,
            "device",
            9.0,
            theme.colors.text_primary,
            0.0,
        );
    }
}
