//! Window geometry: the Qt width formula, the horizontal lock, and the vertical
//! stretch of the strips.
//!
//! # The two sizing rules (user decision, 2026-11-14)
//!
//! 1. **Horizontal — fixed, derived from the strips.**  The user cannot drag the
//!    window wider or narrower: the width is `Σ section_width(count)` exactly as
//!    `MainWindow._adjust_window_size` computes it in the PySide6 build, so it
//!    follows the strip count automatically (adding/removing a strip resizes the
//!    window by one `strip_width + gap`).
//! 2. **Vertical — free, and the strips fill it.**  The user resizes the window
//!    vertically; the strips stretch to the available height and the **fader
//!    absorbs every extra pixel** while the button rows keep their fixed theme
//!    heights.
//!
//! # Why a `ViewportCommand` and not `with_min_inner_size`/`with_max_inner_size`
//!
//! Pinning `min_inner_size == max_inner_size` would be one line, but the window
//! manager owns the drag: on several WMs/compositors the constraints are applied
//! *after* the resize (the window visibly stretches then snaps back, and on
//! tiling WMs it can be ignored altogether).  Re-asserting the width from
//! `Context::send_viewport_cmd(ViewportCommand::InnerSize(..))` is deterministic
//! on every backend, keeps the height the user picked, and is trivially testable
//! headless (see `enforce_window_width`'s test).  `min_inner_size` is still used,
//! for the *height* only, as a floor below which the fixed rows would not fit.
//!
//! # Anti feedback loop
//!
//! `InnerSize` is sent **only when the measured width differs from the required
//! one by more than [`WIDTH_TOLERANCE`]** (2 px — a WM may land one point off
//! because of rounding).  A frame in which the window already has the right
//! width sends nothing, so the resize request cannot ping-pong with the
//! compositor.

use egui::{Context, Vec2, ViewportCommand};
use holaf_core::model::{Strip, StripType};

use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Qt constants (`MainWindow._adjust_window_size`)
// ---------------------------------------------------------------------------

/// `STRIP_WIDTH = 100` — also `StripWidget.setFixedWidth(100)`.
pub const QT_STRIP_WIDTH: f32 = 100.0;
/// `SPACING = 10` — `content_layout.setSpacing(10)` in `_create_section`.
pub const QT_STRIP_SPACING: f32 = 10.0;
/// `MARGIN_TOTAL_PER_SECTION = 22` — `setContentsMargins(10, 10, 10, 10)` plus
/// the 2 px of slack the Qt formula keeps (a `QScrollArea` sits in between).
pub const QT_SECTION_MARGIN_TOTAL: f32 = 22.0;
/// `MIN_SECTION_WIDTH = 160`.
pub const QT_MIN_SECTION_WIDTH: f32 = 160.0;

/// Smallest fader height when the window is squeezed.  Below this the fader
/// would be unusable (the handle is 14 px tall), so the strip stops shrinking
/// and the rows below it are clipped instead — see `views::strip`.
pub const MIN_FADER_HEIGHT: f32 = 40.0;

/// Height reserved at the bottom of the window for the status line, so a strip
/// stretched to the bottom never hides its MIDI button underneath it.
pub const FOOTER_HEIGHT: f32 = 18.0;

/// Width difference (points) tolerated before asking the WM for a resize.
pub const WIDTH_TOLERANCE: f32 = 2.0;

/// `MONO` / `MUTE` row height (the Qt buttons are `setFixedHeight(20)`).
pub const CONTROL_ROW_HEIGHT: f32 = 20.0;

/// `BUS MASTER` label row of an output strip.
pub const BUS_MASTER_HEIGHT: f32 = 20.0;

// ---------------------------------------------------------------------------
// Horizontal geometry
// ---------------------------------------------------------------------------

/// Width one section needs for `strip_count` strips — the Qt formula, verbatim:
///
/// ```text
/// section_width(n) = max( n*STRIP_WIDTH + max(0, n-1)*SPACING + 22 , 160 )
/// ```
///
/// The empty case is the `count == 0` early return of `calculate_section_width`
/// and yields the 160 px floor.  1 strip (120 px of content) also lands on the
/// floor, which is why an almost-empty window does not collapse.
pub fn section_width(strip_count: usize) -> u32 {
    if strip_count == 0 {
        return QT_MIN_SECTION_WIDTH as u32;
    }
    let n = strip_count as f32;
    let total = n * QT_STRIP_WIDTH + (n - 1.0).max(0.0) * QT_STRIP_SPACING + QT_SECTION_MARGIN_TOTAL;
    total.max(QT_MIN_SECTION_WIDTH) as u32
}

