//! `holaf-ui` — the desktop app (eframe + glow).
//!
//! Everything of interest lives in the library: this file only wires the
//! `eframe::App` shell to `holaf_ui::render`, which is the exact same call the
//! headless snapshot binary makes.  Keeping it this thin is deliberate — a UI
//! that can only be looked at through a GPU window is a UI that cannot be
//! tested.
//!
//! # Running
//!
//! ```text
//! cargo run -p holaf-ui                       # demo strips (no engine)
//! cargo run -p holaf-ui --features engine-pw  # drives a real PipeWireBackend
//! ```
//!
//! On a machine with no display server this exits with a clear error instead of
//! a panic (see `docs/ui-rust-design.md` §"Capture").

use eframe::egui;
use holaf_ui::app::AppState;

/// The eframe shell: an [`AppState`] plus a frame counter used for logging.
struct HolafApp {
    state: AppState,
    frames: u64,
}

impl HolafApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // The theme owns every colour the UI uses; egui's own style only
        // matters for the few built-in widgets (the rename dialog).
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        #[allow(unused_mut)] // only mutated with the `engine-pw` feature
        let mut state = match load_state() {
            Ok(state) => state,
            Err(err) => {
                log::warn!("falling back to the demo strips: {err}");
                AppState::demo()
            }
        };

        #[cfg(feature = "engine-pw")]
        {
            state.bridge = Some(holaf_ui::bridge::EngineBridge::spawn_pipewire());
            log::info!("engine: PipeWire backend attached");
        }
        #[cfg(not(feature = "engine-pw"))]
        log::info!("engine: no backend compiled in (demo meters), see feature `engine-pw`");

        // Starting geometry.  Width: exactly what the strip count requires
        // (Qt's `setFixedWidth(Σ section_width)`), re-asserted every frame by
        // `holaf_ui::layout::enforce_window_width`.  Height: the theme's, free
        // for the user to drag from then on.
        let theme = &state.theme.theme;
        // Height floor, from the live theme and strip list: below it the fixed
        // rows of a strip (plus the 40 px minimum fader) would not fit, so the
        // WM must not shrink the window that far.  Sent as a viewport command
        // rather than `ViewportBuilder::with_min_inner_size` because only here
        // is the resolved theme known (it can come from `$HOLAF_THEME`).
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                holaf_ui::layout::minimum_window_width(),
                holaf_ui::layout::minimum_window_height(
                    theme,
                    holaf_ui::layout::counts(&state.strips).1,
                ),
            )));
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                holaf_ui::layout::required_width_for(&state.strips) as f32,
                theme.dimensions.window_height,
            )));
        Self { state, frames: 0 }
    }
}

impl eframe::App for HolafApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frames += 1;
        if self.frames == 1 {
            log::info!(
                "first frame: {} strips, theme {:?}",
                self.state.strips.len(),
                self.state.theme.name
            );
        }
        holaf_ui::render(&mut self.state, ui);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(bridge) = self.state.bridge.take() {
            bridge.shutdown();
        }
    }
}

/// Load `config.json` + the user theme when they exist.
///
/// Both are optional: with neither, the app shows the same demo strips the
/// snapshot renders, which is what makes the two comparable.  A parse error is
/// reported and the demo is used — a broken config must never prevent the UI
/// from starting (that was an explicit Python-side bug report).
fn load_state() -> Result<AppState, String> {
    let theme_path = std::env::var_os("HOLAF_THEME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            holaf_core::config::default_config_path()
                .with_file_name("theme.json")
        });
    let theme = if theme_path.exists() {
        holaf_ui::theme::ThemeFile::load(&theme_path)
            .map_err(|err| format!("{}: {err}", theme_path.display()))?
    } else {
        holaf_ui::theme::ThemeFile::holaf_dark()
    };

    let config_path = holaf_core::config::default_config_path();
    let strips = if config_path.exists() {
        let value = holaf_core::config::load_config(&config_path)
            .map_err(|err| format!("{}: {err}", config_path.display()))?;
        // `strip_from_dict` is the same migration the daemon applies, so the UI
        // shows exactly the strips the engine will build (old short port names,
        // legacy boolean FX entries, …).
        let strips: Vec<holaf_core::model::Strip> = value
            .get("strips")
            .and_then(|strips| strips.as_array())
            .map(|raw| raw.iter().map(holaf_core::migration::strip_from_dict).collect())
            .unwrap_or_default();
        log::info!("loaded {} strips from {}", strips.len(), config_path.display());
        strips
    } else {
        log::info!("no config at {} - demo strips", config_path.display());
        AppState::demo().strips
    };
    Ok(AppState::from_strips(strips, theme))
}

fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Holaf-Mix")
            // Horizontally the window is NOT the user's to size: it is locked to
            // `Σ section_width(strip count)` by `layout::enforce_window_width`
            // (see that function for why a ViewportCommand beats
            // `with_min_inner_size == with_max_inner_size`: the latter is a hint
            // the WM applies *after* the drag, so the window visibly stretches
            // then snaps back — and tiling WMs ignore it outright).  Resizing
            // stays enabled because the *height* is free: the strips stretch.
            //
            // The height floor (`ViewportCommand::MinInnerSize`) is sent from
            // `HolafApp::new`, once the theme is known.
            .with_resizable(true),
        ..Default::default()
    };
    // `run_native` returns an error (rather than panicking) when there is no
    // display server — the case on a headless machine.  Say so usefully instead
    // of dumping a winit error struct at the user.
    match eframe::run_native(
        "Holaf-Mix",
        options,
        Box::new(|cc| Ok(Box::new(HolafApp::new(cc)))),
    ) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("holaf-ui: cannot open a window: {err}");
            let headless = std::env::var_os("DISPLAY").is_none()
                && std::env::var_os("WAYLAND_DISPLAY").is_none();
            if headless {
                eprintln!("holaf-ui: no display server (DISPLAY / WAYLAND_DISPLAY are unset).");
                eprintln!("           To render the UI to a PNG instead:");
                eprintln!(
                    "           cargo run -p holaf-ui --bin holaf-ui-snapshot -- --out /tmp/holaf-ui.png"
                );
            }
            std::process::ExitCode::FAILURE
        }
    }
}
