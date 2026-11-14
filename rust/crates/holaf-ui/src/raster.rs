//! Software rasteriser — turn tessellated epaint meshes into a PNG.
//!
//! # Why this exists
//!
//! This container has **no display server** (no X11, no Wayland, no Xvfb), so
//! an `eframe` window cannot be opened and `eframe`'s own screenshot path is
//! unreachable.  The alternative to "no visual proof at all" is to run the UI
//! headless — `Context::run_ui` + `Context::tessellate` produce exactly the
//! same geometry the GPU would receive — and rasterise those triangles in
//! software.  That is what this module does, and it is why
//! `holaf-ui-snapshot` can prove the layout without a window.
//!
//! # Fidelity
//!
//! Deliberately the *same* maths as epaint's shaders, not an approximation:
//!
//! * vertex colours are premultiplied sRGB (`epaint::Vertex`), interpolated
//!   linearly, multiplied by the texel in gamma space (egui's default
//!   `dithering`/gamma setting), then blended as `dst = src + dst·(1−a)`;
//! * the only texture that exists here is the font atlas (`TextureId::Managed(0)`),
//!   fetched *after* the pass so every glyph used is in it;
//! * clip rectangles are honoured per primitive (the VU fill uses one).
//!
//! What it does **not** do: feathering on hand-built meshes (the tessellator
//! only feathers its own shapes) — mitigated by rendering at 2× and box
//! downsampling, which gives the whole image cheap anti-aliasing.

use egui::epaint::{ClippedPrimitive, Primitive};
use egui::{Color32, ColorImage, Rect, TextureId};

/// A premultiplied-sRGB float framebuffer.
#[derive(Debug, Clone)]
pub struct Framebuffer {
    pub width: usize,
    pub height: usize,
    /// Premultiplied sRGB, `0.0..=1.0` per channel.
    pub pixels: Vec<[f32; 4]>,
}

impl Framebuffer {
    /// New opaque framebuffer filled with `background`.
    pub fn new(width: usize, height: usize, background: Color32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            pixels: vec![to_premultiplied(background); width * height],
        }
    }

    #[inline]
    fn index(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }

    /// Composite an already-premultiplied colour over pixel `(x, y)`.
    #[inline]
    fn blend(&mut self, x: usize, y: usize, src: [f32; 4]) {
        if src[3] <= 0.0 {
            return;
        }
        let index = self.index(x, y);
        let dst = &mut self.pixels[index];
        let inv = 1.0 - src[3];
        for channel in 0..4 {
            dst[channel] = (src[channel] + dst[channel] * inv).clamp(0.0, 1.0);
        }
    }

    /// Box-downsample by an integer factor (used to anti-alias a 2× render).
    ///
    /// Returns `self` unchanged when `factor <= 1` or when the result would be
    /// empty, so callers never have to special-case small images.
    pub fn downsample(&self, factor: usize) -> Self {
        if factor <= 1 || self.width < factor || self.height < factor {
            return self.clone();
        }
        let width = self.width / factor;
        let height = self.height / factor;
        let mut pixels = vec![[0.0f32; 4]; width * height];
        let inv = 1.0 / (factor * factor) as f32;
        for y in 0..height {
            for x in 0..width {
                let mut acc = [0.0f32; 4];
                for dy in 0..factor {
                    for dx in 0..factor {
                        let src = &self.pixels[(y * factor + dy) * self.width + x * factor + dx];
                        for (sum, value) in acc.iter_mut().zip(src.iter()) {
                            *sum += *value;
                        }
                    }
                }
                for sum in &mut acc {
                    *sum *= inv;
                }
                pixels[y * width + x] = acc;
            }
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    /// Un-premultiply into an 8-bit RGBA image (what a PNG wants).
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.width * self.height * 4);
        for pixel in &self.pixels {
            let a = pixel[3].clamp(0.0, 1.0);
            if a <= 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            for value in &pixel[..3] {
                let straight = (value / a).clamp(0.0, 1.0);
                out.push((straight * 255.0).round() as u8);
            }
            out.push((a * 255.0).round() as u8);
        }
        out
    }

    /// Same as [`Self::to_rgba8`] but as an `image` crate buffer.
    pub fn to_image(&self) -> image::RgbaImage {
        image::RgbaImage::from_raw(self.width as u32, self.height as u32, self.to_rgba8())
            .expect("the buffer is exactly width*height*4 bytes")
    }
}

