//! Painting helpers shared by every view.
//!
//! Two things the raw `egui::Painter` cannot do that the PySide stylesheets
//! take for granted:
//!
//! 1. **Gradients with rounded corners.** `painter.rect_filled` takes a single
//!    colour; a gradient has to be a mesh. `QLinearGradient` + `border-radius`
//!    in a Qt stylesheet is free, so the port has to build the mesh itself —
//!    see [`fill_rect`], which triangulates a rounded rectangle as a fan with
//!    per-vertex colours (exact for a vertical linear gradient, because the
//!    interpolation is affine in `y`).
//! 2. **Image fitting.** `QPainter::drawImage` with a source rect is how the
//!    Python side crops/stretches a background image; [`image_mapping`] is the
//!    pure function computing the destination rect + UV rect for
//!    [`Fit`], unit-tested without a `Painter`.

use egui::epaint::{Mesh, Vertex, WHITE_UV};
use egui::{Color32, Painter, Pos2, Rect, Stroke, StrokeKind, TextureId, Vec2};

use super::{Fill, Fit, HexColor};

/// Number of segments used to approximate a quarter-circle corner.  Six keeps
/// the tessellation cheap (24 extra vertices per rounded rect) and is
/// indistinguishable from a real circle at the 5 px radius the strips use.
const CORNER_SEGMENTS: usize = 6;

/// Stroke width of a strip border, in points (PySide: `border: 2px solid`).
pub const STRIP_BORDER_WIDTH: f32 = 1.5;

/// Paint `rect` with `fill`, rounded to `radius` points.
///
/// Solid fills go through `rect_filled` (which is anti-aliased by epaint's
/// tessellator); gradients go through a hand-built mesh.  The mesh is not
/// feather-smooth on its own, so callers that care draw a
/// [`stroke_rect`] on top — every strip does.
pub fn fill_rect(painter: &Painter, rect: Rect, fill: Fill, radius: f32) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    match fill {
        Fill::Solid { color } => {
            painter.rect_filled(rect, corner(radius), color.color32());
        }
        Fill::VerticalGradient { .. } => {
            painter.add(egui::Shape::mesh(gradient_rounded_rect(rect, radius, fill)));
        }
    }
}

/// Same as [`fill_rect`] but never uses `rect_filled` — always the mesh path.
/// Used where a caller needs the exact same geometry for fill and image.
pub fn fill_rect_mesh(painter: &Painter, rect: Rect, fill: Fill, radius: f32) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    painter.add(egui::Shape::mesh(gradient_rounded_rect(rect, radius, fill)));
}

/// Border of `rect`, drawn inside the rectangle.
pub fn stroke_rect(painter: &Painter, rect: Rect, radius: f32, color: HexColor) {
    let stroke = Stroke::new(STRIP_BORDER_WIDTH, color.color32());
    painter.rect_stroke(rect, corner(radius), stroke, StrokeKind::Inside);
}

/// Build a rounded-rectangle mesh with a vertical gradient.
///
/// The fan is anchored on the rect centre (the shape is convex, so a fan is a
/// valid triangulation) and every rim vertex is coloured with
/// `fill.at(y_normalised)`; the centre gets `fill.at(0.5)`.  For a linear
/// gradient that is exact: colour is affine in `y`, and so is barycentric
/// interpolation over the fan.
pub fn gradient_rounded_rect(rect: Rect, radius: f32, fill: Fill) -> Mesh {
    let mut mesh = Mesh::default();
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return mesh; // degenerate: nothing to paint, not a zero-area triangle soup
    }
    let points = rounded_rect_points(rect, radius);
    if points.len() < 3 {
        return mesh;
    }

    let normalised_y = |p: Pos2| -> f32 {
        if rect.height() <= 0.0 {
            0.0
        } else {
            ((p.y - rect.top()) / rect.height()).clamp(0.0, 1.0)
        }
    };

    mesh.colored_vertex(rect.center(), fill.at(0.5).color32());
    for p in &points {
        mesh.colored_vertex(*p, fill.at(normalised_y(*p)).color32());
    }
    let n = points.len() as u32;
    for i in 0..n {
        let a = 1 + i;
        let b = 1 + (i + 1) % n;
        mesh.add_triangle(0, a, b);
    }
    mesh
}

