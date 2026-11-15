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

use egui::{Rect, Sense, Ui, UiBuilder, Vec2};

use holaf_core::model::StripType;

use crate::app::{AppState, UiAction};
use crate::layout;
use crate::theme::paint;
use crate::views::{strip_ui, text_centered};

/// Height of the "engine disconnected" banner drawn over the section headers.
///
/// 18 px is the smallest line the theme's `small` font fits in; the header is
/// 40 px tall and its content (title, "+") sits in the middle, so the banner
/// never hides a control.
pub const ENGINE_BANNER_HEIGHT: f32 = 18.0;

/// Gap between the bottom edge of the window and a transient notice.
pub const NOTICE_MARGIN: f32 = 12.0;

/// Draw the whole window into `ui`.
pub fn render(state: &mut AppState, ui: &mut Ui) {
    // `ui.max_rect()` is the *available* area (5000 × 5000 in a root Ui), not
    // the window: `content_rect()` is the real screen rectangle.
    let screen = ui.ctx().content_rect();
    let mut actions: Vec<UiAction> = Vec::new();

    // Right click anywhere that is not a control: the window context menu
    // ("Restart audio engine" / "Debug window" / "Open project page").
    //
    // egui gives a click to the **last** widget registered under the pointer, so
    // this interaction is registered before every strip and header: a strip's
    // own widgets keep their clicks and everything else (background, VU meters,
    // the empty space of a column) falls through to the application menu — the
    // Qt rule (`contextMenuEvent` + the `DefaultContextMenu` event filter).
    let background = ui.interact(
        screen,
        ui.scope_id().with("holaf-window-background"),
        Sense::click(),
    );
    if background.secondary_clicked() {
        let pos = ui
            .ctx()
            .input(|input| input.pointer.interact_pos())
            .unwrap_or(screen.center());
        actions.push(UiAction::OpenContextMenu {
            kind: crate::menus::ContextMenuKind::Window,
            pos,
        });
    }

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
                // The Qt "+" opens a name dialog (`QInputDialog.getText`) before
                // anything is added — `dialogs::draw_add_strip` is its twin.
                actions.push(UiAction::BeginAddStrip { kind });
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
                // The MIDI button carries three states: idle, `LEARNING...` while
                // a learn is armed for this strip, and the CC it just learned for
                // a moment afterwards.
                let flash = state.midi_button_text(&strip.uid);
                let summary = state.midi_summary(&strip.uid);
                let midi_button = crate::widgets::MidiButtonState {
                    armed: state
                        .midi_learning
                        .as_ref()
                        .filter(|(uid, _)| uid == &strip.uid)
                        .map(|(_, target)| *target),
                    flash: flash
                        .as_deref()
                        .filter(|_| !state.is_midi_learning(&strip.uid)),
                    tooltip: Some(summary.as_str()),
                };
                let presence = if strip.device_name.is_some() {
                    state.device_presence(&strip.uid)
                } else {
                    crate::devices::DevicePresence::Found
                };
                actions.extend(strip_ui(
                    ui,
                    rect,
                    &state.theme,
                    strip,
                    &outputs,
                    levels,
                    &mut state.image_cache,
                    midi_button,
                    presence,
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

    // Engine banner: "disconnected mode" is a first-class UI state, not a
    // console line.  Drawn over the (mostly empty) top of the section headers
    // so it costs no layout: the strips keep their geometry, the user keeps
    // every control, only the audio path is dead.
    if let Some(reason) = state.engine_error() {
        let theme = &state.theme.theme;
        let painter = ui.painter().with_clip_rect(screen);
        let banner =
            Rect::from_min_size(screen.min, Vec2::new(screen.width(), ENGINE_BANNER_HEIGHT));
        painter.rect_filled(
            banner,
            egui::CornerRadius::ZERO,
            egui::Color32::from_rgb(0x8b, 0x1a, 0x1a),
        );
        let text = format!(
            "engine disconnected - {reason} (strips are editable, audio actions are ignored)"
        );
        painter.text(
            egui::pos2(banner.left() + 6.0, banner.center().y),
            egui::Align2::LEFT_CENTER,
            &text,
            egui::FontId::proportional(theme.typography.small.min(11.0)),
            egui::Color32::WHITE,
        );
    }

    // A transient notice, drawn last so it sits on top of everything.
    //
    // There is **no status bar**: the Qt build had a permanent 18 px strip at the
    // bottom of the window and the user never asked for it (all it said was
    // "engine ready").  A real message — a failed config write, a learned MIDI
    // mapping, "Removed HP" — is still shown, as a small pill centred at the
    // bottom, and it disappears on its own ([`crate::app::STATUS_NOTICE`]).  When
    // there is nothing to say, nothing is drawn and the strips keep that space.
    if let Some(status) = state.transient_status() {
        let theme = &state.theme.theme;
        let painter = ui.painter().with_clip_rect(screen);
        let font = egui::FontId::proportional(theme.typography.small);
        let galley = painter.layout_no_wrap(
            status.to_string(),
            font,
            theme.colors.text_primary.color32(),
        );
        let pad = Vec2::new(10.0, 4.0);
        let size = galley.size() + pad * 2.0;
        let pill = Rect::from_center_size(
            egui::pos2(
                screen.center().x,
                screen.bottom() - size.y / 2.0 - NOTICE_MARGIN,
            ),
            size,
        );
        painter.rect_filled(
            pill,
            paint::corner(pill.height() / 2.0),
            egui::Color32::from_black_alpha(200),
        );
        painter.rect_stroke(
            pill,
            paint::corner(pill.height() / 2.0),
            egui::Stroke::new(1.0, theme.colors.combo_border.color32()),
            egui::StrokeKind::Inside,
        );
        painter.galley(pill.min + pad, galley, theme.colors.text_primary.color32());
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
        assert!(
            (405.0..425.0).contains(&natural),
            "natural height {natural}"
        );
    }

    /// A right click on the window background (here: the 10 px gap between two
    /// strips) must open the application menu — the Qt `contextMenuEvent` rule.
    /// The strips' own controls keep their clicks, which is why the background
    /// interaction is registered *before* them.
    #[test]
    fn a_right_click_on_the_background_opens_the_window_menu() {
        let ctx = crate::test_support::headless_context();
        let mut state = AppState::demo();
        // The demo has 4 inputs (column ends at x≈455 with an 800 px window) and
        // the first two outputs are separated by a `strip_gap` at x≈570: an area
        // where no widget of any strip lives.
        let pos = egui::pos2(570.0, 300.0);
        let run = |state: &mut AppState, time: f64, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                time: Some(time),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| crate::render(state, ui));
            output.textures_delta.clear();
        };
        // egui turns a click into a *press then release* over two frames: the
        // release frame is the one that reports `secondary_clicked()`.
        run(&mut state, 1.0, vec![egui::Event::PointerMoved(pos)]);
        run(
            &mut state,
            1.1,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            }],
        );
        run(
            &mut state,
            1.2,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        );
        match &state.context_menu {
            Some(menu) => assert_eq!(menu.kind, crate::menus::ContextMenuKind::Window),
            None => panic!("a right click on the background opened no menu"),
        }
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
    /// bottom margin.  (Both sections get this same number — that invariant is
    /// what keeps the two columns aligned.)
    #[test]
    fn the_strip_height_follows_the_window_height() {
        let t = Theme::holaf_dark();
        let mut previous = 0.0;
        // `minimum_window_height` is the shortest window the compositor is
        // allowed to make, and the fader still has its 40 px there.
        let floor = layout::minimum_window_height(&t, 3);
        for window_h in [floor, 420.0, 600.0, 900.0, 1440.0] {
            let h = layout::strip_height_for(&t, window_h, 3);
            assert!(h >= layout::strip_min_height(&t, StripType::Input, 3));
            assert!(h >= previous, "{h} must grow with the window ({window_h})");
            // Strip top (header) + strip + bottom margin = window.  There is no
            // status bar reserved any more.
            assert!((40.0 + h + 10.0 - window_h).abs() < 0.01);
            previous = h;
        }
        // Below the floor the strip stops shrinking (the fader keeps 40 px).
        let squeezed = layout::strip_height_for(&t, 200.0, 3);
        assert_eq!(squeezed, layout::strip_min_height(&t, StripType::Input, 3));
        assert_eq!(
            squeezed,
            layout::strip_fixed_height(&t, StripType::Input, 3) + 40.0
        );
    }

    /// There is **no status bar** any more.  Two renders of the same state, one
    /// with nothing to say and one with a fresh notice: only the pill area (and
    /// nothing along the bottom edge where the Qt bar used to be) may differ.
    #[test]
    fn there_is_no_status_bar_only_a_transient_notice() {
        use crate::raster::{render as rasterize, Framebuffer, Textures};
        use egui::Color32;

        let paint = |status: Option<&str>| {
            let ctx = crate::test_support::headless_context();
            let mut state = AppState::demo();
            state.status = status.map(str::to_string);
            let primitives =
                crate::test_support::run_pass(&ctx, Vec2::new(794.0, 600.0), 1.0, |ui| {
                    render(&mut state, ui)
                });
            let textures = Textures::from_context(&ctx);
            let mut fb = Framebuffer::new(794, 600, Color32::BLACK);
            rasterize(&mut fb, &primitives, &textures, 1.0);
            fb
        };
        let quiet = paint(None);
        let notice = paint(Some("Removed HP"));
        let differs = |x: usize, y: usize| {
            let a = quiet.pixels[y * 794 + x];
            let b = notice.pixels[y * 794 + x];
            (a[0] - b[0]).abs() > 1e-3 || (a[1] - b[1]).abs() > 1e-3
        };
        // The Qt status bar was a FULL-WIDTH band along the bottom edge, its
        // text starting at the far left: none of that may react to a message any
        // more.  (A pill is centred, so the corners stay untouched.)
        let left_edge: usize = (582..600)
            .map(|y| (0..40).filter(|x| differs(*x, y)).count())
            .sum();
        assert_eq!(
            left_edge, 0,
            "a full-width status bar is back at the bottom edge"
        );
        // The notice itself must be visible: a small pill, centred at the bottom.
        let pill: usize = (560..600)
            .map(|y| {
                (794 / 2 - 120..794 / 2 + 120)
                    .filter(|x| differs(*x, y))
                    .count()
            })
            .sum();
        assert!(
            pill > 200,
            "the transient notice is not painted ({pill} px)"
        );
    }

    /// "Disconnected mode" must be *visible*: the banner is a red band across the
    /// top of the window, drawn by the same `render` the app uses.
    #[test]
    fn the_disconnected_banner_is_painted() {
        use crate::app::EngineState;
        use crate::raster::{render as rasterize, Framebuffer, Textures};
        use egui::Color32;

        // Two renders of the same state, connected vs. disconnected: the only
        // pixels that may change are the banner band.
        let paint = |engine: EngineState| {
            let ctx = crate::test_support::headless_context();
            let mut state = AppState::demo();
            state.engine = engine;
            let primitives =
                crate::test_support::run_pass(&ctx, Vec2::new(800.0, 600.0), 1.0, |ui| {
                    render(&mut state, ui)
                });
            let textures = Textures::from_context(&ctx);
            let mut fb = Framebuffer::new(800, 600, Color32::BLACK);
            rasterize(&mut fb, &primitives, &textures, 1.0);
            fb
        };
        let connected = paint(EngineState::None);
        let disconnected = paint(EngineState::Disconnected(
            "cannot connect to PipeWire".to_string(),
        ));
        let differs = |y: usize| {
            (0..800)
                .filter(|x| {
                    let a = connected.pixels[y * 800 + x];
                    let b = disconnected.pixels[y * 800 + x];
                    (a[0] - b[0]).abs() > 1e-3 || (a[1] - b[1]).abs() > 1e-3
                })
                .count()
        };
        let band: usize = (0..ENGINE_BANNER_HEIGHT as usize).map(differs).sum();
        let below: usize = (ENGINE_BANNER_HEIGHT as usize + 1..600).map(differs).sum();
        assert!(
            band > 500,
            "the disconnected banner is not visible ({band} px)"
        );
        assert_eq!(below, 0, "the banner leaked out of its band");
    }
}
