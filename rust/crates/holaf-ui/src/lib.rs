//! Holaf-Mix Rust/egui user interface.
//!
//! A faithful port of the PySide6 interface (`src/ui/` in the Python tree) onto
//! `egui` 0.37 / `eframe` 0.37.  The crate is deliberately layered so that
//! everything except the two binaries is testable without a window, a GPU or a
//! PipeWire daemon:
//!
//! | module        | role                                                        |
//! |---------------|-------------------------------------------------------------|
//! | [`theme`]     | serialisable design tokens + pure `strip → style` resolution |
//! | [`widgets`]   | custom drawing: fader + VU, button family                    |
//! | [`views`]     | layout: the main window and one strip                        |
//! | [`app`]       | [`app::AppState`] (strips, theme, caches) + [`app::render`]   |
//! | [`layout`]    | window geometry: Qt width lock + strips filling the height    |
//! | [`bridge`]    | `EngineBridge`: UI thread ↔ engine thread (mpsc)             |
//! | [`raster`]    | software rasteriser used by the headless PNG snapshot        |
//!
//! `AppState::demo()` builds the same seven strips the Python app shows out of
//! the box (4 inputs, 3 outputs, one of which is a virtual bus), so
//! `holaf-ui-snapshot` renders a picture that can be compared side by side with
//! a screenshot of the Qt build.
//!
//! See `docs/ui-rust-design.md` for the migration plan (`HOLAF_UI=rust|python`)
//! and `docs/ui-inventory.md` for the measured Qt geometry/colours.

pub mod app;
pub mod bridge;
pub mod layout;
pub mod raster;
pub mod theme;
pub mod views;
pub mod widgets;

pub use app::{render, AppState};

/// Helpers shared by the unit tests: a headless `egui::Context` with no
/// renderer, so font layout and tessellation work without a GPU or a window.
///
/// The one pitfall: `FullOutput::textures_delta` **panics on drop** when it
/// still holds unapplied deltas (there is no renderer to upload them to), so
/// every helper here clears it.
#[cfg(test)]
pub(crate) mod test_support {
    use egui::epaint::ClippedPrimitive;

    /// A context that has already run one empty pass (fonts are only available
    /// after the first `run`, which `Painter::layout_no_wrap` needs).
    pub fn headless_context() -> egui::Context {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        ctx
    }

    /// Run one pass at `size` points and return the tessellated primitives.
    pub fn run_pass(
        ctx: &egui::Context,
        size: egui::Vec2,
        time: f64,
        mut ui_fn: impl FnMut(&mut egui::Ui),
    ) -> Vec<ClippedPrimitive> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            time: Some(time),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| ui_fn(ui));
        output.textures_delta.clear();
        let ppp = ctx.pixels_per_point();
        ctx.tessellate(std::mem::take(&mut output.shapes), ppp)
    }
}