/// A multi-stop vertical gradient filling `rect` (no rounding).
///
/// Used by the VU meter, whose band is green→yellow→red exactly like the
/// `QLinearGradient` in `strip_widget.VUMeterWidget`: the gradient spans the
/// whole rail and the level only decides how much of it is revealed.
pub fn vertical_gradient_rect(rect: Rect, stops: &[(f32, HexColor)]) -> Mesh {
    let mut mesh = Mesh::default();
    if stops.len() < 2 || rect.height() <= 0.0 {
        return mesh;
    }
    let color_at = |t: f32| -> Color32 { sample_stops(stops, t).color32() };
    for pair in stops.windows(2) {
        let (t0, _) = pair[0];
        let (t1, _) = pair[1];
        let y0 = rect.top() + rect.height() * t0.clamp(0.0, 1.0);
        let y1 = rect.top() + rect.height() * t1.clamp(0.0, 1.0);
        if y1 <= y0 {
            continue;
        }
        let quad = Rect::from_min_max(egui::pos2(rect.left(), y0), egui::pos2(rect.right(), y1));
        // Two triangles, four vertices, each corner coloured by its stop so
        // every band interpolates linearly inside itself.
        mesh.vertices.push(Vertex {
            pos: quad.left_top(),
            uv: WHITE_UV,
            color: color_at(t0),
        });
        mesh.vertices.push(Vertex {
            pos: quad.right_top(),
            uv: WHITE_UV,
            color: color_at(t0),
        });
        mesh.vertices.push(Vertex {
            pos: quad.right_bottom(),
            uv: WHITE_UV,
            color: color_at(t1),
        });
        mesh.vertices.push(Vertex {
            pos: quad.left_bottom(),
            uv: WHITE_UV,
            color: color_at(t1),
        });
        let base = mesh.vertices.len() as u32 - 4;
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    mesh
}

/// Colour of a stop list at `t` (`0.0..=1.0`), interpolating between stops and
/// clamping outside the outermost ones.
pub fn sample_stops(stops: &[(f32, HexColor)], t: f32) -> HexColor {
    debug_assert!(!stops.is_empty());
    let t = t.clamp(0.0, 1.0);
    if t <= stops[0].0 {
        return stops[0].1;
    }
    for pair in stops.windows(2) {
        let (t0, c0) = pair[0];
        let (t1, c1) = pair[1];
        if t <= t1 {
            let span = t1 - t0;
            let local = if span <= f32::EPSILON {
                0.0
            } else {
                (t - t0) / span
            };
            return c0.lerp(c1, local);
        }
    }
    stops[stops.len() - 1].1
}

/// The outline of a rounded rectangle, clockwise, starting after the top-left
/// corner arc.
pub fn rounded_rect_points(rect: Rect, radius: f32) -> Vec<Pos2> {
    let max_radius = (rect.width().min(rect.height()) / 2.0).max(0.0);
    let r = radius.clamp(0.0, max_radius);
    if r <= 0.5 {
        return vec![
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ];
    }
    // (centre, start angle in radians) per corner, clockwise from the top-left.
    let corners = [
        (
            egui::pos2(rect.left() + r, rect.top() + r),
            std::f32::consts::PI,
        ),
        (
            egui::pos2(rect.right() - r, rect.top() + r),
            std::f32::consts::FRAC_PI_2 * 3.0,
        ),
        (egui::pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (
            egui::pos2(rect.left() + r, rect.bottom() - r),
            std::f32::consts::FRAC_PI_2,
        ),
    ];
    let mut points = Vec::with_capacity(corners.len() * (CORNER_SEGMENTS + 1));
    for (center, start) in corners {
        for step in 0..=CORNER_SEGMENTS {
            let angle =
                start + std::f32::consts::FRAC_PI_2 * (step as f32 / CORNER_SEGMENTS as f32);
            points.push(egui::pos2(
                center.x + r * angle.cos(),
                center.y + r * angle.sin(),
            ));
        }
    }
    points
}

/// Destination rect + UV rect for painting a texture inside `rect` with a
/// given [`Fit`].
///
/// * `Cover` fills the rect and crops the overflow: dest = rect, UV = centred
///   sub-rectangle of the source.
/// * `Contain` shows the whole image: UV = full, dest = centred fitted rect.
/// * `Tile` repeats the source at its natural size: dest = rect, UV spans
///   `rect / texture` (the texture must use a repeat wrap mode).
/// * `Stretch` distorts to fill: dest = rect, UV = full.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageMapping {
    pub dest: Rect,
    pub uv: Rect,
}

pub fn image_mapping(fit: Fit, rect: Rect, texture: Vec2) -> ImageMapping {
    let full_uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    if texture.x <= 0.0 || texture.y <= 0.0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return ImageMapping {
            dest: rect,
            uv: full_uv,
        };
    }
    match fit {
        Fit::Stretch => ImageMapping {
            dest: rect,
            uv: full_uv,
        },
        Fit::Cover => {
            let scale = (rect.width() / texture.x).max(rect.height() / texture.y);
            let visible = Vec2::new(rect.width() / scale, rect.height() / scale);
            let u0 = (texture.x - visible.x) / 2.0 / texture.x;
            let v0 = (texture.y - visible.y) / 2.0 / texture.y;
            let u1 = u0 + visible.x / texture.x;
            let v1 = v0 + visible.y / texture.y;
            ImageMapping {
                dest: rect,
                uv: Rect::from_min_max(egui::pos2(u0, v0), egui::pos2(u1, v1)),
            }
        }
        Fit::Contain => {
            let scale = (rect.width() / texture.x).min(rect.height() / texture.y);
            let size = Vec2::new(texture.x * scale, texture.y * scale);
            let dest = Rect::from_center_size(rect.center(), size);
            ImageMapping { dest, uv: full_uv }
        }
        Fit::Tile => {
            let u = rect.width() / texture.x;
            let v = rect.height() / texture.y;
            ImageMapping {
                dest: rect,
                uv: Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(u, v)),
            }
        }
    }
}