/// The textures a frame can reference.
///
/// Only the font atlas exists: the snapshot renders the demo theme, which uses
/// no decorative image, and any other `TextureId` is treated as white (the same
/// thing an unbound texture does on the GPU).
pub struct Textures {
    pub font_atlas: ColorImage,
}

impl Textures {
    /// Grab the font atlas of a context **after** a pass (glyphs are rasterised
    /// on demand while laying out text).
    pub fn from_context(ctx: &egui::Context) -> Self {
        Self {
            font_atlas: ctx.fonts(|fonts| fonts.image()),
        }
    }
}

/// Rasterise every clipped primitive of one frame into `fb`.
///
/// `pixels_per_point` converts the point-space geometry to pixels; the caller
/// passes the same value it gave the `Context` (a value > 1 is supersampling).
pub fn render(fb: &mut Framebuffer, primitives: &[ClippedPrimitive], textures: &Textures, pixels_per_point: f32) {
    let ppp = if pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    for clipped in primitives {
        let Primitive::Mesh(mesh) = &clipped.primitive else {
            // Paint callbacks need a real GPU: nothing to rasterise.
            continue;
        };
        let clip = clip_pixels(clipped.clip_rect, ppp, fb.width, fb.height);
        let Some(clip) = clip else { continue };
        let texture = if mesh.texture_id == TextureId::Managed(0) {
            Some(&textures.font_atlas)
        } else {
            None
        };
        for triangle in mesh.indices.as_chunks::<3>().0 {
            let (Some(a), Some(b), Some(c)) = (
                mesh.vertices.get(triangle[0] as usize),
                mesh.vertices.get(triangle[1] as usize),
                mesh.vertices.get(triangle[2] as usize),
            ) else {
                continue;
            };
            raster_triangle(fb, clip, texture, ppp, a, b, c);
        }
    }
}

/// Pixel-space clip rectangle, or `None` when fully off-screen.
fn clip_pixels(rect: Rect, ppp: f32, width: usize, height: usize) -> Option<(usize, usize, usize, usize)> {
    let min_x = (rect.min.x * ppp).floor().max(0.0) as usize;
    let min_y = (rect.min.y * ppp).floor().max(0.0) as usize;
    let max_x = (rect.max.x * ppp).ceil().min(width as f32) as usize;
    let max_y = (rect.max.y * ppp).ceil().min(height as f32) as usize;
    if max_x <= min_x || max_y <= min_y {
        None
    } else {
        Some((min_x, min_y, max_x, max_y))
    }
}

/// One triangle, in premultiplied sRGB — the exact maths of epaint's shader.
#[allow(clippy::too_many_arguments)]
fn raster_triangle(
    fb: &mut Framebuffer,
    clip: (usize, usize, usize, usize),
    texture: Option<&ColorImage>,
    ppp: f32,
    a: &egui::epaint::Vertex,
    b: &egui::epaint::Vertex,
    c: &egui::epaint::Vertex,
) {
    let pa = [a.pos.x * ppp, a.pos.y * ppp];
    let pb = [b.pos.x * ppp, b.pos.y * ppp];
    let pc = [c.pos.x * ppp, c.pos.y * ppp];
    if !pa[0].is_finite()
        || !pa[1].is_finite()
        || !pb[0].is_finite()
        || !pb[1].is_finite()
        || !pc[0].is_finite()
        || !pc[1].is_finite()
    {
        return; // epaint never emits this; a hand-built mesh might
    }
    let area = edge(pa, pb, pc);
    if area.abs() < 1e-9 {
        return; // degenerate
    }

    // Clamp to the clip rectangle *before* casting: an infinity (a mesh built
    // from `Rect::EVERYTHING`, which the tests use) saturates to `usize::MAX`
    // and would overflow on the `+ 1`.
    let clamp = |value: f32, low: usize, high: usize| -> usize {
        if value.is_nan() || value < low as f32 {
            low
        } else if value > high as f32 {
            high
        } else {
            value as usize
        }
    };
    let min_x = clamp(pa[0].min(pb[0]).min(pc[0]).floor(), clip.0, clip.2);
    let max_x = clamp(pa[0].max(pb[0]).max(pc[0]).ceil(), clip.0, clip.2)
        .saturating_add(1)
        .min(clip.2);
    let min_y = clamp(pa[1].min(pb[1]).min(pc[1]).floor(), clip.1, clip.3);
    let max_y = clamp(pa[1].max(pb[1]).max(pc[1]).ceil(), clip.1, clip.3)
        .saturating_add(1)
        .min(clip.3);
    if max_x <= min_x || max_y <= min_y {
        return;
    }

    let ca = to_premultiplied(a.color);
    let cb = to_premultiplied(b.color);
    let cc = to_premultiplied(c.color);

    for y in min_y..max_y {
        for x in min_x..max_x {
            let p = [x as f32 + 0.5, y as f32 + 0.5];
            // Barycentric weights. The `+ 1e-4` tolerance keeps shared edges
            // covered by both triangles instead of leaving a hairline gap;
            // opaque fills are unaffected and semi-transparent ones only darken
            // by one pixel on a seam.
            let w0 = edge(pb, pc, p) / area;
            let w1 = edge(pc, pa, p) / area;
            let w2 = edge(pa, pb, p) / area;
            if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                continue;
            }
            let mut src = [0.0f32; 4];
            for channel in 0..4 {
                src[channel] = w0 * ca[channel] + w1 * cb[channel] + w2 * cc[channel];
            }
            if let Some(image) = texture {
                let u = w0 * a.uv.x + w1 * b.uv.x + w2 * c.uv.x;
                let v = w0 * a.uv.y + w1 * b.uv.y + w2 * c.uv.y;
                let texel = sample_bilinear(image, u, v);
                for channel in 0..4 {
                    src[channel] *= texel[channel];
                }
            }
            fb.blend(x, y, src);
        }
    }
}