/// `(inputs, outputs)` — the two section counts `_adjust_window_size` reads from
/// `content_layout.count()`.
pub fn counts(strips: &[Strip]) -> (usize, usize) {
    let inputs = strips
        .iter()
        .filter(|s| s.kind == StripType::Input)
        .count();
    (inputs, strips.len() - inputs)
}

/// Total window width for a given pair of section counts:
/// `window_width = section_inputs_width + section_outputs_width`.
pub fn required_width(inputs: usize, outputs: usize) -> u32 {
    section_width(inputs) + section_width(outputs)
}

/// [`required_width`] for a live strip list — what `setFixedWidth` would be
/// called with after this `refresh_ui`.
pub fn required_width_for(strips: &[Strip]) -> u32 {
    let (inputs, outputs) = counts(strips);
    required_width(inputs, outputs)
}

/// Decide whether the window must be re-sized horizontally.
///
/// Returns `Some(width)` when `|current - required| >` [`WIDTH_TOLERANCE`], and
/// `None` when the window is already (within rounding) the right width — the
/// anti-feedback-loop rule.  Pure, so the decision is unit-tested without a
/// window.
pub fn width_correction(current_width: f32, required: u32) -> Option<f32> {
    if required == 0 || !current_width.is_finite() {
        return None;
    }
    let required = required as f32;
    if (current_width - required).abs() > WIDTH_TOLERANCE {
        Some(required)
    } else {
        None
    }
}

/// Keep the window at [`required_width_for`] `strips`, **preserving the current
/// height** (only the width is locked; the user owns the vertical axis).
///
/// Called once per frame from [`crate::render`]: it is cheap (two rect reads and
/// a comparison) and it self-corrects when a strip is added, deleted or when the
/// WM ignored a previous request.
pub fn enforce_window_width(ctx: &Context, strips: &[Strip]) {
    let current = ctx.content_rect();
    if let Some(width) = width_correction(current.width(), required_width_for(strips)) {
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(
            width,
            current.height().max(1.0),
        )));
    }
}

/// Smallest window width: two empty sections.  Only reachable with no strip.
pub fn minimum_window_width() -> f32 {
    2.0 * QT_MIN_SECTION_WIDTH
}

// ---------------------------------------------------------------------------
// Vertical geometry
// ---------------------------------------------------------------------------

/// Vertical gap between two rows of a strip: literally `layout.setSpacing(5)`
/// from `strip_widget.py`.
///
/// [`views::strip`](crate::views::strip) zeroes egui's own
/// `Spacing::item_spacing.y` for its content `Ui`, otherwise egui would add its
/// 3 px default gap *on top of* every allocation and the strip would space its
/// rows at 8 px instead of the theme's 5.  With that zeroed, the theme token is
/// the whole gap — so this function is also the exact per-row term of the height
/// model below, and the drawn strip matches [`strip_fixed_height`] pixel for
/// pixel (asserted by `the_height_model_matches_what_is_drawn`).
pub fn row_gap(theme: &Theme) -> f32 {
    theme.dimensions.strip_spacing.max(0.0)
}

/// Height of a section header (title + "+" button + margins):
/// `header_layout.setContentsMargins(10, 10, 10, 0)` + a 30 px button.
pub fn header_height(theme: &Theme) -> f32 {
    theme.dimensions.add_button_size + theme.dimensions.section_padding
}

/// Routing plate height for `targets` output strips: one 20 px chip per target,
/// 2 px apart, 2 px of padding all round (`routing_layout.setContentsMargins(2,2,2,2)`).
pub fn routing_plate_height(theme: &Theme, targets: usize) -> f32 {
    if targets == 0 {
        return 18.0; // "No Outputs" placeholder
    }
    let n = targets as f32;
    n * theme.dimensions.route_button_height + (n - 1.0) * 2.0 + 4.0
}