/// Paint a texture over `rect` according to a [`Fit`], tinted/opaqued by `tint`
/// (a multiply in gamma space, like Qt's `CompositionMode_Multiply`-lite that
/// the Python side does not even do — this is a Rust-only nicety).
#[allow(clippy::too_many_arguments)]
pub fn paint_texture(
    painter: &Painter,
    rect: Rect,
    texture_id: TextureId,
    texture_size: Vec2,
    fit: Fit,
    tint: Option<HexColor>,
    opacity: f32,
) {
    let mapping = image_mapping(fit, rect, texture_size);
    if mapping.dest.width() <= 0.0 || mapping.dest.height() <= 0.0 {
        return;
    }
    let color = match tint {
        Some(tint) => tint.scale_alpha(opacity.clamp(0.0, 1.0)).color32(),
        None => HexColor::rgba(255, 255, 255, 255)
            .scale_alpha(opacity.clamp(0.0, 1.0))
            .color32(),
    };
    let mut mesh = Mesh::with_texture(texture_id);
    mesh.add_rect_with_uv(mapping.dest, mapping.uv, color);
    painter.add(egui::Shape::mesh(mesh));
}

/// `CornerRadius` from a float radius (egui 0.37 uses `u8` radii).
pub fn corner(radius: f32) -> egui::CornerRadius {
    egui::CornerRadius::same(radius.clamp(0.0, 255.0).round() as u8)
}