/// Twice the signed area of `(a, b, p)`.
#[inline]
fn edge(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    (p[0] - a[0]) * (b[1] - a[1]) - (p[1] - a[1]) * (b[0] - a[0])
}

/// Premultiplied sRGB floats from an epaint vertex colour.
#[inline]
fn to_premultiplied(color: Color32) -> [f32; 4] {
    [
        color.r() as f32 / 255.0,
        color.g() as f32 / 255.0,
        color.b() as f32 / 255.0,
        color.a() as f32 / 255.0,
    ]
}

/// Bilinear sample of a `ColorImage` at normalised `(u, v)`, clamped.
fn sample_bilinear(image: &ColorImage, u: f32, v: f32) -> [f32; 4] {
    let [width, height] = image.size;
    if width == 0 || height == 0 {
        return [1.0, 1.0, 1.0, 1.0];
    }
    let fx = (u * width as f32 - 0.5).clamp(0.0, (width - 1) as f32);
    let fy = (v * height as f32 - 0.5).clamp(0.0, (height - 1) as f32);
    let x0 = fx.floor() as usize;
    let y0 = fy.floor() as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;
    let texel = |x: usize, y: usize| -> [f32; 4] {
        let index = y.min(height - 1) * width + x.min(width - 1);
        image
            .pixels
            .get(index)
            .map(|c| to_premultiplied(*c))
            .unwrap_or([1.0, 1.0, 1.0, 1.0])
    };
    let c00 = texel(x0, y0);
    let c10 = texel(x1, y0);
    let c01 = texel(x0, y1);
    let c11 = texel(x1, y1);
    let mut out = [0.0f32; 4];
    for channel in 0..4 {
        let top = c00[channel] * (1.0 - tx) + c10[channel] * tx;
        let bottom = c01[channel] * (1.0 - tx) + c11[channel] * tx;
        out[channel] = top * (1.0 - ty) + bottom * ty;
    }
    out
}