/// Height of the rows **above** the fader for exactly the rows that are drawn.
///
/// Must stay in sync with the drawing order of `views::strip::strip_ui`
/// (header → caption → combo → [inputs: checkbox, SELECT APPS, routing plate |
/// outputs: BUS MASTER] → gap → *fader*).  `apps_row` is
/// `_update_app_btn_visibility`: the `SELECT APPS` row only exists on a virtual,
/// non-default input, so the same kind can be 23 px shorter.
pub fn rows_before_fader(
    theme: &Theme,
    kind: StripType,
    apps_row: bool,
    route_targets: usize,
) -> f32 {
    let d = &theme.dimensions;
    let t = &theme.typography;
    let mut rows: Vec<f32> = vec![
        t.body + 6.0,       // header: label + "×"
        t.tiny + 4.0,       // caption (SOURCE IN / DEVICE OUT)
        d.combo_height,     // device combo
        d.fx_button_height, // FX toggles
    ];
    match kind {
        StripType::Input => {
            rows.push(d.checkbox_height); // "Default Sink"
            if apps_row {
                rows.push(d.combo_height); // "SELECT APPS"
            }
            rows.push(routing_plate_height(theme, route_targets));
        }
        StripType::Output => rows.push(BUS_MASTER_HEIGHT),
    }
    // One gap between each pair of rows + the gap that separates the last row
    // from the fader.
    let sum: f32 = rows.iter().sum();
    sum + row_gap(theme) * rows.len() as f32
}

/// Worst case of [`rows_before_fader`]: every optional row drawn (`SELECT APPS`
/// included).  Used by the height floors, which must hold whatever the strip is.
pub fn strip_rows_before_fader(theme: &Theme, kind: StripType, route_targets: usize) -> f32 {
    rows_before_fader(theme, kind, true, route_targets)
}

/// Height of the rows **below** the fader: gap, MONO/MUTE, gap, MIDI.
pub fn strip_rows_after_fader(theme: &Theme) -> f32 {
    let gap = row_gap(theme);
    gap + CONTROL_ROW_HEIGHT + gap + theme.dimensions.midi_button_height
}

/// Every fixed row of a strip (fader excluded), paddings included — the Qt
/// `sizeHint` minus the fader.
pub fn strip_fixed_height(theme: &Theme, kind: StripType, route_targets: usize) -> f32 {
    strip_rows_before_fader(theme, kind, route_targets)
        + strip_rows_after_fader(theme)
        + theme.dimensions.strip_padding
        + theme.dimensions.strip_padding_bottom
}

/// [`fader_height_for`] for the rows a strip actually draws — `apps_row` tells
/// whether the optional `SELECT APPS` row is there.
///
/// `views::strip` measures what it drew (`Ui::available_height`) rather than
/// trusting this, but it debug-asserts the two agree, so the model can never
/// drift away from the picture.
pub fn fader_height_for_case(
    theme: &Theme,
    kind: StripType,
    apps_row: bool,
    route_targets: usize,
    strip_height: f32,
) -> f32 {
    let fixed = rows_before_fader(theme, kind, apps_row, route_targets)
        + strip_rows_after_fader(theme)
        + theme.dimensions.strip_padding
        + theme.dimensions.strip_padding_bottom;
    (strip_height - fixed).max(MIN_FADER_HEIGHT)
}

/// Natural (Qt `sizeHint`) height of a strip: fixed rows + the theme's default
/// fader.  The strip is **not** held to this height any more — it fills the
/// available space — but the number is still what the Qt build shows at rest,
/// and it is the floor of [`strip_min_height`].
pub fn strip_natural_height(theme: &Theme, kind: StripType, route_targets: usize) -> f32 {
    strip_fixed_height(theme, kind, route_targets) + theme.dimensions.fader_height
}

/// Shortest height a strip may be drawn at: every fixed row + the minimum fader.
pub fn strip_min_height(theme: &Theme, kind: StripType, route_targets: usize) -> f32 {
    strip_fixed_height(theme, kind, route_targets) + MIN_FADER_HEIGHT
}

/// Height the fader gets when the strip body is `strip_height` tall.
///
/// This is the whole vertical rule in one line: the fixed rows are subtracted
/// from the strip and the **fader takes the rest** (never less than
/// [`MIN_FADER_HEIGHT`]; when the window is squeezed below that the strip
/// clips its last rows instead of drawing a 2 px fader).
///
/// *Model, not the drawn value*: `views::strip` measures the rows it actually
/// drew (a strip whose `SELECT APPS` row is hidden has 21 px more fader), so the
/// drawn fader of a virtual-and-default input is 21 px taller than this
/// function says.  The formula is the worst case (every optional row shown),
/// which is what the height floors below must be built on.
pub fn fader_height_for(
    theme: &Theme,
    kind: StripType,
    route_targets: usize,
    strip_height: f32,
) -> f32 {
    (strip_height - strip_fixed_height(theme, kind, route_targets)).max(MIN_FADER_HEIGHT)
}

/// Height available to a strip inside a window `window_height` tall: the window
/// minus the section header, the section's bottom margin and the status line.
pub fn available_strip_height(theme: &Theme, window_height: f32) -> f32 {
    (window_height - header_height(theme) - theme.dimensions.section_padding - FOOTER_HEIGHT)
        .max(0.0)
}

