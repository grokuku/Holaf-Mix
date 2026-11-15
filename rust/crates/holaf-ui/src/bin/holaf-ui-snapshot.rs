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
//! cargo run -p holaf-ui --bin holaf-ui-snapshot -- --config config.json
//! ```
//!
//! `--config` renders the **real** strips of that `config.json` (migrations
//! included, exactly what the app loads) instead of the demo set; the meters are
//! the deterministic animation [`holaf_ui::app::AppState::animate`] produces when
//! no engine is attached.  The capture is strictly read-only: the config store is
//! detached (`state.config = None`), so a snapshot can never rewrite the file it
//! renders.

use std::path::PathBuf;
use std::process::ExitCode;

use holaf_ui::app::AppState;
use holaf_ui::raster::{render_frame_in_canvas, Framebuffer};
use holaf_ui::theme::ThemeFile;

/// Width of the **window** in a capture: the number `layout::enforce_window_width`
/// locks the app to (`Σ section_width`, 794 px for the seven demo strips).
///
/// This is not the canvas size — see [`capture_rects`].
fn window_width(state: &AppState) -> f32 {
    holaf_ui::layout::required_width_for(&state.strips) as f32
}

/// Size used when the caller does not ask for one.
///
/// Width: the theme's default window, never narrower than the width the strip
/// list requires (`Σ section_width`, the number the app locks itself to — 794 px
/// for the seven demo strips).  Height: the theme's default; pass `--height` to
/// show the strips stretching (see `docs/ui-rust-snapshot-tall.png`).
fn default_size(state: &AppState) -> egui::Vec2 {
    let dims = &state.theme.theme.dimensions;
    egui::vec2(
        dims.window_width.max(window_width(state)),
        dims.window_height,
    )
}

/// `(canvas, window)` — what to rasterise, and the window rectangle egui lays the
/// UI out in.
///
/// A **dialog** is an `egui::Area`: it floats over the window, it never widens
/// it — in Qt the main window is `setFixedWidth(Σ section_width)`.  So a dialog
/// capture lays the UI out in a window of exactly that width:
///
/// * no `--width` → the canvas **is** the window (794 px for the seven strips);
/// * `--width 900` (a dialog that needs more room) → the canvas grows to 900 and
///   the window stays 794, the 106 px outside it painted with the caller's
///   "outside" colour.  Rendering the layout into the wider canvas instead would
///   stretch both columns to `900/794` and leave ~70 px of background to the
///   right of the last strip — a band no real window ever shows.
///
/// A plain capture (no dialog) keeps canvas == window: those images are the
/// validated ones (`docs/ui-rust-snapshot.png` at the theme's 800 px).
fn capture_rects(
    state: &AppState,
    dialog: bool,
    width: Option<f32>,
    fallback_width: f32,
    height: f32,
) -> (egui::Vec2, egui::Rect) {
    let locked = window_width(state);
    let (canvas_width, window_width) = if dialog {
        (width.unwrap_or(locked).max(locked), locked)
    } else {
        let width = width.unwrap_or(fallback_width);
        (width, width)
    };
    let canvas = egui::vec2(canvas_width, height);
    let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(window_width, height));
    (canvas, window)
}