/// Run one full headless frame of `ui_fn` and rasterise it.
///
/// Returns the framebuffer at `supersample` resolution — the caller downsamples
/// (`factor = supersample`) for anti-aliasing.
pub fn render_frame(
    ctx: &egui::Context,
    size_points: egui::Vec2,
    time: f64,
    supersample: usize,
    background: Color32,
    mut ui_fn: impl FnMut(&mut egui::Ui),
) -> Framebuffer {
    let factor = supersample.max(1);
    let pixels_per_point = factor as f32;

    // Do NOT use `Context::set_pixels_per_point`: it sets a *zoom factor*, and
    // on the first pass egui rescales the previous `content_rect` (10 000 ×
    // 10 000 points by default) by that factor — which would silently override
    // the `screen_rect` below and yield a 5 000 pt root Ui.  The honest way is
    // to declare the scale as the viewport's *native* pixels-per-point.
    let rect = Rect::from_min_size(egui::Pos2::ZERO, size_points);
    let viewport_info = egui::ViewportInfo {
        native_pixels_per_point: Some(pixels_per_point),
        inner_rect: Some(rect),
        outer_rect: Some(rect),
        ..Default::default()
    };
    let raw_input = egui::RawInput {
        viewport_id: egui::ViewportId::ROOT,
        viewports: std::iter::once((egui::ViewportId::ROOT, viewport_info)).collect(),
        screen_rect: Some(rect),
        time: Some(time),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw_input, |ui| ui_fn(ui));
    // No renderer here: the font-atlas deltas must be cleared or
    // `TexturesDelta` panics on drop.
    output.textures_delta.clear();
    let primitives = ctx.tessellate(std::mem::take(&mut output.shapes), pixels_per_point);
    let textures = Textures::from_context(ctx);

    let mut fb = Framebuffer::new(
        (size_points.x * pixels_per_point).round() as usize,
        (size_points.y * pixels_per_point).round() as usize,
        background,
    );
    render(&mut fb, &primitives, &textures, pixels_per_point);
    fb
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_mesh(rect: Rect, color: Color32) -> egui::epaint::Mesh {
        let mut mesh = egui::epaint::Mesh::default();
        mesh.add_rect_with_uv(
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            color,
        );
        mesh
    }

    fn clipped(mesh: egui::epaint::Mesh, clip: Rect) -> ClippedPrimitive {
        ClippedPrimitive {
            clip_rect: clip,
            primitive: Primitive::Mesh(mesh),
        }
    }

    fn white_textures() -> Textures {
        // A 1×1 fully white texture stands in for the font atlas: sampling it
        // must leave the vertex colour untouched.
        Textures {
            font_atlas: ColorImage::new([1, 1], vec![Color32::WHITE]),
        }
    }

    #[test]
    fn a_quad_covers_its_rect_and_nothing_else() {
        let mut fb = Framebuffer::new(20, 20, Color32::BLACK);
        let mesh = solid_mesh(
            Rect::from_min_size(egui::pos2(5.0, 5.0), egui::vec2(10.0, 10.0)),
            Color32::from_rgb(255, 0, 0),
        );
        render(
            &mut fb,
            &[clipped(mesh.clone(), Rect::EVERYTHING)],
            &white_textures(),
            1.0,
        );
        // Inside: red and opaque.
        let inside = fb.pixels[10 * 20 + 10];
        assert_eq!(inside, [1.0, 0.0, 0.0, 1.0]);
        // Outside: still the black background.
        assert_eq!(fb.pixels[0], [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(fb.pixels[19 * 20 + 19], [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn premultiplied_semi_transparent_fills_blend_over_the_background() {
        let mut fb = Framebuffer::new(10, 10, Color32::WHITE);
        let whole = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(10.0, 10.0));
        let mesh = solid_mesh(whole, Color32::from_black_alpha(128));
        render(&mut fb, &[clipped(mesh, whole)], &white_textures(), 1.0);
        let pixel = fb.pixels[5 * 10 + 5];
        // 50 % black over white → ~0.5 grey, still opaque.
        assert!((pixel[0] - 0.5).abs() < 0.02, "{pixel:?}");
        assert_eq!(pixel[3], 1.0);
    }

    #[test]
    fn the_clip_rect_is_honoured() {
        let mut fb = Framebuffer::new(20, 20, Color32::BLACK);
        let mesh = solid_mesh(
            Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(20.0, 20.0)),
            Color32::from_rgb(0, 255, 0),
        );
        let clip = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 20.0));
        render(&mut fb, &[clipped(mesh, clip)], &white_textures(), 1.0);
        assert_eq!(fb.pixels[10 * 20 + 5], [0.0, 1.0, 0.0, 1.0], "inside the clip");
        assert_eq!(fb.pixels[10 * 20 + 15], [0.0, 0.0, 0.0, 1.0], "outside the clip");
    }

    #[test]
    fn pixels_per_point_scales_the_geometry() {
        let mut fb = Framebuffer::new(40, 40, Color32::BLACK);
        let mesh = solid_mesh(
            Rect::from_min_size(egui::pos2(5.0, 5.0), egui::vec2(10.0, 10.0)),
            Color32::WHITE,
        );
        render(&mut fb, &[clipped(mesh, Rect::EVERYTHING)], &white_textures(), 2.0);
        // In points the quad is 5..15; at ppp=2 that is 10..30 px.
        assert_eq!(fb.pixels[20 * 40 + 20], [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(fb.pixels[2 * 40 + 2], [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn degenerate_triangles_are_skipped() {
        let mut fb = Framebuffer::new(4, 4, Color32::BLACK);
        let mut mesh = egui::epaint::Mesh::default();
        mesh.colored_vertex(egui::pos2(1.0, 1.0), Color32::WHITE);
        mesh.colored_vertex(egui::pos2(1.0, 1.0), Color32::WHITE);
        mesh.colored_vertex(egui::pos2(1.0, 1.0), Color32::WHITE);
        mesh.add_triangle(0, 1, 2);
        let clip = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(4.0, 4.0));
        render(&mut fb, &[clipped(mesh, clip)], &white_textures(), 1.0);
        assert!(fb.pixels.iter().all(|p| *p == [0.0, 0.0, 0.0, 1.0]));
    }

    #[test]
    fn a_mesh_with_non_finite_vertices_is_skipped() {
        // `Rect::EVERYTHING` has infinite bounds: such a mesh must be ignored
        // rather than produce NaN pixels.
        let mut fb = Framebuffer::new(4, 4, Color32::BLACK);
        let mesh = solid_mesh(Rect::EVERYTHING, Color32::WHITE);
        let clip = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(4.0, 4.0));
        render(&mut fb, &[clipped(mesh, clip)], &white_textures(), 1.0);
        assert!(fb.pixels.iter().all(|p| p.iter().all(|c| c.is_finite())), "{:?}", fb.pixels[0]);
        assert!(fb.pixels.iter().all(|p| *p == [0.0, 0.0, 0.0, 1.0]));
    }

    #[test]
    fn a_paint_callback_is_ignored_without_a_gpu() {
        let mut fb = Framebuffer::new(4, 4, Color32::BLACK);
        let prim = ClippedPrimitive {
            clip_rect: Rect::EVERYTHING,
            primitive: Primitive::Callback(egui::PaintCallback {
                rect: Rect::EVERYTHING,
                callback: std::sync::Arc::new(
                    |_info: &egui::epaint::PaintCallbackInfo, _renderer: &dyn std::any::Any| {},
                ),
            }),
        };
        render(&mut fb, &[prim], &white_textures(), 1.0);
        assert!(fb.pixels.iter().all(|p| *p == [0.0, 0.0, 0.0, 1.0]));
    }

    #[test]
    fn downsample_averages_blocks() {
        let mut fb = Framebuffer::new(4, 4, Color32::BLACK);
        // Paint the top-left 2×2 block white.
        for pixel in &mut fb.pixels[..2] {
            *pixel = [1.0, 1.0, 1.0, 1.0];
        }
        for pixel in &mut fb.pixels[4..6] {
            *pixel = [1.0, 1.0, 1.0, 1.0];
        }
        let small = fb.downsample(2);
        assert_eq!((small.width, small.height), (2, 2));
        assert_eq!(small.pixels[0], [1.0, 1.0, 1.0, 1.0], "fully white block");
        assert_eq!(small.pixels[3], [0.0, 0.0, 0.0, 1.0], "fully black block");
    }

    #[test]
    fn downsample_mixed_block_gets_the_average() {
        let mut fb = Framebuffer::new(2, 2, Color32::BLACK);
        fb.pixels[0] = [1.0, 1.0, 1.0, 1.0];
        let small = fb.downsample(2);
        assert_eq!(small.width, 1);
        assert!((small.pixels[0][0] - 0.25).abs() < 1e-6, "{:?}", small.pixels[0]);
    }

    #[test]
    fn downsample_by_one_or_too_much_is_a_no_op() {
        let fb = Framebuffer::new(2, 2, Color32::BLACK);
        assert_eq!(fb.downsample(1).width, 2);
        assert_eq!(fb.downsample(8).width, 2);
    }

    #[test]
    fn to_rgba8_unpremultiplies() {
        let mut fb = Framebuffer::new(1, 1, Color32::TRANSPARENT);
        fb.pixels[0] = [0.25, 0.0, 0.0, 0.5];
        let bytes = fb.to_rgba8();
        assert_eq!(bytes, vec![128, 0, 0, 128]);
    }

    #[test]
    fn to_rgba8_keeps_fully_transparent_pixels_clear() {
        let fb = Framebuffer::new(1, 1, Color32::TRANSPARENT);
        assert_eq!(fb.to_rgba8(), vec![0, 0, 0, 0]);
    }

    #[test]
    fn to_image_has_the_expected_dimensions() {
        let fb = Framebuffer::new(3, 2, Color32::from_rgb(1, 2, 3));
        let image = fb.to_image();
        assert_eq!(image.dimensions(), (3, 2));
        assert_eq!(image.get_pixel(0, 0).0, [1, 2, 3, 255]);
    }

    #[test]
    fn bilinear_sampling_clamps_outside_the_texture() {
        let image = ColorImage::new([2, 2], vec![Color32::RED, Color32::GREEN, Color32::BLUE, Color32::WHITE]);
        // Corners of the UV square are clamped, never wrapped.
        let top_left = sample_bilinear(&image, -5.0, -5.0);
        assert_eq!(top_left, [1.0, 0.0, 0.0, 1.0]);
        let bottom_right = sample_bilinear(&image, 5.0, 5.0);
        assert_eq!(bottom_right, [1.0, 1.0, 1.0, 1.0]);
        // Centre is the average of the four texels.
        let centre = sample_bilinear(&image, 0.5, 0.5);
        assert!((centre[1] - 0.5).abs() < 1e-3, "{centre:?}");
    }

    #[test]
    fn a_vertex_colour_is_multiplied_by_the_texel() {
        // Half-covered font texel (premultiplied grey) behind a yellow vertex
        // colour must yield a half-transparent yellow — the same result as the
        // shader's gamma-space multiply.
        let texture = ColorImage::new([1, 1], vec![Color32::from_white_alpha(128)]);
        let mut fb = Framebuffer::new(2, 2, Color32::BLACK);
        let whole = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(2.0, 2.0));
        let mesh = solid_mesh(whole, Color32::from_rgba_unmultiplied(255, 255, 0, 255));
        render(
            &mut fb,
            &[clipped(mesh, whole)],
            &Textures { font_atlas: texture },
            1.0,
        );
        let pixel = fb.pixels[3]; // (x=1, y=1) in a 2 × 2 framebuffer
        assert!((pixel[0] - 0.5).abs() < 0.02, "{pixel:?}");
        assert!((pixel[1] - 0.5).abs() < 0.02, "{pixel:?}");
        assert!(pixel[2] < 0.02, "{pixel:?}");
    }

    #[test]
    fn render_frame_runs_a_real_ui_pass() {
        let ctx = egui::Context::default();
        let fb = render_frame(
            &ctx,
            egui::vec2(80.0, 40.0),
            1.0,
            1,
            Color32::from_rgb(0, 0, 0),
            |ui| {
                ui.painter().rect_filled(
                    Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(80.0, 40.0)),
                    egui::CornerRadius::ZERO,
                    Color32::from_rgb(0, 128, 255),
                );
            },
        );
        assert_eq!((fb.width, fb.height), (80, 40));
        let pixel = fb.pixels[20 * 80 + 40];
        assert!(pixel[2] > 0.9, "the painted blue must be there: {pixel:?}");
    }

    #[test]
    fn render_frame_supersamples_then_the_caller_downsamples() {
        let ctx = egui::Context::default();
        let fb = render_frame(
            &ctx,
            egui::vec2(20.0, 20.0),
            1.0,
            2,
            Color32::BLACK,
            |ui| {
                ui.painter().rect_filled(
                    Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(20.0, 20.0)),
                    egui::CornerRadius::ZERO,
                    Color32::WHITE,
                );
            },
        );
        assert_eq!((fb.width, fb.height), (40, 40));
        let small = fb.downsample(2);
        assert_eq!((small.width, small.height), (20, 20));
        assert!(small.pixels[0][0] > 0.99);
    }

    #[test]
    fn text_is_rasterised_from_the_font_atlas() {
        // A pass that only draws text must still produce ink: this is the
        // regression test for "the snapshot shows no labels".
        let ctx = egui::Context::default();
        let fb = render_frame(
            &ctx,
            egui::vec2(120.0, 30.0),
            1.0,
            1,
            Color32::BLACK,
            |ui| {
                ui.painter().text(
                    egui::pos2(4.0, 15.0),
                    egui::Align2::LEFT_CENTER,
                    "INPUTS",
                    egui::FontId::proportional(14.0),
                    Color32::WHITE,
                );
            },
        );
        let ink = fb
            .pixels
            .iter()
            .filter(|p| p[0] > 0.5 && p[1] > 0.5 && p[2] > 0.5)
            .count();
        assert!(ink > 20, "expected white glyph pixels, found {ink}");
    }

    #[test]
    fn an_empty_primitive_list_leaves_the_background() {
        let mut fb = Framebuffer::new(8, 8, Color32::from_rgb(35, 35, 35));
        render(&mut fb, &[], &white_textures(), 1.0);
        assert!(fb.pixels.iter().all(|p| {
            (p[0] - 35.0 / 255.0).abs() < 1e-6 && p[3] == 1.0
        }));
    }
}