/// Height every strip is drawn at for a window `window_height` tall: all the
/// room the window offers, floored at [`strip_min_height`] so a window squeezed
/// below the floor clips the strip's last rows instead of crushing the fader.
///
/// This is the one number the main window hands to every strip of both
/// sections — which is what keeps INPUTS and OUTPUTS (and the strips inside a
/// section) exactly aligned.
pub fn strip_height_for(theme: &Theme, window_height: f32, route_targets: usize) -> f32 {
    available_strip_height(theme, window_height)
        .max(strip_min_height(theme, StripType::Input, route_targets))
}

/// Smallest window height that still shows a whole input strip with the minimum
/// fader.  Used for `ViewportBuilder::with_min_inner_size` so the WM cannot
/// shrink the window into the clipping regime.
pub fn minimum_window_height(theme: &Theme, route_targets: usize) -> f32 {
    header_height(theme)
        + strip_min_height(theme, StripType::Input, route_targets)
        + theme.dimensions.section_padding
        + FOOTER_HEIGHT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn demo_strips() -> Vec<Strip> {
        crate::app::AppState::demo().strips
    }

    #[test]
    fn section_width_reproduces_the_qt_formula() {
        // `min(n*100 + max(0, n-1)*10 + 22, 160)` with `MIN_SECTION_WIDTH = 160`.
        assert_eq!(section_width(0), 160, "empty section → the 160 px floor");
        assert_eq!(section_width(1), 160, "100 + 22 = 122 → floor");
        assert_eq!(section_width(2), 232, "232 > 160");
        assert_eq!(section_width(3), 342);
        assert_eq!(section_width(4), 452);
        // 7 strips: 700 + 6*10 + 22.
        assert_eq!(section_width(7), 782);
        // Monotonic above the floor, and the floor is the minimum.
        for n in 0..40 {
            assert!(section_width(n) >= 160);
            assert!(section_width(n + 1) > section_width(n) || section_width(n) == 160);
        }
    }

    #[test]
    fn required_width_is_the_sum_of_the_two_sections() {
        // The shipped configuration: 4 inputs + 3 outputs.
        assert_eq!(required_width(4, 3), 452 + 342);
        assert_eq!(required_width(4, 3), 794);
        // Empty sections keep their floor.
        assert_eq!(required_width(0, 0), 320);
        assert_eq!(required_width(0, 4), 612);
        assert_eq!(required_width(1, 1), 320);
        // One more strip widens the window by exactly `strip_width + gap`.
        assert_eq!(required_width(5, 3) - required_width(4, 3), 110);
    }

    #[test]
    fn required_width_for_a_strip_list_counts_by_kind() {
        let strips = demo_strips();
        assert_eq!(counts(&strips), (4, 3));
        assert_eq!(required_width_for(&strips), 794);
        assert_eq!(required_width_for(&[]), 320);
    }

    #[test]
    fn width_correction_only_fires_past_the_tolerance() {
        // Already right (or within rounding): no command, no feedback loop.
        assert_eq!(width_correction(794.0, 794), None);
        assert_eq!(width_correction(795.5, 794), None);
        assert_eq!(width_correction(792.0, 794), None, "exactly 2 px is tolerated");
        // Off by more than the tolerance: ask for the exact width.
        assert_eq!(width_correction(800.0, 794), Some(794.0));
        assert_eq!(width_correction(452.0, 794), Some(794.0));
        assert_eq!(width_correction(0.0, 794), Some(794.0));
        // Degenerate input never produces a command.
        assert_eq!(width_correction(f32::NAN, 794), None);
        assert_eq!(width_correction(400.0, 0), None);
    }

    #[test]
    fn a_wrong_width_is_corrected_with_a_viewport_command_and_the_height_is_kept() {
        // A plain `egui::Context` records the command in its `FullOutput`
        // (`viewport_output[ROOT].commands`), so the lock is testable without a
        // window manager.
        let ctx = crate::test_support::headless_context();
        let strips = demo_strips();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 640.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |_ui| {
            enforce_window_width(&ctx, &strips);
        });
        output.textures_delta.clear();
        let commands = &output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("the root viewport is always reported")
            .commands;
        assert_eq!(
            commands,
            &vec![ViewportCommand::InnerSize(egui::vec2(794.0, 640.0))],
            "1200 px wide window must be pulled back to 794 px, height untouched"
        );
    }

    #[test]
    fn a_frame_with_the_right_width_sends_no_command() {
        let ctx = crate::test_support::headless_context();
        let strips = demo_strips();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(794.0, 720.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |_ui| {
            enforce_window_width(&ctx, &strips);
        });
        output.textures_delta.clear();
        let commands = &output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("the root viewport is always reported")
            .commands;
        assert!(
            commands.is_empty(),
            "already 794 px wide: {commands:?} would fight the compositor"
        );
    }

    #[test]
    fn header_and_rows_add_up_to_the_qt_geometry() {
        let t = Theme::holaf_dark();
        assert_eq!(header_height(&t), 40.0, "30 px button + 10 px margin");
        // 3 routing targets: 3*20 + 2*2 + 4 = 68.
        assert_eq!(routing_plate_height(&t, 3), 68.0);
        assert_eq!(routing_plate_height(&t, 0), 18.0);
        // Inputs: 17 + 12 + 18 + 18 + 14 + 18 + 68 = 165, 7 gaps of 5.
        assert_eq!(strip_rows_before_fader(&t, StripType::Input, 3), 165.0 + 35.0);
        // Outputs: 17 + 12 + 18 + 18 + 20 = 85, 5 gaps of 5.
        assert_eq!(
            strip_rows_before_fader(&t, StripType::Output, 3),
            85.0 + 25.0
        );
        // Below the fader: gap + MONO/MUTE + gap + MIDI = 5 + 20 + 5 + 20.
        assert_eq!(strip_rows_after_fader(&t), 50.0);
        // + strip padding 5 + bottom padding 10.
        assert_eq!(strip_fixed_height(&t, StripType::Input, 3), 200.0 + 50.0 + 15.0);
        assert_eq!(
            strip_fixed_height(&t, StripType::Output, 3),
            110.0 + 50.0 + 15.0
        );
    }

    /// What the demo strips give: `SELECT APPS` only on a virtual, non-default
    /// input (`_update_app_btn_visibility`), which is worth 23 px of fader.
    #[test]
    fn the_optional_apps_row_is_worth_its_height_and_its_gap() {
        let t = Theme::holaf_dark();
        let with = rows_before_fader(&t, StripType::Input, true, 3);
        let without = rows_before_fader(&t, StripType::Input, false, 3);
        assert_eq!(with - without, t.dimensions.combo_height + row_gap(&t));
        // Same strip height, the shorter layout hands 23 px to the fader.
        let a = fader_height_for_case(&t, StripType::Input, true, 3, 532.0);
        let b = fader_height_for_case(&t, StripType::Input, false, 3, 532.0);
        assert_eq!(b - a, 23.0);
        // `fader_height_for` is the worst case, so it matches the full layout.
        assert_eq!(a, fader_height_for(&t, StripType::Input, 3, 532.0));
    }

    #[test]
    fn the_fader_absorbs_the_window_height() {
        let t = Theme::holaf_dark();
        // A 600 px window: 600 - 40 (header) - 10 (margin) - 18 (status line).
        assert_eq!(available_strip_height(&t, 600.0), 532.0);
        let short = fader_height_for(&t, StripType::Input, 3, available_strip_height(&t, 600.0));
        assert_eq!(short, 532.0 - 265.0);
        // A 900 px window gives the fader 300 px more, nothing else moves.
        let tall = fader_height_for(&t, StripType::Input, 3, available_strip_height(&t, 900.0));
        assert_eq!(tall, 832.0 - 265.0);
        assert_eq!(tall - short, 300.0);
        // Both sections are the same height: an output strip just has a
        // shorter fixed part, so *its* fader is the one that is longer.
        let out = fader_height_for(&t, StripType::Output, 3, 532.0);
        assert_eq!(out, 532.0 - 175.0);
        // Below the floor the fader stops shrinking (the strip clips instead).
        assert_eq!(
            fader_height_for(&t, StripType::Input, 3, 120.0),
            MIN_FADER_HEIGHT
        );
    }

    #[test]
    fn natural_and_minimum_strip_heights_are_consistent() {
        let t = Theme::holaf_dark();
        let natural = strip_natural_height(&t, StripType::Input, 3);
        assert_eq!(natural, 265.0 + t.dimensions.fader_height);
        // The old port's ~394 px natural height is now only a resting size.
        assert!((405.0..425.0).contains(&natural), "{natural}");
        assert_eq!(strip_min_height(&t, StripType::Input, 3), natural - 110.0);
        // The minimum window height is the one that still fits that strip.
        assert_eq!(
            minimum_window_height(&t, 3),
            40.0 + 305.0 + 10.0 + 18.0
        );
        assert!(minimum_window_height(&t, 3) < 600.0);
        // The window floor is narrower than any real configuration.
        assert_eq!(minimum_window_width(), 320.0);
    }
}
