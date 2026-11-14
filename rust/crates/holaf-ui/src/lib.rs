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
//! | [`app`]       | [`app::AppState`] (strips, theme, engine, config) + `render`  |
//! | [`layout`]    | window geometry: Qt width lock + strips filling the height    |
//! | [`logbuf`]    | log ring buffer behind the diagnostics "Journal" tab         |
//! | [`bridge`]    | `EngineBridge`: UI thread ↔ engine thread (mpsc)             |
//! | [`config`]    | `config.json` load/save: migrations, unknown keys, debounce  |
//! | [`meters`]    | RMS/peak maths, decay envelope, lock-free `MeterBoard`       |
//! | [`capture`]   | `pw-cat` capture children feeding the meters (engine-pw)     |
//! | [`dialogs`]   | FX settings, add strip, delete confirmation, device picker   |
//! | [`devices`]   | device identity: description/structural matching             |
//! | [`apps`]      | per-application routing: list, selection, 5 s enforcement    |
//! | [`menus`]     | window/strip/FX context menus + their actions                |
//! | [`diagnostics`] | the pure diagnostic report (collector + findings + text)   |
//! | [`midi`]      | MIDI CC mapping / learn / LED feedback (I/O behind `midi`)   |
//! | [`tray`]      | tray menu policy + always-on-top (the icon itself: TODO)     |
//! | [`fx_ui`]     | FX control-port ranges/order (the Qt keyword table)          |
//! | [`coalesce`]  | the 25 Hz fader throttle                                     |
//! | [`raster`]    | software rasteriser used by the headless PNG snapshot        |
//!
//! The app itself starts from the user's `config.json`
//! ([`app::AppState::from_config`]).  `AppState::demo()` builds the same seven
//! strips the Python app shows out of the box (4 inputs, 3 outputs, one of
//! which is a virtual bus) and is what `holaf-ui-snapshot` renders, so the
//! picture can be compared side by side with a screenshot of the Qt build.
//!
//! See `docs/ui-rust-design.md` for the migration plan (`HOLAF_UI=rust|python`)
//! and `docs/ui-inventory.md` for the measured Qt geometry/colours.

pub mod app;
pub mod apps;
pub mod bridge;
pub mod coalesce;
pub mod config;
pub mod devices;
pub mod diagnostics;
pub mod dialogs;
pub mod engine_json;
pub mod fx_ui;
pub mod layout;
pub mod logbuf;
pub mod menus;
pub mod meters;
pub mod midi;
pub mod raster;
pub mod theme;
pub mod tray;
pub mod views;
pub mod widgets;

#[cfg(feature = "engine-pw")]
pub mod capture;

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
