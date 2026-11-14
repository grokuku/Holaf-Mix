//! Vertical fader with an integrated VU meter pair.
//!
//! Reproduces `strip_widget.StripWidget`'s fader row: `[VU left 6px] [slider]
//! [VU right 6px]`.  Qt gives this for free with `QSlider::groove` /
//! `QSlider::handle` styles plus a custom `paintEvent` for the meters; here the
//! whole row is one widget that allocates one interaction rect and paints
//! everything itself.
//!
//! Semantics kept from Qt:
//!
//! * the slider range is `0..=100` and the stored volume is `value / 100`
//!   (`_on_slider_move`), so the fader speaks the same unit as the model;
//! * dragging *and* clicking anywhere on the rail set the level (Qt jumps to
//!   the clicked position);
//! * the VU gradient is anchored to the **whole** rail and only revealed up to
//!   the current level, exactly like `VUMeterWidget.paintEvent` (which sets the
//!   gradient's start/stop from the widget rect, then clips by painting a
//!   shorter rect with the full gradient as the brush).

use egui::{Color32, Rect, Response, Sense, Shape, Ui, Vec2};

use crate::theme::paint;
use crate::theme::{HexColor, Theme};

/// Position of the yellow VU stop, measured from the top of the meter
/// (`gradient.setColorAt(0.2, QColor("#ffff33"))`).
const VU_MID_STOP: f32 = 0.2;

/// Colours and geometry of one fader row, snapshotted from the theme so the
/// paint code does not chase `&Theme` through nested calls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaderStyle {
    pub rail_bg: HexColor,
    pub rail_loaded: HexColor,
    pub handle: HexColor,
    pub vu_bg: HexColor,
    pub vu_low: HexColor,
    pub vu_mid: HexColor,
    pub vu_high: HexColor,
    pub vu_width: f32,
    pub rail_width: f32,
    pub handle_size: f32,
    pub fader_width: f32,
    /// Gap between a VU bar and the fader (`fader_area_layout.setSpacing(4)`).
    pub gap: f32,
}

impl Default for FaderStyle {
    fn default() -> Self {
        Self::from_theme(&Theme::holaf_dark())
    }
}

impl FaderStyle {
    pub fn from_theme(theme: &Theme) -> Self {
        let c = &theme.colors;
        let d = &theme.dimensions;
        Self {
            rail_bg: c.rail_bg,
            rail_loaded: c.rail_loaded,
            handle: c.handle,
            vu_bg: c.vu_bg,
            vu_low: c.vu_low,
            vu_mid: c.vu_mid,
            vu_high: c.vu_high,
            vu_width: d.vu_width,
            rail_width: d.rail_width,
            handle_size: d.handle_size,
            fader_width: d.fader_width,
            gap: 4.0,
        }
    }

    /// Total width of the row: VU + gap + fader + gap + VU.
    pub fn total_width(&self) -> f32 {
        self.vu_width * 2.0 + self.fader_width + self.gap * 2.0
    }

    /// The VU gradient, **top → bottom**, as Qt declares it in
    /// `VUMeterWidget.__init__`:
    ///
    /// ```text
    /// setColorAt(0.0, #ff3333)   red at the top
    /// setColorAt(0.2, #ffff33)   yellow at 20 %
    /// setColorAt(1.0, #33ff33)   green at the bottom
    /// ```
    ///
    /// The middle stop is at **0.2**, not 0.5: the red band is thin, the
    /// yellow one is short, and the meter is mostly green.
    pub fn vu_stops(&self) -> [(f32, HexColor); 3] {
        [
            (0.0, self.vu_high),
            (VU_MID_STOP, self.vu_mid),
            (1.0, self.vu_low),
        ]
    }

    /// Minimum height that still leaves a usable rail (two handles' worth).
    pub fn min_height(&self) -> f32 {
        self.handle_size * 2.0 + 8.0
    }
}

/// What the caller needs to know after drawing a fader.
#[derive(Debug)]
pub struct FaderOutput {
    pub response: Response,
    /// `true` when the user moved the fader this frame (the caller should push
    /// the new volume to the engine and mark the state dirty).
    pub changed: bool,
}

/// Draw `[VU][fader][VU]` and let the user drag `volume` (0.0–1.0).
///
/// `levels` are the current L/R meter values in `0.0–1.0`; they are **input
/// only** (the engine owns them), so a mono strip shows the same value twice —
/// the caller applies that rule, like `StripWidget.update_vumeter` does.
pub fn fader_with_vu(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    volume: &mut f32,
    levels: [f32; 2],
    style: &FaderStyle,
    height: f32,
) -> FaderOutput {
    // One interaction id per strip: `push_id` scopes the auto-generated id of
    // the allocation below, so two faders on screen never share one.
    ui.push_id(id_salt, |ui| {
        fader_inner(ui, volume, levels, style, height)
    })
    .inner
}

