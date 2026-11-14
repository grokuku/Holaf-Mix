//! The main window: two column sections, `INPUTS` and `OUTPUTS`.
//!
//! Mirrors `MainWindow._create_section` + `_adjust_window_size`:
//!
//! * each section is a full-height **neutral grey** column (`#2b2b2b` for
//!   INPUTS, `#1e1e1e` for OUTPUTS) with a bold title and the round "+" button
//!   in its header.  The colour of the window comes from the strips, never
//!   from these columns — see [`SectionStyle`](crate::theme::SectionStyle);
//! * strips are laid out **left to right** with a 10 px gap and 10 px margins;
//! * a section is exactly `layout::section_width(count)` wide (the Qt formula)
//!   and the two columns share the window width proportionally
//!   (`main_layout.setStretch`), which is what makes the last strip of a column
//!   never sit flush against the split;
//! * strips **fill the height** the window gives them (user decision,
//!   2026-11-14): the fader absorbs whatever the fixed rows leave, so a taller
//!   window means a taller fader and nothing else moves.  Both sections get the
//!   same height, so their strips stay aligned.
//!
//! The geometry itself (widths, row heights, the resize decision) lives in
//! [`crate::layout`]; this module only paints.

use egui::{Rect, Ui, UiBuilder, Vec2};

use holaf_core::model::StripType;

use crate::app::{AppState, UiAction};
use crate::layout;
use crate::theme::paint;
use crate::views::{strip_ui, text_centered};