struct Args {
    out: PathBuf,
    width: Option<f32>,
    height: Option<f32>,
    scale: usize,
    time: f64,
    theme: Option<PathBuf>,
    /// `Some(path)` renders that config's strips instead of the demo set.
    config: Option<PathBuf>,
    /// `--dialog NAME`: open one dialog before rendering (see [`open_dialog`]).
    dialog: Option<String>,
    /// `--dialog-tab NAME`: which tab of the diagnostics window to show.
    dialog_tab: Option<String>,
    /// `--devices N`: replace the mock engine's device list with `N` simulated
    /// devices, so the device picker's geometry can be checked for a short and a
    /// long list (3 devices versus 10, the two cases the user asked about).
    devices: Option<usize>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        out: PathBuf::from("/tmp/holaf-ui-snapshot.png"),
        width: None,
        height: None,
        scale: 2,
        time: 1.4,
        theme: None,
        config: None,
        dialog: None,
        dialog_tab: None,
        devices: None,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--out" | "-o" => args.out = PathBuf::from(value()?),
            "--width" => args.width = Some(value()?.parse().map_err(|e| format!("--width: {e}"))?),
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
            "--config" | "-c" => args.config = Some(PathBuf::from(value()?)),
            "--dialog" => args.dialog = Some(value()?),
            "--dialog-tab" => args.dialog_tab = Some(value()?),
            "--devices" => {
                args.devices = Some(
                    value()?
                        .parse::<usize>()
                        .map_err(|e| format!("--devices: {e}"))?
                        .max(1),
                )
            }
            "--help" | "-h" => {
                return Err(
                    "usage: holaf-ui-snapshot [--out FILE.png] [--width N] [--height N] \
                     [--scale 1..4] [--time SECONDS] [--theme FILE.json] \
                     [--config CONFIG.json] [--dialog diagnostics|apps|fx|device|add|delete|\
                     menu|midi-port|midi-port-none|midi-learn|midi-menu] \
                     [--dialog-tab summary|engine|graph|system|journal] \
                     [--devices N]"
                        .to_string(),
                )
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    // Same logger as the app: the diagnostics capture then shows a real
    // "Journal" tab instead of an empty one.
    holaf_ui::logbuf::install("warn");

    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let mut state = match &args.config {
        Some(path) => {
            let mut state = AppState::from_config(path);
            if state.strips.is_empty() {
                eprintln!(
                    "warning: {} yielded no strip ({:?}) - rendering the demo set",
                    path.display(),
                    state.status
                );
                state = AppState::demo();
            }
            // A capture is read-only: no debounced write may touch the file we
            // just rendered.
            state.config = None;
            state
        }
        None => AppState::demo(),
    };
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

    // `--dialog`: open one dialog *before* rendering, with a mock engine so the
    // diagnostics window has a real report and the app dialog a real list.  The
    // engine is not left attached when the dialog does not need it.
    if let Some(kind) = &args.dialog {
        match open_dialog(&mut state, kind, args.dialog_tab.as_deref()) {
            Ok(()) => log::info!("snapshot: dialog {kind:?} opened"),
            Err(message) => {
                eprintln!("{message}");
                return ExitCode::FAILURE;
            }
        }
    }

    // `--devices N`: how the picker looks with a short and a long list.  The
    // names are the ones the user's machine reports, so the rows are as wide as
    // the real ones (a `mock-1`/`mock-2` list would prove nothing about width).
    if let Some(count) = args.devices {
        state.engine_devices = simulated_devices(count);
    }

    let size = default_size(&state);
    let saved_height = state
        .geometry()
        .map(|geometry| geometry.height)
        .filter(|height| *height >= 200.0);
    let height = args.height.or(saved_height).unwrap_or(size.y);
    let (canvas, window) = capture_rects(&state, args.dialog.is_some(), args.width, size.x, height);

    let ctx = egui::Context::default();
    // Flat, dark defaults: `holaf_ui::render` paints every pixel it cares about,
    // so the style only affects the rename dialog (not shown in a snapshot).
    ctx.set_visuals(egui::Visuals::dark());

    let background = state.theme.theme.backgrounds.window;
    // Outside the window — only ever visible when `--width` asks for a canvas
    // wider than the window (a dialog that needs the room).  A colour no theme
    // token uses, so a reader cannot mistake it for a section background.
    let outside = egui::Color32::from_rgb(0x14, 0x14, 0x14);
    let clear = if canvas.x > window.width() + 0.5 {
        outside
    } else {
        background.at(0.0).color32()
    };
    if canvas.x > window.width() + 0.5 {
        eprintln!(
            "note: canvas {} px wide, window locked to {:.0} px ({:.0} px outside the window, \
             painted #141414)",
            canvas.x,
            window.width(),
            canvas.x - window.width()
        );
    }
    // A dialog is an `egui::Area`: egui measures it on the first pass and paints
    // it on the second, so one throw-away pass is rendered when one is open.
    // The throw-away pass runs *earlier* in time: `egui::Area` fades in over
    // `style.animation_time`, so two passes at the same instant would paint the
    // dialog at alpha 0.
    if args.dialog.is_some() {
        let warmup = (args.time - 0.5).max(0.0);
        let _ = render_frame_in_canvas(&ctx, canvas, window, warmup, 1, clear, |ui| {
            holaf_ui::render(&mut state, ui);
        });
    }
    let fb = render_frame_in_canvas(&ctx, canvas, window, args.time, args.scale, clear, |ui| {
        holaf_ui::render(&mut state, ui);
    });
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

/// `--devices N`: `N` plausible **sinks** (the picker a snapshot opens belongs
/// to an output strip, so a source would simply not be listed).
///
/// The first entries are the user's own devices (a built-in HDA card, a PreSonus
/// Studio 26c in mono and stereo profiles, a C-Media USB dongle); past those the
/// names are generated variants of the same shape.  Deterministic, and every name
/// is a real-looking PipeWire `node.name`, so the captured pop-up has the widths a
/// real machine produces.
fn simulated_devices(count: usize) -> Vec<holaf_ui::engine_json::DeviceInfo> {
    const BASE: [(&str, Option<&str>); 4] = [
        (
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            Some("Built-in Audio Analog Stereo"),
        ),
        (
            "alsa_output.usb-PreSonus_Studio_26c_01424548-00.pro-output-0",
            None,
        ),
        (
            "alsa_output.usb-PreSonus_Studio_26c_01424548-00.pro-output-1",
            None,
        ),
        (
            "alsa_output.usb-C-Media_Electronics_Inc._USB_Audio_Device-00.analog-stereo",
            None,
        ),
    ];
    (0..count)
        .map(|index| {
            let (name, description) = if index < BASE.len() {
                (BASE[index].0.to_string(), BASE[index].1.map(str::to_string))
            } else {
                // Same shape, numbered: the *labels* stay distinct, which is what
                // the pop-up must guarantee (the model is the interface name).
                (
                    format!(
                        "alsa_output.usb-Holaf_Test_Interface_{index:02}-00.analog-stereo"
                    ),
                    None,
                )
            };
            holaf_ui::engine_json::DeviceInfo {
                id: index as u32 + 1,
                name,
                media_class: "Audio/Sink".to_string(),
                description,
            }
        })
        .collect()
}

/// Open one dialog so the snapshot can show it.
///
/// The dialogs that need data (diagnostics, app selection) are filled from a
/// **mock engine**: the mock answers `status`, `device_list` and `app_list` with
/// deterministic payloads (see `crate::bridge`), which is exactly the "what does
/// the dialog look like when the engine answers" case — and it needs no daemon,
/// so a capture stays reproducible.
fn open_dialog(state: &mut AppState, kind: &str, tab: Option<&str>) -> Result<(), String> {
    // A virtual input strip is the one that can take applications over.
    let virtual_input = state
        .strips
        .iter()
        .find(|strip| {
            strip.kind == holaf_core::model::StripType::Input && strip.device_name.is_none()
        })
        .map(|strip| strip.uid.clone());
    let input = state
        .strips
        .iter()
        .find(|strip| strip.kind == holaf_core::model::StripType::Input)
        .map(|strip| strip.uid.clone());
    let output = state
        .strips
        .iter()
        .find(|strip| strip.kind == holaf_core::model::StripType::Output)
        .map(|strip| strip.uid.clone());

    let needs_engine = matches!(kind, "diagnostics" | "apps" | "device");
    if needs_engine {
        state.attach_engine(holaf_ui::bridge::EngineBridge::spawn_mock());
        // Let the mock answer: the UI is event driven, so this is a bounded poll
        // of the very same `poll_engine` the app runs every frame.
        for _ in 0..400 {
            state.poll_engine();
            let ready = match kind {
                "diagnostics" => state.backend_status.is_some(),
                "apps" => !state.app_list.is_empty(),
                _ => !state.engine_devices.is_empty(),
            };
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    match kind {
        "diagnostics" => {
            state.apply(holaf_ui::app::UiAction::OpenDiagnostics);
            // The `status` answer refreshes the report a frame later.
            for _ in 0..100 {
                state.poll_engine();
                if state
                    .diagnostics
                    .as_ref()
                    .is_some_and(|diagnostics| diagnostics.report.engine.available)
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if let Some(tab) = tab {
                let tab = match tab.to_lowercase().as_str() {
                    "summary" => holaf_ui::diagnostics::DiagTab::Summary,
                    "engine" => holaf_ui::diagnostics::DiagTab::Engine,
                    "graph" => holaf_ui::diagnostics::DiagTab::Graph,
                    "system" => holaf_ui::diagnostics::DiagTab::System,
                    "journal" | "log" => holaf_ui::diagnostics::DiagTab::Log,
                    other => return Err(format!("unknown --dialog-tab {other:?}")),
                };
                if let Some(diagnostics) = &mut state.diagnostics {
                    diagnostics.tab = tab;
                }
                if tab == holaf_ui::diagnostics::DiagTab::Log && holaf_ui::logbuf::len() == 0 {
                    // A capture is a fresh process with nothing to report (the
                    // engine is not attached, so no warning is ever logged): fill
                    // the buffer with the lines a real start emits, exactly like
                    // the MIDI port picker gets the names of a MIDI Mix.  Without
                    // this the "Journal" tab captures as an empty box.
                    for line in [
                        "09:31:02 INFO  holaf_ui: config: /home/user/.config/holaf-mix/config.json",
                        "09:31:02 INFO  holaf_ui::app: loaded 7 strips",
                        "09:31:02 INFO  holaf_ui: engine: PipeWire backend attached",
                        "09:31:03 WARN  holaf_ui::bridge::pw: engine: cannot connect to PipeWire: connect: Creation failed",
                        "09:31:03 INFO  holaf_ui::midi: MIDI Mix:MIDI Mix MIDI 1 24:0 opened",
                        "09:31:07 WARN  holaf_ui::devices: strip Mic: device alsa_input.gone not found",
                    ] {
                        holaf_ui::logbuf::push(line.to_string());
                    }
                }
            }
        }
        "apps" => {
            let uid = virtual_input
                .or(input)
                .ok_or_else(|| "no input strip to assign applications to".to_string())?;
            state.apply(holaf_ui::app::UiAction::SelectApps { uid });
        }
        "fx" => {
            let uid = input
                .or(output)
                .ok_or_else(|| "no strip to show an effect of".to_string())?;
            state.apply(holaf_ui::app::UiAction::OpenFxSettings {
                uid,
                effect: holaf_core::model::EffectKey::Eq,
            });
        }
        "device" => {
            let uid = output
                .or(input)
                .ok_or_else(|| "no strip to bind a device to".to_string())?;
            state.apply(holaf_ui::app::UiAction::OpenDevicePicker { uid });
        }
        "add" => state.apply(holaf_ui::app::UiAction::BeginAddStrip {
            kind: holaf_core::model::StripType::Input,
        }),
        "delete" => {
            let uid = output
                .or(input)
                .ok_or_else(|| "no strip to delete".to_string())?;
            state.apply(holaf_ui::app::UiAction::RequestDelete { uid });
        }
        "midi-port" => {
            // What the port picker looks like when the shell found controllers.
            // The list normally comes from ALSA (`state.midi_ports`, refreshed by
            // the app); a capture cannot open a sequencer, so it is filled with
            // the names a MIDI Mix box shows, and the reason a build without
            // `midi` would show none is exercised by `--dialog midi-port-none`.
            state.midi_ports = vec![
                "MIDI Mix:MIDI Mix MIDI 1 24:0".to_string(),
                "Midi Through:Midi Through Port-0 14:0".to_string(),
            ];
            state.midi_ports_error = None;
            state.apply(holaf_ui::app::UiAction::OpenMidiPortPicker);
        }
        "midi-port-none" => {
            // The degraded case: no sequencer, or a build without the feature.
            state.midi_ports.clear();
            state.midi_ports_error = Some(
                "no ALSA sequencer: /dev/snd/seq is missing - load the `snd-seq` module".to_string(),
            );
            state.apply(holaf_ui::app::UiAction::OpenMidiPortPicker);
        }
        "midi-learn" => {
            // The strip's MIDI button in `LEARNING...` (orange, and the whole
            // frame turns orange like `set_learning(true)`).
            let uid = input
                .or(virtual_input)
                .ok_or_else(|| "no input strip to learn a control for".to_string())?;
            state.apply(holaf_ui::app::UiAction::MidiLearnTarget {
                uid,
                target: holaf_ui::midi::MidiTarget::Volume,
            });
        }
        "midi-menu" => {
            // The per-strip MIDI menu (`Learn Volume` / `Learn Mute` /
            // `Learn Mono`, `Clear Mappings`).
            let uid = input
                .or(virtual_input)
                .ok_or_else(|| "no input strip to open the MIDI menu of".to_string())?;
            let label = state
                .strips
                .iter()
                .find(|strip| strip.uid == uid)
                .map(|strip| strip.label.clone())
                .unwrap_or_default();
            state.apply(holaf_ui::app::UiAction::OpenContextMenu {
                kind: holaf_ui::menus::ContextMenuKind::Midi {
                    uid,
                    label,
                    armed: None,
                },
                pos: egui::pos2(200.0, 200.0),
            });
        }
        "menu" => {
            // The context menu floats over the window: render it open.
            state.apply(holaf_ui::app::UiAction::OpenContextMenu {
                kind: holaf_ui::menus::ContextMenuKind::Window,
                pos: egui::pos2(120.0, 120.0),
            });
        }
        other => {
            return Err(format!(
                "unknown --dialog {other:?} \
                 (diagnostics|apps|fx|device|add|delete|menu|midi-port|midi-port-none|midi-learn|midi-menu)"
            ))
        }
    }
    // A capture is read-only: the mock engine must not survive it, and no
    // debounced write may touch the config file.
    if needs_engine {
        // `shutdown` detaches the mock engine (no thread survives the capture)
        // but keeps the payload it answered with, which is what the open dialog
        // is showing.
        state.shutdown();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The validated plain captures keep canvas == window: `docs/` shows them at
    /// the theme's 800 px and they must not change.
    #[test]
    fn a_plain_capture_renders_the_window_at_the_requested_size() {
        let state = AppState::demo();
        let (canvas, window) = capture_rects(&state, false, None, 800.0, 600.0);
        assert_eq!(canvas, egui::vec2(800.0, 600.0));
        assert_eq!(window.size(), egui::vec2(800.0, 600.0));
        // An explicit width is taken as-is (the caller asked for it).
        let (canvas, window) = capture_rects(&state, false, Some(1000.0), 800.0, 600.0);
        assert_eq!(canvas.x, 1000.0);
        assert_eq!(window.width(), 1000.0);
    }

    /// A dialog floats *over* the window (`egui::Area`): it must never widen the
    /// background, otherwise the two columns get stretched by
    /// `canvas/required` and the capture grows a trailing band no real window
    /// shows (900 px canvas → 72 px of background after the last input strip
    /// instead of 12).
    #[test]
    fn a_dialog_capture_locks_the_window_to_the_strip_count() {
        let state = AppState::demo();
        let required = holaf_ui::layout::required_width_for(&state.strips) as f32;
        assert_eq!(required, 794.0, "4 inputs (452) + 3 outputs (342)");

        // No --width: the canvas is exactly the window.
        let (canvas, window) = capture_rects(&state, true, None, 800.0, 700.0);
        assert_eq!(canvas, egui::vec2(required, 700.0));
        assert_eq!(window.size(), egui::vec2(required, 700.0));

        // --width 900: the canvas grows to fit the dialog, the window does not.
        let (canvas, window) = capture_rects(&state, true, Some(900.0), 800.0, 700.0);
        assert_eq!(canvas, egui::vec2(900.0, 700.0));
        assert_eq!(window.size(), egui::vec2(required, 700.0));

        // --width narrower than the lock cannot squeeze the columns either.
        let (canvas, window) = capture_rects(&state, true, Some(600.0), 800.0, 700.0);
        assert_eq!(canvas.x, required);
        assert_eq!(window.width(), required);
    }

    /// The 12 px of section background right of the last strip are the Qt
    /// formula's (`setContentsMargins(10)` + 2 px of slack, `MARGIN_TOTAL = 22`
    /// for 20 px of margins), not a margin — and the window lock above keeps them
    /// at 12 px whatever canvas the caller asks for.
    #[test]
    fn the_last_strip_keeps_its_qt_right_margin_at_any_canvas_width() {
        let state = AppState::demo();
        let theme = &state.theme.theme;
        let (inputs, outputs) = holaf_ui::layout::counts(&state.strips);
        let needed = holaf_ui::layout::section_width(inputs) as f32
            + holaf_ui::layout::section_width(outputs) as f32;
        for width in [794.0_f32, 900.0, 1440.0] {
            let (_, window) = capture_rects(&state, true, Some(width), 800.0, 700.0);
            // The `views::main_window` column rule: `setStretch` shares the window
            // in the ratio of the two `section_width`s.
            let scale = window.width() / needed;
            let input_w = holaf_ui::layout::section_width(inputs) as f32 * scale;
            assert_eq!(input_w, 452.0, "a {width} px canvas must not stretch it");
            assert_eq!(
                window.width() - input_w,
                342.0,
                "the output column keeps its Qt width too"
            );
            // Strips at x = 10, 120, 230, 340: the last one ends at 440.
            let last_strip_right = theme.dimensions.section_padding
                + 3.0 * (theme.dimensions.strip_width + theme.dimensions.strip_gap)
                + theme.dimensions.strip_width;
            assert_eq!(
                input_w - last_strip_right,
                12.0,
                "10 px of Qt margin + 2 px of formula slack"
            );
        }
    }
}