/// The fader body, without the id scoping (see [`fader_with_vu`]).
fn fader_inner(
    ui: &mut Ui,
    volume: &mut f32,
    levels: [f32; 2],
    style: &FaderStyle,
    height: f32,
) -> FaderOutput {
    // Never force a minimum here: a short strip must still show its MONO/
    // MUTE/MIDI rows, so the fader gives up height instead of pushing them out.
    let size = Vec2::new(style.total_width(), height.max(20.0));
    let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());

    // Rail geometry: keep the handle fully inside the allocated rect so it can
    // never overlap the row above/below.
    let half_handle = style.handle_size / 2.0;
    let rail_rect = Rect::from_min_max(
        egui::pos2(
            rect.center().x - style.rail_width / 2.0,
            rect.top() + half_handle,
        ),
        egui::pos2(
            rect.center().x + style.rail_width / 2.0,
            rect.bottom() - half_handle,
        ),
    );

    let mut changed = false;
    // Qt jumps to the clicked position and then follows the drag; `dragged()`
    // covers both because egui reports a drag for a click that moved, so we
    // also honour a plain click.
    let interacting = response.dragged() || response.clicked();
    if interacting {
        if let Some(pointer) = response.interact_pointer_pos() {
            let span = rail_rect.height();
            if span > 0.0 {
                let new_value = ((rail_rect.bottom() - pointer.y) / span).clamp(0.0, 1.0);
                if (new_value - *volume).abs() > f32::EPSILON {
                    *volume = new_value;
                    changed = true;
                }
            }
        }
    }
    let value = volume.clamp(0.0, 1.0);

    let painter = ui.painter().with_clip_rect(rect);
    // Top → bottom: red at the top, green at the bottom (Qt's
    // `setColorAt(0, #ff3333)` … `setColorAt(1, #33ff33)`).
    let stops = style.vu_stops();

    // --- VU bars -------------------------------------------------------
    let vu_height = rect.height();
    let left_rect = Rect::from_min_size(
        egui::pos2(rect.left(), rect.top() + half_handle),
        Vec2::new(style.vu_width, vu_height - style.handle_size),
    );
    let right_rect = Rect::from_min_max(
        egui::pos2(rect.right() - style.vu_width, left_rect.top()),
        egui::pos2(rect.right(), left_rect.bottom()),
    );
    for (bar, level) in [(left_rect, levels[0]), (right_rect, levels[1])] {
        paint_vu(&painter, bar, level, &stops, style.vu_bg);
    }

    // --- rail ----------------------------------------------------------
    paint::fill_rect(
        &painter,
        rail_rect,
        crate::theme::Fill::solid(style.rail_bg),
        style.rail_width / 2.0,
    );
    // The part below the handle is the "loaded" part (`add-page`).
    let handle_center_y = rail_rect.bottom() - rail_rect.height() * value;
    let loaded_rect = Rect::from_min_max(
        egui::pos2(rail_rect.left(), handle_center_y),
        rail_rect.right_bottom(),
    );
    if loaded_rect.height() > 0.5 {
        paint::fill_rect(
            &painter,
            loaded_rect,
            crate::theme::Fill::solid(style.rail_loaded),
            style.rail_width / 2.0,
        );
    }

    // --- handle --------------------------------------------------------
    let handle_rect = Rect::from_center_size(
        egui::pos2(rect.center().x, handle_center_y),
        Vec2::new(style.fader_width, style.handle_size),
    );
    paint::fill_rect(
        &painter,
        handle_rect,
        crate::theme::Fill::solid(style.handle),
        3.0,
    );
    // Qt only changes the cursor on hover; here the handle also gets a faint
    // dark outline so it stays visible against the light "loaded" groove.
    if response.hovered() || response.dragged() {
        painter.rect_stroke(
            handle_rect,
            paint::corner(3.0),
            egui::Stroke::new(1.0, style.rail_bg.scale_alpha(0.6).color32()),
            egui::StrokeKind::Inside,
        );
    }

    FaderOutput { response, changed }
}

/// One VU bar: background plus the revealed part of a bottom→top gradient.
fn paint_vu(
    painter: &egui::Painter,
    rect: Rect,
    level: f32,
    stops: &[(f32, HexColor)],
    background: HexColor,
) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    painter.rect_filled(rect, egui::CornerRadius::ZERO, background.color32());
    let level = level.clamp(0.0, 1.0);
    if level <= 0.0 {
        return;
    }
    // The gradient always spans the full bar; only the bottom `level` fraction
    // is visible, so the colours at a given height never shift as the level
    // moves (this is what makes a VU meter readable).
    let filled = Rect::from_min_max(
        egui::pos2(rect.left(), rect.bottom() - rect.height() * level),
        rect.right_bottom(),
    );
    let clipped = painter.with_clip_rect(filled);
    clipped.add(Shape::mesh(paint::vertical_gradient_rect(rect, stops)));
}