/// Draw the whole window into `ui`.
pub fn render(state: &mut AppState, ui: &mut Ui) {
    // `ui.max_rect()` is the *available* area (5000 × 5000 in a root Ui), not
    // the window: `content_rect()` is the real screen rectangle.
    let screen = ui.ctx().content_rect();
    let mut actions: Vec<UiAction> = Vec::new();

    // The drawing scope borrows `state.theme` (immutably) and `state.strips`
    // (immutably) while pushing into `state.image_cache` (mutably) — disjoint
    // fields, so this is fine — but it must end before `state.apply` runs.
    {
        let theme = &state.theme.theme;
        let dims = &theme.dimensions;
        let painter = ui.painter().with_clip_rect(screen);

        // --- window backdrop (+ optional decorative image) ---------------
        paint::fill_rect(&painter, screen, theme.backgrounds.window, 0.0);
        if let Some(spec) = &theme.backgrounds.image {
            if let Some((handle, size)) = state.image_cache.get(ui.ctx(), spec) {
                paint::paint_texture(
                    &painter,
                    screen,
                    handle.id(),
                    size,
                    spec.fit,
                    spec.tint,
                    spec.opacity(),
                );
            }
        }

        // --- column widths -----------------------------------------------
        //
        // `_adjust_window_size`: each column is `section_width(count)` wide and
        // the window is their sum (locked by `layout::enforce_window_width`).
        // `setStretch(width_inputs, width_outputs)` then shares the window in
        // the same ratio, so a window that is momentarily wider (or a capture
        // rendered at 800 px) widens both columns instead of leaving a gap.
        let (input_count, output_count) = layout::counts(&state.strips);
        let input_w = layout::section_width(input_count) as f32;
        let output_w = layout::section_width(output_count) as f32;
        let needed = input_w + output_w;
        let scale = if needed > 0.0 {
            (screen.width() / needed).max(1.0)
        } else {
            1.0
        };
        let input_w = input_w * scale;
        let output_rect = Rect::from_min_size(
            egui::pos2(screen.left() + input_w, screen.top()),
            Vec2::new((screen.width() - input_w).max(0.0), screen.height()),
        );
        let input_rect = Rect::from_min_size(screen.min, Vec2::new(input_w, screen.height()));

        // --- strip height ------------------------------------------------
        //
        // Vertical axis: the strips FILL what the window gives them (user
        // decision, 2026-11-14) — the fader absorbs the extra height, the
        // button rows keep their theme heights.  The `QScrollArea` + `AlignTop`
        // behaviour of Qt (strip at its `sizeHint`, empty space below) is
        // deliberately *not* reproduced any more: the window is the mixer's
        // fader surface, dead space below a fader is wasted fader travel.
        //
        // Both sections use the same `strip_h`, so INPUTS and OUTPUTS stay
        // aligned.  The floor is the shortest an input strip may be (fixed rows
        // + 40 px fader); below it the strip clips its last rows (the painter
        // clip rect is the strip) rather than drawing an unusable 2 px fader.
        // `main.rs` passes `layout::minimum_window_height` to
        // `with_min_inner_size`, so the WM cannot reach that regime by dragging.
        let outputs = state.output_targets();
        let strip_h = layout::strip_height_for(theme, screen.height(), outputs.len());

        for (section_rect, kind) in [
            (input_rect, StripType::Input),
            (output_rect, StripType::Output),
        ] {
            let style = match kind {
                StripType::Input => &theme.sections.input,
                StripType::Output => &theme.sections.output,
            };
            painter.rect_filled(
                section_rect,
                egui::CornerRadius::ZERO,
                style.background.color32(),
            );

            // header: title (bold) + "+"
            let header = Rect::from_min_size(
                section_rect.min,
                Vec2::new(section_rect.width(), layout::header_height(theme)),
            );
            let title_pos = egui::pos2(
                header.left() + dims.section_padding,
                header.top() + dims.section_padding + dims.add_button_size / 2.0,
            );
            let title_painter = ui.painter().with_clip_rect(header);
            let title_font = egui::FontId::proportional(theme.typography.section_title);
            // Drawn twice, one hair apart: fake bold (egui ships one weight).
            for dx in [0.0, theme.typography.bold_extra] {
                title_painter.text(
                    title_pos + Vec2::new(dx, 0.0),
                    egui::Align2::LEFT_CENTER,
                    &style.title,
                    title_font.clone(),
                    style.title_color.color32(),
                );
            }
            let add_rect = Rect::from_center_size(
                egui::pos2(
                    title_pos.x + 130.0 + dims.add_button_size / 2.0,
                    header.center().y,
                ),
                Vec2::splat(dims.add_button_size),
            );
            let mut header_ui = ui.new_child(UiBuilder::new().max_rect(add_rect));
            if crate::widgets::round_add_button(&mut header_ui, kind.as_str(), theme).clicked() {
                actions.push(UiAction::AddStrip { kind });
            }

            // strips, left to right
            let indices: Vec<usize> = state
                .strips
                .iter()
                .enumerate()
                .filter(|(_, s)| s.kind == kind)
                .map(|(i, _)| i)
                .collect();
            let top = section_rect.top() + layout::header_height(theme);
            let mut x = section_rect.left() + dims.section_padding;
            for index in indices {
                let rect =
                    Rect::from_min_size(egui::pos2(x, top), Vec2::new(dims.strip_width, strip_h));
                if rect.right() > section_rect.right() {
                    break; // column narrower than its content: never spill over
                }
                let strip = &state.strips[index];
                let levels = state.levels_for(index);
                let learning = state
                    .midi_learning
                    .as_ref()
                    .is_some_and(|(uid, _)| uid == &strip.uid);
                actions.extend(strip_ui(
                    ui,
                    rect,
                    &state.theme,
                    strip,
                    &outputs,
                    levels,
                    &mut state.image_cache,
                    learning,
                ));
                x += dims.strip_width + dims.strip_gap;
            }
        }

        if state.strips.is_empty() {
            text_centered(
                ui.painter(),
                screen,
                "No strips - use the + button to add one",
                theme.typography.heading,
                theme.colors.text_secondary,
                0.0,
            );
        }
    }

    for action in actions {
        state.apply(action);
    }

    // Status line, drawn last so it sits on top of everything.
    if let Some(status) = state.status.clone() {
        let theme = &state.theme.theme;
        let bar = Rect::from_min_size(
            egui::pos2(screen.left(), screen.bottom() - layout::FOOTER_HEIGHT),
            Vec2::new(screen.width(), layout::FOOTER_HEIGHT),
        );
        let painter = ui.painter().with_clip_rect(screen);
        painter.rect_filled(
            bar,
            egui::CornerRadius::ZERO,
            egui::Color32::from_black_alpha(160),
        );
        painter.text(
            egui::pos2(bar.left() + 8.0, bar.center().y),
            egui::Align2::LEFT_CENTER,
            status,
            egui::FontId::proportional(theme.typography.small),
            theme.colors.text_secondary.color32(),
        );
    }

    // Keep the VU meters moving at 25 Hz, like `METER_POLL_INTERVAL_MS`.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(40));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn a_strip_is_taller_than_its_fixed_rows() {
        let t = Theme::holaf_dark();
        let input_fixed = layout::strip_fixed_height(&t, StripType::Input, 3);
        let output_fixed = layout::strip_fixed_height(&t, StripType::Output, 3);
        // An input differs from an output by: the "Default Sink" checkbox (14),
        // the "SELECT APPS" button (18), the routing plate instead of the
        // "BUS MASTER" label (68 - 20) and two extra rows/gaps (2 × 3).
        assert!(input_fixed > output_fixed);
        assert!(
            (input_fixed - output_fixed - (14.0 + 18.0 + 68.0 - 20.0 + 2.0 * layout::row_gap(&t)))
                .abs()
                < 0.01,
            "{input_fixed} vs {output_fixed}"
        );
        // With its 150 px fader an input strip is ~397 px tall at rest.
        let natural = layout::strip_natural_height(&t, StripType::Input, 3);
        assert!((405.0..425.0).contains(&natural), "natural height {natural}");
    }

    #[test]
    fn the_demo_window_width_is_the_qt_width() {
        let state = AppState::demo();
        let (inputs, outputs) = layout::counts(&state.strips);
        // 452 (4 inputs) + 342 (3 outputs) = 794 px — the same number the Qt
        // `_adjust_window_size` passes to `setFixedWidth`.
        assert_eq!((inputs, outputs), (4, 3));
        assert_eq!(layout::required_width(inputs, outputs), 794);
        // The theme's default window is wide enough for it (so a capture at
        // 800 px shows the columns at their true width).
        assert!(794.0 <= state.theme.theme.dimensions.window_width);
    }

    /// Strips fill the window: taller window, taller strip, and never past the
    /// status line.  (Both sections get this same number — that invariant is
    /// what keeps the two columns aligned.)
    #[test]
    fn the_strip_height_follows_the_window_height() {
        let t = Theme::holaf_dark();
        let mut previous = 0.0;
        // 373 = `layout::minimum_window_height(&t, 3)`: the shortest window the
        // WM is allowed to make, in which the fader still has its 40 px.
        for window_h in [373.0_f32, 420.0, 600.0, 900.0, 1440.0] {
            let h = layout::strip_height_for(&t, window_h, 3);
            assert!(h >= layout::strip_min_height(&t, StripType::Input, 3));
            assert!(h > previous, "{h} must grow with the window ({window_h})");
            // Strip top (header) + strip + bottom margin + status line = window.
            assert!((40.0 + h + 10.0 + 18.0 - window_h).abs() < 0.01);
            previous = h;
        }
        // Below the floor the strip stops shrinking (the fader keeps 40 px).
        let squeezed = layout::strip_height_for(&t, 200.0, 3);
        assert_eq!(squeezed, layout::strip_min_height(&t, StripType::Input, 3));
        assert_eq!(squeezed, layout::strip_fixed_height(&t, StripType::Input, 3) + 40.0);
    }
}
