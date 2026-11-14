//! `holaf-ui-snapshot` — render the UI headless and write a PNG.
//!
//! This container has no display server, so the only way to *look* at the port
//! and compare it with the Qt build is to run the UI inside a plain
//! `egui::Context` and rasterise the tessellated meshes in software (see
//! `holaf_ui::raster`).  The UI tree is the production one: `holaf_ui::render`
//! is the same function `holaf-ui` calls from `eframe`.
//!
//! The picture is rendered at `--scale` (default 2) and box-downsampled, which
//! is how it gets anti-aliased without a GPU.
//!
//! ```text
//! cargo run -p holaf-ui --bin holaf-ui-snapshot -- --out /tmp/holaf-ui.png
//! cargo run -p holaf-ui --bin holaf-ui-snapshot -- --scale 3 --time 2.0
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use holaf_ui::app::AppState;
use holaf_ui::raster::{render_frame, Framebuffer};
use holaf_ui::theme::ThemeFile;

/// Window size used when the caller does not ask for one.
///
/// Width: the theme's default window, never narrower than the width the strip
/// list requires (`Σ section_width`, the number the app locks itself to — 794 px
/// for the seven demo strips).  Height: the theme's default; pass `--height` to
/// show the strips stretching (see `docs/ui-rust-snapshot-tall.png`).
fn default_size(state: &AppState) -> egui::Vec2 {
    let dims = &state.theme.theme.dimensions;
    let required = holaf_ui::layout::required_width_for(&state.strips) as f32;
    egui::vec2(dims.window_width.max(required), dims.window_height)
}

struct Args {
    out: PathBuf,
    width: Option<f32>,
    height: Option<f32>,
    scale: usize,
    time: f64,
    theme: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        out: PathBuf::from("/tmp/holaf-ui-snapshot.png"),
        width: None,
        height: None,
        scale: 2,
        time: 1.4,
        theme: None,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || {
            argv.next()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--out" | "-o" => args.out = PathBuf::from(value()?),
            "--width" => {
                args.width = Some(value()?.parse().map_err(|e| format!("--width: {e}"))?)
            }
            "--height" => {
                args.height = Some(value()?.parse().map_err(|e| format!("--height: {e}"))?)
            }
            "--scale" => {
                args.scale = value()?
                    .parse::<usize>()
                    .map_err(|e| format!("--scale: {e}"))?
                    .clamp(1, 4)
            }
            "--time" => args.time = value()?.parse().map_err(|e| format!("--time: {e}"))?,
            "--theme" => args.theme = Some(PathBuf::from(value()?)),
            "--help" | "-h" => {
                return Err(
                    "usage: holaf-ui-snapshot [--out FILE.png] [--width N] [--height N] \
                     [--scale 1..4] [--time SECONDS] [--theme FILE.json]"
                        .to_string(),
                )
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let mut state = AppState::demo();
    if let Some(path) = &args.theme {
        match ThemeFile::load(path) {
            Ok(theme) => {
                log::info!("using theme {path:?} ({})", theme.name);
                state.theme = theme;
            }
            Err(err) => {
                eprintln!("could not load theme {}: {err}", path.display());
                return ExitCode::FAILURE;
            }
        }
    }

    let size = default_size(&state);
    let size = egui::vec2(
        args.width.unwrap_or(size.x),
        args.height.unwrap_or(size.y),
    );

    let ctx = egui::Context::default();
    // Flat, dark defaults: `holaf_ui::render` paints every pixel it cares about,
    // so the style only affects the rename dialog (not shown in a snapshot).
    ctx.set_visuals(egui::Visuals::dark());

    let background = state.theme.theme.backgrounds.window;
    let fb = render_frame(
        &ctx,
        size,
        args.time,
        args.scale,
        background
            .at(0.0)
            .color32(),
        |ui| {
            holaf_ui::render(&mut state, ui);
        },
    );
    let fb: Framebuffer = fb.downsample(args.scale);

    if let Err(err) = std::fs::create_dir_all(
        args.out
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new(".")),
    ) {
        eprintln!("cannot create the output directory: {err}");
        return ExitCode::FAILURE;
    }
    if let Err(err) = fb.to_image().save(&args.out) {
        eprintln!("cannot write {}: {err}", args.out.display());
        return ExitCode::FAILURE;
    }

    let bytes = std::fs::metadata(&args.out).map(|m| m.len()).unwrap_or(0);
    println!(
        "wrote {} ({}x{}, {:.1} KiB, scale {}x, theme {:?})",
        args.out.display(),
        fb.width,
        fb.height,
        bytes as f64 / 1024.0,
        args.scale,
        state.theme.name
    );
    ExitCode::SUCCESS
}