/// Colour of the VU gradient at a normalised position (bottom = 0, top = 1).
/// Exposed for tests: it is the only non-trivial colour logic in this file.
pub fn vu_color_at(level: f32, style: &FaderStyle) -> Color32 {
    paint::sample_stops(&style.vu_stops(), 1.0 - level).color32()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> FaderStyle {
        FaderStyle::from_theme(&Theme::holaf_dark())
    }

    #[test]
    fn style_matches_the_measured_dimensions() {
        let s = style();
        assert_eq!(s.vu_width, 6.0);
        assert_eq!(s.rail_width, 6.0);
        assert_eq!(s.handle_size, 14.0);
        assert_eq!(s.vu_bg, HexColor::parse("#222222").unwrap());
        assert_eq!(s.vu_low, HexColor::parse("#33ff33").unwrap());
        assert_eq!(s.vu_mid, HexColor::parse("#ffff33").unwrap());
        assert_eq!(s.vu_high, HexColor::parse("#ff3333").unwrap());
        assert_eq!(s.rail_bg, HexColor::parse("#222222").unwrap());
        assert_eq!(s.rail_loaded, HexColor::parse("#555555").unwrap());
        assert_eq!(s.handle, HexColor::parse("#ffffff").unwrap());
    }

    #[test]
    fn total_width_is_the_sum_of_the_row() {
        let s = style();
        // 6 + 4 + 20 + 4 + 6 = 40 — narrower than the 100 px strip, so the row
        // fits with the 5 px strip padding on each side.
        assert_eq!(s.total_width(), 40.0);
        assert!(s.total_width() + 2.0 * 5.0 <= 100.0);
    }

    #[test]
    fn min_height_keeps_two_handles_visible() {
        let s = style();
        assert!(s.min_height() >= 2.0 * s.handle_size);
    }

    #[test]
    fn vu_colour_goes_green_yellow_red_from_bottom_to_top() {
        let s = style();
        assert_eq!(vu_color_at(0.0, &s), HexColor::parse("#33ff33").unwrap().color32());
        assert_eq!(vu_color_at(1.0, &s), HexColor::parse("#ff3333").unwrap().color32());
        // The yellow stop sits 20 % below the top (`setColorAt(0.2, #ffff33)`),
        // so 80 % on the meter is exactly the yellow, not 50 %.
        assert_eq!(vu_color_at(0.8, &s), HexColor::parse("#ffff33").unwrap().color32());
        let mid = vu_color_at(0.5, &s);
        assert!(mid.g() > 200 && mid.r() > 60, "half scale is a yellow-green: {mid:?}");
        // Below the yellow stop, everything is a green→yellow ramp.
        let low = vu_color_at(0.25, &s);
        assert!(low.g() > 200 && low.r() > 100, "unexpected colour {low:?}");
    }

    /// End-to-end: a fader drawn headless and rasterised must show a green VU
    /// fill at the bottom, the dark rail and the white handle.
    #[test]
    fn a_drawn_fader_shows_vu_ink_and_a_handle() {
        use crate::raster::{render, Framebuffer, Textures};
        use egui::Color32;

        let ctx = crate::test_support::headless_context();
        let style = style();
        let mut volume = 0.5_f32;
        let primitives = crate::test_support::run_pass(
            &ctx,
            egui::vec2(44.0, 104.0),
            1.0,
            |ui| {
                fader_with_vu(ui, "test", &mut volume, [0.8, 0.8], &style, 100.0);
            },
        );
        let textures = Textures::from_context(&ctx);
        let mut fb = Framebuffer::new(44, 104, Color32::BLACK);
        render(&mut fb, &primitives, &textures, 1.0);

        let count = |pred: &dyn Fn([f32; 4]) -> bool| fb.pixels.iter().filter(|p| pred(**p)).count();
        let green = count(&|p| p[1] > 0.6 && p[0] < 0.6 && p[2] < 0.6);
        let white = count(&|p| p[0] > 0.9 && p[1] > 0.9 && p[2] > 0.9);
        let dark = count(&|p| p[0] < 0.2 && p[1] < 0.2 && p[2] < 0.2);
        assert!(green > 200, "the VU fill must be visible, found {green} green pixels");
        assert!(white > 50, "the handle must be visible, found {white} white pixels");
        assert!(dark > 50, "the rail and the VU background must be visible, {dark}");
    }

    #[test]
    fn default_style_is_the_built_in_theme() {
        assert_eq!(FaderStyle::default(), style());
    }
}