/// Clip `rect` to `parent` plus a small inset — used so a strip's content can
/// never paint over its own border.
pub fn inset(rect: Rect, by: f32) -> Rect {
    Rect::from_min_max(
        egui::pos2(rect.left() + by, rect.top() + by),
        egui::pos2(rect.right() - by, rect.bottom() - by),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> HexColor {
        HexColor::parse(s).expect("valid colour")
    }

    #[test]
    fn solid_fill_is_a_single_quad_via_rect_filled() {
        // `fill_rect` on a solid does not build a mesh: nothing to assert on a
        // Painter, so assert the geometry helper instead.
        let rect = Rect::from_min_size(egui::pos2(1.0, 2.0), Vec2::new(10.0, 20.0));
        let points = rounded_rect_points(rect, 0.0);
        assert_eq!(points.len(), 4);
        assert_eq!(points[0], rect.left_top());
        assert_eq!(points[2], rect.right_bottom());
    }

    #[test]
    fn rounded_rect_points_stay_inside_the_rect() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(100.0, 200.0));
        for radius in [0.0, 1.0, 5.0, 50.0, 1000.0] {
            let points = rounded_rect_points(rect, radius);
            assert!(points.len() >= 4);
            for p in points {
                assert!(p.x >= -0.01 && p.x <= 100.01, "x out of the rect: {p:?}");
                assert!(p.y >= -0.01 && p.y <= 200.01, "y out of the rect: {p:?}");
            }
        }
    }

    #[test]
    fn corner_radius_is_clamped_to_half_the_short_side() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(20.0, 100.0));
        // Radius 1000 on a 20 px wide rect must not produce points outside it.
        let points = rounded_rect_points(rect, 1000.0);
        let min_x = points.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
        let max_x = points.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max);
        assert!(
            min_x >= -0.01 && max_x <= 20.01,
            "min_x={min_x} max_x={max_x}"
        );
    }

    #[test]
    fn gradient_mesh_has_a_fan_topology() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(100.0, 400.0));
        let mesh = gradient_rounded_rect(rect, 5.0, Fill::vertical(hex("#000000"), hex("#ffffff")));
        let rim = rounded_rect_points(rect, 5.0).len();
        assert_eq!(mesh.vertices.len(), rim + 1);
        assert_eq!(mesh.indices.len(), rim * 3);
        // Top rim vertices are dark, bottom rim vertices are light.
        let top = mesh
            .vertices
            .iter()
            .filter(|v| v.pos.y < 1.0)
            .map(|v| v.color)
            .collect::<Vec<_>>();
        assert!(!top.is_empty());
        assert!(top.iter().all(|c| c.r() == 0), "top rim must be #000000");
        let bottom = mesh
            .vertices
            .iter()
            .filter(|v| v.pos.y > 399.0)
            .map(|v| v.color)
            .collect::<Vec<_>>();
        assert!(!bottom.is_empty());
        assert!(
            bottom.iter().all(|c| c.r() == 255),
            "bottom rim must be #ffffff"
        );
    }

    #[test]
    fn gradient_mesh_indices_are_valid() {
        let rect = Rect::from_min_size(egui::pos2(5.0, 5.0), Vec2::new(90.0, 300.0));
        let mesh = gradient_rounded_rect(rect, 5.0, Fill::vertical(hex("#444444"), hex("#3daee9")));
        let n = mesh.vertices.len() as u32;
        assert!(mesh.indices.iter().all(|&i| i < n));
        assert!(mesh.is_valid());
    }

    #[test]
    fn degenerate_rect_produces_no_geometry() {
        let empty = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::ZERO);
        let mesh = gradient_rounded_rect(empty, 5.0, Fill::solid(hex("#ffffff")));
        assert!(mesh.indices.is_empty());
    }

    #[test]
    fn multi_stop_gradient_has_one_band_per_segment() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(6.0, 100.0));
        let mesh = vertical_gradient_rect(
            rect,
            &[
                (0.0, hex("#00ff00")),
                (0.5, hex("#ffff00")),
                (1.0, hex("#ff0000")),
            ],
        );
        assert_eq!(mesh.vertices.len(), 8); // 2 bands × 4 vertices
        assert_eq!(mesh.indices.len(), 12); // 2 bands × 2 triangles
                                            // The top of the first band is green, the bottom of the last is red.
        let first_top = mesh
            .vertices
            .iter()
            .find(|v| v.pos.y <= 0.01 && v.pos.x <= 0.01)
            .expect("top-left vertex");
        assert_eq!(first_top.color, hex("#00ff00").color32());
        let last_bottom = mesh
            .vertices
            .iter()
            .find(|v| (v.pos.y - 100.0).abs() <= 0.01 && v.pos.x <= 0.01)
            .expect("bottom-left vertex");
        assert_eq!(last_bottom.color, hex("#ff0000").color32());
    }

    #[test]
    fn sample_stops_interpolates_and_clamps() {
        let stops = [(0.0, hex("#000000")), (1.0, hex("#ffffff"))];
        assert_eq!(sample_stops(&stops, 0.0), hex("#000000"));
        assert_eq!(sample_stops(&stops, 1.0), hex("#ffffff"));
        assert_eq!(sample_stops(&stops, 0.5), hex("#808080"));
        assert_eq!(sample_stops(&stops, -2.0), hex("#000000"));
        assert_eq!(sample_stops(&stops, 5.0), hex("#ffffff"));
    }

    #[test]
    fn image_stretch_uses_the_whole_texture() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(50.0, 30.0));
        let m = image_mapping(Fit::Stretch, rect, Vec2::new(200.0, 100.0));
        assert_eq!(m.dest, rect);
        assert_eq!(m.uv.min, egui::pos2(0.0, 0.0));
        assert_eq!(m.uv.max, egui::pos2(1.0, 1.0));
    }

    #[test]
    fn image_cover_crops_the_overflow_and_keeps_the_ratio() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(50.0, 30.0));
        // Texture is wider than the rect's ratio (2:1 vs 5:3) → crop horizontally.
        let m = image_mapping(Fit::Cover, rect, Vec2::new(200.0, 100.0));
        assert_eq!(m.dest, rect);
        assert!(m.uv.min.y.abs() < 1e-6, "{:?}", m.uv.min);
        assert!((m.uv.max.y - 1.0).abs() < 1e-6, "{:?}", m.uv.max);
        assert!(m.uv.min.x > 0.0 && m.uv.max.x < 1.0, "{:?}", m.uv);
        // The visible region is a sub-rectangle of the texture…
        let visible = Vec2::new(m.uv.width() * 200.0, m.uv.height() * 100.0);
        assert!(
            visible.x <= 200.0 + 1e-3 && visible.y <= 100.0 + 1e-3,
            "{visible:?}"
        );
        // …that fills the rect with ONE uniform scale on both axes (that is what
        // "cover" means: no distortion)…
        let scale_x = rect.width() / visible.x;
        let scale_y = rect.height() / visible.y;
        assert!((scale_x - scale_y).abs() < 1e-4, "{scale_x} != {scale_y}");
        // …and it must touch both edges of the texture on the cropped axis.
        assert!(
            (visible.y - 100.0).abs() < 1e-3,
            "the shorter axis is fully used: {visible:?}"
        );
    }

    #[test]
    fn image_contain_fits_inside_and_centres() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(100.0, 100.0));
        let m = image_mapping(Fit::Contain, rect, Vec2::new(200.0, 100.0));
        assert_eq!(
            m.uv,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
        );
        assert_eq!(m.dest.width(), 100.0);
        assert_eq!(m.dest.height(), 50.0);
        assert_eq!(m.dest.center(), rect.center());
        assert!(rect.contains_rect(m.dest));
    }

    #[test]
    fn image_tile_spans_the_rect_in_texture_units() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(64.0, 128.0));
        let m = image_mapping(Fit::Tile, rect, Vec2::new(32.0, 32.0));
        assert_eq!(m.dest, rect);
        assert_eq!(m.uv.max, egui::pos2(2.0, 4.0));
    }

    #[test]
    fn image_mapping_survives_a_zero_sized_texture() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(10.0, 10.0));
        let m = image_mapping(Fit::Cover, rect, Vec2::ZERO);
        assert_eq!(m.dest, rect);
        assert_eq!(m.uv.width(), 1.0);
    }

    #[test]
    fn corner_maps_float_radius_to_u8() {
        assert_eq!(corner(5.0), egui::CornerRadius::same(5));
        assert_eq!(corner(-3.0), egui::CornerRadius::same(0));
        assert_eq!(corner(1000.0), egui::CornerRadius::same(255));
    }

    #[test]
    fn inset_shrinks_each_side() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(100.0, 50.0));
        let shrunk = inset(rect, 5.0);
        assert_eq!(shrunk.min, egui::pos2(5.0, 5.0));
        assert_eq!(shrunk.max, egui::pos2(95.0, 45.0));
    }
}
