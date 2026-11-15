//! `holaf-ui` — the desktop app (eframe + glow).
//!
//! Everything of interest lives in the library: this file parses the CLI, loads
//! the real `config.json` ([`AppState::from_config`]), attaches the engine and
//! wires the `eframe::App` shell to `holaf_ui::render` — the exact same call the
//! headless snapshot binary makes.  Keeping this thin is deliberate: a UI that
//! can only be looked at through a GPU window is a UI that cannot be tested.
//!
//! # Running
//!
//! ```text
//! cargo run -p holaf-ui                                # with the PipeWire engine + MIDI
//! cargo run -p holaf-ui -- --config /tmp/test.json     # a specific config
//! cargo run -p holaf-ui -- --midi-list                 # which controllers are visible
//! cargo run -p holaf-ui --no-default-features          # UI only, no engine, no MIDI
//! ```
//!
//! On a machine with no display server this exits with a clear error instead of
//! a panic (see `docs/ui-rust-design.md` §"Capture").
//!
//! # Why the MIDI device lives here
//!
//! The library (`holaf_ui`) owns the *policy* — which CC drives what, learn,
//! reconnection timing — in pure, always-compiled, always-tested code
//! (`holaf_ui::midi`).  Opening an ALSA port needs `midir` and a sequencer, so it
//! happens in this shell, in [`MidiDevice`]: open, retry every
//! [`RETRY_INTERVAL`](holaf_ui::midi::RETRY_INTERVAL), notice an unplugged
//! controller, and hand the messages to `AppState::handle_midi_message` like any
//! other user gesture.

use std::path::PathBuf;
use std::process::ExitCode;

use eframe::egui;
use holaf_ui::app::AppState;

/// The eframe shell: an [`AppState`] plus a frame counter used for logging.
struct HolafApp {
    state: AppState,
    frames: u64,
    /// Whether the window is currently shown.  The tray's Show/Hide entry toggles
    /// it; egui hides/shows the viewport with `ViewportCommand::Visible`.
    window_visible: bool,
    /// Set by the tray's Quit (or anything else that means "really leave"):
    /// the close request that follows must **not** be turned into a hide.
    ///
    /// This is what made "Quit" from the tray do nothing: it asked the window to
    /// close, the close request came back on the next frame, `close_action()`
    /// answered `Hide` (a tray exists and the preference is on) and the app went
    /// back to the tray instead of quitting.
    quitting: bool,
    /// The MIDI controller this window drives (`--features midi` only).
    #[cfg(feature = "midi")]
    midi: MidiDevice,
}

/// Command line of the desktop app (deliberately tiny: the config path is the
/// only thing a user or a test harness needs to vary).
#[derive(Debug, Clone, Default, PartialEq)]
struct Args {
    /// `--config <path>`; `None` = [`holaf_core::config::default_config_path`].
    config: Option<PathBuf>,
    /// `--midi <port name>`: the MIDI input to listen to (`--features midi`).
    midi_port: Option<String>,
    /// `--midi-list`: print the MIDI inputs and exit.
    midi_list: bool,
    /// `--no-always-on-top` / `--always-on-top`: override the stored flag.
    always_on_top: Option<bool>,
}

fn parse_args(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args::default();
    let mut argv = argv.peekable();
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let value = argv.next().ok_or_else(|| format!("{arg} needs a path"))?;
                args.config = Some(PathBuf::from(value));
            }
            "--midi" => {
                let value = argv
                    .next()
                    .ok_or_else(|| format!("{arg} needs a port name"))?;
                args.midi_port = Some(value.to_string());
            }
            "--midi-list" => args.midi_list = true,
            "--always-on-top" => args.always_on_top = Some(true),
            "--no-always-on-top" => args.always_on_top = Some(false),
            "--help" | "-h" => {
                return Err(
                    "usage: holaf-ui [--config FILE.json] [--midi PORT] [--midi-list]\n\
                     [--always-on-top | --no-always-on-top]\n\n\
                     Loads the given config.json (default: the repo's config.json) \
                     and drives the PipeWire engine.  The MIDI controller stored in \
                     ui_rust.midi_port is opened automatically (a MIDI Mix is picked \
                     by name on first run); --midi PORT overrides it for one run and \
                     --midi-list prints the port names to choose from.  The port can \
                     also be changed at run time: window menu > MIDI port..."
                        .to_string(),
                )
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    Ok(args)
}

impl HolafApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        state: AppState,
        #[cfg(feature = "midi")] midi: MidiDevice,
    ) -> Self {
        // The theme owns every colour the UI uses; egui's own style only
        // matters for the dialogs.
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        // Starting geometry.  Width: exactly what the strip count requires
        // (Qt's `setFixedWidth(Σ section_width)`), pinned by the size hints so a
        // horizontal drag cannot move it (`layout::size_hints`; re-asserted every
        // frame by `layout::enforce_window_width`).  Height: the one saved in
        // `config.json` (`window.height`), else the theme's, free for the user to
        // drag from then on.
        let theme = &state.theme.theme;
        let (_, outputs) = holaf_ui::layout::counts(&state.strips);
        let (min_size, max_size) = holaf_ui::layout::size_hints(
            holaf_ui::layout::required_width_for(&state.strips),
            holaf_ui::layout::minimum_window_height(theme, outputs),
        );
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::MinInnerSize(min_size));
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::MaxInnerSize(max_size));
        let height = state
            .geometry()
            .map(|geometry| geometry.height.max(200.0))
            .unwrap_or(theme.dimensions.window_height);
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                holaf_ui::layout::required_width_for(&state.strips) as f32,
                height,
            )));

        log::info!(
            "ui: {} strips, theme {:?}, engine {:?}",
            state.strips.len(),
            state.theme.name,
            state.engine
        );
        Self {
            state,
            frames: 0,
            window_visible: true,
            quitting: false,
            #[cfg(feature = "midi")]
            midi,
        }
    }
}

impl eframe::App for HolafApp {
    /// Close button: quit, unless a tray icon is available *and* the user asked
    /// for hide-on-close (Qt's `closeEvent` rule, see [`holaf_ui::tray`]).
    ///
    /// Whether closing hides or quits is decided in [`Self::intercept_close`]
    /// by `tray::available()`: with no tray icon (no StatusNotifier host, or a
    /// build without the `tray` feature) the close is allowed and the app
    /// exits — hiding it would leave an unreachable process.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Shutdown mirrors `make_shutdown_all`: pending config write, engine
        // `Shutdown`, join — no PipeWire object or capture child survives.
        self.state.shutdown();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frames += 1;
        if self.frames == 1 {
            log::info!(
                "first frame: {} strips, theme {:?}",
                self.state.strips.len(),
                self.state.theme.name
            );
        }
        // The close button: hide (and stay alive in the tray) only when a tray
        // icon is actually there; otherwise let the window close and the app exit
        // (`on_exit` → `shutdown`).  This is the Qt `closeEvent` rule.
        self.intercept_close(ui.ctx());
        self.pump_midi(ui.ctx());
        self.pump_tray(ui.ctx());
        holaf_ui::render(&mut self.state, ui);
    }
}

impl HolafApp {
    /// Turn a WM close request into the [`holaf_ui::tray::CloseAction`]: with a
    /// tray + the preference, the window is hidden (`CancelClose` + `Visible`
    /// false) and the app keeps running; otherwise the close is allowed and
    /// eframe runs `on_exit`/`shutdown`.
    fn intercept_close(&mut self, ctx: &egui::Context) {
        let requested = ctx.input(|input| input.viewport().close_requested());
        if !requested {
            return;
        }
        // `quitting` is the tray's Quit entry: it always means "quit", even
        // when closing the window would only hide it (`close_action_with`).
        match self.state.close_action(self.quitting) {
            holaf_ui::tray::CloseAction::Hide => {
                log::info!("ui: close requested - hiding to the tray");
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.window_visible = false;
            }
            holaf_ui::tray::CloseAction::Quit => {
                log::info!(
                    "ui: close requested - quitting ({})",
                    if self.quitting {
                        "tray Quit"
                    } else {
                        "no tray"
                    }
                );
            }
        }
    }

    /// Dispatch the system tray's events (feature `tray`; a no-op otherwise).
    ///
    /// Polled once per frame from the UI thread — `muda`'s channel is `Send`, so
    /// this reads the menu clicks without touching GTK.  Each [`TrayEvent`] maps
    /// onto exactly what the equivalent window control does (context-menu
    /// "Restart audio engine", the close button, show/hide).
    fn pump_tray(&mut self, ctx: &egui::Context) {
        for event in holaf_ui::tray::poll() {
            match event {
                holaf_ui::tray::TrayEvent::ToggleWindow => {
                    self.window_visible = !self.window_visible;
                    log::info!(
                        "tray: {}",
                        if self.window_visible { "show window" } else { "hide window" }
                    );
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(self.window_visible));
                }
                holaf_ui::tray::TrayEvent::RestartEngine => {
                    log::info!("tray: restart audio engine");
                    self.state.restart_engine(std::time::Instant::now());
                }
                holaf_ui::tray::TrayEvent::Quit => {
                    log::info!("tray: quit");
                    // `ViewportCommand::Close` runs the normal exit path
                    // (`on_exit` → `AppState::shutdown`): no PipeWire object or
                    // capture child survives.  The `quitting` flag is what keeps
                    // `intercept_close` from turning this close request back into
                    // "hide to the tray" — the bug that made the Quit entry do
                    // nothing at all.
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// Move messages between the controller and the model (`--features midi`).
    /// The port lives in [`MidiDevice`], not in `AppState`: the UI library stays
    /// free of ALSA (its MIDI policy is pure), and the shell is the only place
    /// that touches a device.  A closed/disappeared port is a status line, never
    /// a crash.
    fn pump_midi(&mut self, #[allow(unused_variables)] ctx: &egui::Context) {
        #[cfg(feature = "midi")]
        {
            // Hand the context to the reader thread the first time (the port may
            // be opened later, at any point of the session).
            if self.midi.ctx.is_none() {
                self.midi.ctx = Some(ctx.clone());
            }
            if self.midi.pump(&mut self.state, std::time::Instant::now()) {
                // A controller message is user input, exactly like a mouse move:
                // repaint **now** instead of waiting for the next 40 ms meter
                // tick, otherwise a knob sweep is drawn in 40 ms steps and feels
                // "saccadé" while the mouse stays smooth.
                ctx.request_repaint();
            }
        }
        #[cfg(not(feature = "midi"))]
        {
            // Without the `midi` feature the mappings still load, show and save
            // (they are config data); only the device I/O is missing.  Say so
            // instead of pretending a controller could work.
            if self.state.midi_ports_error.is_none() {
                self.state.midi_ports_error = Some(
                    "this build has no MIDI support (rebuild with the `midi` feature)".to_string(),
                );
            }
            let pending = self.state.take_midi_feedback();
            if !pending.is_empty() {
                log::debug!(
                    "midi: {} LED message(s) dropped (build without --features midi)",
                    pending.len()
                );
            }
        }
    }
}

/// How often the port list is re-read after a failure.
///
/// A failure to *list* ports is a static condition (no ALSA sequencer on this
/// machine), not a flapping device: retrying it every 2 s would only spam the
/// console with ALSA's own stderr message.  A port that is merely absent keeps the
/// 2 s retry ([`holaf_ui::midi::RETRY_INTERVAL`]) so plugging the controller back
/// in is picked up at once.
#[cfg(feature = "midi")]
const PROBE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

/// The MIDI controller driver of the app (`--features midi`).
///
/// Everything it *decides* is in [`holaf_ui::midi::MidiRuntime`] (pure, tested);
/// this type only performs the I/O: probe the port list, open, drain, send the
/// LEDs, drop the connection when the device disappears.  It is polled once per
/// frame, so nothing here may block.
#[cfg(feature = "midi")]
struct MidiDevice {
    runtime: holaf_ui::midi::MidiRuntime,
    port: Option<holaf_ui::midi::MidiPort>,
    /// When the port list is read again (presence check + status), `None` = now.
    next_probe: Option<std::time::Instant>,
    /// When the controller's lamps are re-synced with the mixer.
    next_sync: std::time::Instant,
    /// The last port-list failure, so a missing ALSA sequencer is logged once per
    /// change instead of twice a second.
    list_error: Option<String>,
    /// The window's `egui::Context`, handed to the MIDI reader thread so a
    /// message can wake the UI at once (`MidiPort::open_with_wake`).  Set on the
    /// first frame: without it, a knob would only be *seen* on the next vumeter
    /// tick (40 ms), which is exactly what made MIDI feel jerky next to the mouse.
    ctx: Option<egui::Context>,
}

#[cfg(feature = "midi")]
impl MidiDevice {
    fn new(desired: Option<String>) -> Self {
        Self {
            runtime: holaf_ui::midi::MidiRuntime::new(desired),
            port: None,
            next_probe: None,
            next_sync: std::time::Instant::now(),
            list_error: None,
            ctx: None,
        }
    }

    /// One frame of MIDI work: follow the selection, (re)connect, move messages
    /// and LEDs, keep the status line honest.
    ///
    /// Returns `true` when at least one controller message was applied this
    /// frame: the shell then asks for an immediate repaint, so a knob sweep is
    /// drawn as fast as the controller sends it instead of once per meter tick.
    fn pump(&mut self, state: &mut AppState, now: std::time::Instant) -> bool {
        // 1. The user may have picked another port (menu, picker, --midi).
        if self.runtime.set_desired(state.midi_port_request.clone()) {
            self.close(state, "controller changed");
            self.next_probe = None;
        }

        // 2. Probe the ports: the picker's list, and the answer to "is my
        //    controller still there?".
        let probed = self.probe(state, now);
        if let Some(port) = &self.port {
            if !state.midi_ports.iter().any(|name| name == port.name()) {
                let name = port.name().to_string();
                log::warn!("midi: controller {name:?} disappeared - retrying");
                self.close(state, &format!("{name} disconnected"));
            }
        }

        // 3. (Re)open what is due.  A failed *list* means no sequencer at all:
        //    opening would fail the same way, so the attempt is skipped and the
        //    status line carries the reason.
        if probed {
            if let Some(name) = self.runtime.attempt(now) {
                self.open(state, &name);
            }
        }

        // 4. Messages in, LEDs out.
        let handled = self.drain_events(state, now);
        if self.port.is_some() && now >= self.next_sync {
            self.next_sync = now + std::time::Duration::from_secs(1);
            // A mute toggled from the mouse, a strip added, a label changed: the
            // controller's lamps must follow the mixer, not the last MIDI event.
            state.restore_midi_button_states();
        }
        let feedback = state.take_midi_feedback();
        match &self.port {
            Some(port) => {
                for message in feedback {
                    port.send(message);
                }
            }
            None => {
                if !feedback.is_empty() {
                    log::debug!(
                        "midi: {} LED message(s) dropped (no controller)",
                        feedback.len()
                    );
                }
            }
        }
        handled
    }

    /// Read the port list; returns whether it could be read.
    fn probe(&mut self, state: &mut AppState, now: std::time::Instant) -> bool {
        if self.next_probe.is_some_and(|at| now < at) {
            return self.list_error.is_none() && state.midi_ports_error.is_none();
        }
        self.next_probe = Some(now + holaf_ui::midi::RETRY_INTERVAL);
        match holaf_ui::midi::MidiPort::ports() {
            Ok(ports) => {
                state.midi_ports = ports.inputs;
                state.midi_ports_error = None;
                if self.list_error.take().is_some() {
                    log::info!("midi: the MIDI port list is readable again");
                }
                true
            }
            Err(err) => {
                state.midi_ports.clear();
                state.midi_ports_error = Some(err.clone());
                if self.list_error.as_deref() != Some(err.as_str()) {
                    log::warn!("midi: cannot list the MIDI ports: {err}");
                    self.list_error = Some(err.clone());
                }
                // Static failure (no sequencer): do not hammer it.
                self.next_probe = Some(now + PROBE_BACKOFF);
                self.runtime.on_failed(err);
                state.midi_connected = false;
                if let Some(line) = self.runtime.status_line() {
                    state.status = Some(line);
                }
                false
            }
        }
    }

    fn open(&mut self, state: &mut AppState, name: &str) {
        match holaf_ui::midi::MidiPort::open_with_wake(name, self.ctx.clone()) {
            Ok(port) => {
                log::info!("midi: listening on {name:?}");
                self.port = Some(port);
                self.runtime.on_opened(name);
                state.midi_port_name = Some(name.to_string());
                state.midi_connected = true;
                state.status = Some(format!("MIDI: {name}"));
                // The lamps must match the mixer at once (`_sync_initial_midi_leds`,
                // which Qt delayed by 200 ms because some controllers drop the
                // very first messages).
                self.next_sync = std::time::Instant::now();
            }
            Err(err) => {
                log::warn!("midi: cannot open {name:?}: {err}");
                self.runtime.on_failed(err);
                state.midi_connected = false;
                if let Some(line) = self.runtime.status_line() {
                    state.status = Some(line);
                }
            }
        }
    }

    fn close(&mut self, state: &mut AppState, reason: &str) {
        self.port = None;
        self.runtime.on_lost(reason);
        state.midi_connected = false;
        state.midi_port_name = None;
        if let Some(line) = self.runtime.status_line() {
            state.status = Some(line);
        }
    }

    /// Apply everything the controller has queued.  Returns how many messages
    /// were handled (the shell repaints at once when it is not zero).
    ///
    /// The port's reader thread fills a channel as the messages arrive, so this
    /// drain is the only latency between the knob and the fader — there is no
    /// throttle left in the path (`midi::decode` applies every message, the
    /// engine side coalesces to 25 Hz with last-value-wins).
    fn drain_events(&mut self, state: &mut AppState, now: std::time::Instant) -> bool {
        let Some(port) = &self.port else { return false };
        let mut handled = false;
        for event in port.drain() {
            match event {
                holaf_ui::midi::MidiEvent::Message(message) => {
                    state.handle_midi_message(message, now);
                    handled = true;
                }
                holaf_ui::midi::MidiEvent::Opened(name) => {
                    log::info!("midi: port {name:?} opened");
                }
                holaf_ui::midi::MidiEvent::Error(reason) => {
                    log::warn!("midi: {reason}");
                    self.close(state, &reason);
                    return handled;
                }
            }
        }
        handled
    }
}

/// List the MIDI ports the system offers, at startup.
///
/// This is the single most useful thing the log can say for a controller that
/// does not answer: the exact names `--midi` and the port picker expect (with
/// several devices, "MIDI Mix MIDI 1" and "Midi Through Port-0" are otherwise
/// indistinguishable from a typo).
#[cfg(feature = "midi")]
fn log_midi_ports(state: &mut AppState) {
    match holaf_ui::midi::MidiPort::ports() {
        Ok(ports) => {
            state.midi_ports = ports.inputs.clone();
            state.midi_ports_error = None;
            if ports.inputs.is_empty() {
                log::info!("midi: no MIDI input found (plug the controller in, see --midi-list)");
            } else {
                log::info!("midi: {} input port(s):", ports.inputs.len());
                for name in &ports.inputs {
                    log::info!("midi:   in  {name}");
                }
            }
            for name in &ports.outputs {
                log::info!("midi:   out {name} (LED feedback)");
            }
        }
        Err(err) => {
            state.midi_ports_error = Some(err.clone());
            log::warn!("midi: cannot list the MIDI ports: {err}");
            // Said once, in the status bar: a build/box without an ALSA
            // sequencer is a fact the user should see, not only read in a log.
            state.status = Some(format!("MIDI unavailable: {err}"));
        }
    }
}

/// Pick a controller when the config does not name one.
///
/// `ui_rust.midi_port` absent = never chosen: recognise the MIDI Mix (or the only
/// port there is) and store it, so the next start connects straight to it.  The Qt
/// UI re-picked a port at every start (`_init_midi`: first name containing "mix",
/// else the first port); here the choice is remembered, and an *explicit* "no
/// controller" is never overridden.
#[cfg(feature = "midi")]
fn auto_select_midi_port(state: &mut AppState) {
    if !state.midi_port_auto || state.midi_port_request.is_some() {
        return;
    }
    let ports = state.midi_ports.clone();
    match holaf_ui::midi::choose_port(&ports, None) {
        Some(name) => {
            log::info!("midi: selected {name:?} automatically (MIDI port menu to change it)");
            state.adopt_midi_port(&name, std::time::Instant::now());
        }
        None if !ports.is_empty() => log::info!(
            "midi: {} controller(s) found, none recognised - pick one in the MIDI port menu",
            ports.len()
        ),
        None => {}
    }
}

fn main() -> ExitCode {
    holaf_ui::logbuf::install("info");

    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(feature = "midi")]
    if args.midi_list {
        match holaf_ui::midi::MidiPort::ports() {
            Ok(ports) => {
                if ports.inputs.is_empty() {
                    println!("no MIDI input found");
                } else {
                    println!("MIDI inputs ({}) - the names --midi accepts:", ports.inputs.len());
                    for name in &ports.inputs {
                        println!("  {name}");
                    }
                }
                if ports.outputs.is_empty() {
                    println!("no MIDI output found (LED feedback unavailable)");
                } else {
                    println!("MIDI outputs ({}) - LED feedback:", ports.outputs.len());
                    for name in &ports.outputs {
                        println!("  {name}");
                    }
                }
            }
            Err(err) => {
                eprintln!("cannot query the MIDI ports: {err}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }

    let config_path = args
        .config
        .unwrap_or_else(holaf_core::config::default_config_path);
    log::info!("config: {}", config_path.display());

    // Only mutated when a backend is compiled in (see the `cfg` block below).
    #[allow(unused_mut)]
    let mut state = AppState::from_config(&config_path);
    // The flag lives in `config.json` (`ui_rust.always_on_top`, default `true`
    // like the Qt `WindowStaysOnTopHint`); the command line is the override a
    // user needs on a WM that fights it.
    if let Some(value) = args.always_on_top {
        state.always_on_top = value;
    }

    #[cfg(feature = "engine-pw")]
    {
        // The backend connects (and applies the loaded snapshot) on its own
        // thread; a missing daemon becomes a visible banner, never a crash.
        state.attach_engine(holaf_ui::bridge::EngineBridge::spawn_pipewire());
        log::info!("engine: PipeWire backend attached");
    }
    #[cfg(not(feature = "engine-pw"))]
    log::info!("engine: no backend compiled in (demo meters), see feature `engine-pw`");

    // Tray policy: with no tray icon the close button must quit (it does), and
    // the "always on top" flag is a *stored preference* here instead of the
    // unconditional `WindowStaysOnTopHint` the Qt build set.
    //
    // The tray itself is behind the `tray` feature (ksni: a D-Bus
    // StatusNotifierItem, no GTK).  `install()` never panics: a missing
    // StatusNotifier host, session bus or display logs a warning and
    // `available()` stays `false`, so closing the window quits — the safe
    // fallback on a bare Wayland session with no tray host.
    holaf_ui::tray::install();
    log::info!(
        "ui: always_on_top={}, tray={}",
        state.always_on_top,
        if holaf_ui::tray::available() {
            "available"
        } else {
            "unavailable"
        }
    );
    // The invariant `close_action` encodes: hide only when a tray exists.  A
    // build/time without a tray must quit, never hide an unreachable window.
    if holaf_ui::tray::available() {
        log::info!("ui: closing the window hides it (the tray keeps the app alive)");
    } else {
        log::info!("ui: closing the window quits (no tray icon compiled/available)");
    }

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
    //
    // The controller is chosen *before* the window is created: a missing or
    // unplugged one is then a warning in the log and a line in the status bar,
    // never a failed window.
    #[cfg(feature = "midi")]
    let midi = {
        // `--midi PORT` overrides the stored controller for this run (a test
        // harness, a second controller); it is not persisted.
        if let Some(name) = args.midi_port.clone() {
            log::info!("midi: --midi {name:?} overrides the stored controller");
            state.midi_port_request = Some(name);
            state.midi_port_auto = false;
        }
        log_midi_ports(&mut state);
        auto_select_midi_port(&mut state);
        if let Some(name) = &state.midi_port_request {
            log::info!("midi: will listen on {name:?}");
        }
        MidiDevice::new(state.midi_port_request.clone())
    };

    match eframe::run_native(
        "Holaf-Mix",
        options,
        Box::new(move |cc| {
            Ok(Box::new(HolafApp::new(
                cc,
                state,
                #[cfg(feature = "midi")]
                midi,
            )))
        }),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("holaf-ui: cannot open a window: {err}");
            let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
            let x11 = std::env::var_os("DISPLAY").is_some();
            if !wayland && !x11 {
                eprintln!("holaf-ui: no display server (DISPLAY / WAYLAND_DISPLAY are unset).");
                eprintln!("           To render the UI to a PNG instead:");
                eprintln!(
                    "           cargo run -p holaf-ui --bin holaf-ui-snapshot -- --out /tmp/holaf-ui.png"
                );
            } else if !wayland {
                // This build targets Wayland (see `crates/holaf-ui/Cargo.toml`):
                // there is no X11 backend compiled in, so an X11-only session
                // cannot show it.  Say so, and say how to get one.
                eprintln!(
                    "holaf-ui: this build is Wayland-only (no X11 backend compiled in), \
                     and WAYLAND_DISPLAY is unset."
                );
                eprintln!(
                    "           For an X11-only session, rebuild with: cargo build -p holaf-ui \
                     --features x11"
                );
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_flag_is_parsed() {
        let args = parse_args(["--config", "/tmp/x.json"].iter().map(|s| s.to_string())).unwrap();
        assert_eq!(args.config, Some(PathBuf::from("/tmp/x.json")));
        let args = parse_args(std::iter::empty()).unwrap();
        assert_eq!(args.config, None);
    }

    #[test]
    fn the_always_on_top_flags_are_parsed() {
        let args = parse_args(["--always-on-top"].iter().map(|arg| arg.to_string())).unwrap();
        assert_eq!(args.always_on_top, Some(true));
        let args = parse_args(["--no-always-on-top"].iter().map(|arg| arg.to_string())).unwrap();
        assert_eq!(args.always_on_top, Some(false));
        assert_eq!(parse_args(std::iter::empty()).unwrap().always_on_top, None);
    }

    #[test]
    fn bad_arguments_are_reported_not_panicked() {
        assert!(parse_args(["--config"].iter().map(|s| s.to_string())).is_err());
        assert!(parse_args(["--nope"].iter().map(|s| s.to_string())).is_err());
        let help = parse_args(["--help"].iter().map(|s| s.to_string())).unwrap_err();
        assert!(help.contains("usage: holaf-ui"));
    }

    /// A configured controller that is **not there** must not fail the app: the
    /// status bar says so, the port list is still published for the picker, and
    /// the retry loop keeps trying (instead of hammering every frame).
    ///
    /// Runs in both worlds on purpose: on a desktop the ALSA sequencer answers
    /// and the open fails ("no MIDI input named …"); in a container without
    /// `/dev/snd/seq` the port *list* fails first. The user-visible outcome is the
    /// same.
    #[cfg(feature = "midi")]
    #[test]
    fn a_missing_controller_is_reported_and_retried_not_fatal() {
        use std::time::{Duration, Instant};

        let mut state = AppState::demo();
        let missing = "holaf-ui:no-such-controller 255:0".to_string();
        state.midi_port_request = Some(missing.clone());
        state.midi_port_auto = false;
        let mut device = MidiDevice::new(state.midi_port_request.clone());

        let now = Instant::now();
        device.pump(&mut state, now);
        assert!(!state.midi_connected, "nothing was opened");
        assert!(!device.runtime.is_open());
        assert_eq!(state.midi_port_name, None);
        let line = state.status.clone().expect("the status bar explains it");
        assert!(line.contains(&missing), "{line}");
        assert!(line.contains("retrying"), "{line}");
        // Either the list was readable (and is offered to the picker) or the
        // reason it was not is published: never a silent void.
        assert!(
            !state.midi_ports.is_empty() || state.midi_ports_error.is_some(),
            "the picker needs either the ports or the reason"
        );
        // The retry has a floor (2 s): half a second later nothing is attempted
        // again, so a missing controller does not spin every frame.  (When the
        // sequencer itself is missing, the *list* failed and no open was tried at
        // all — the backoff is 10 s there, see `probe`.)
        let probed = state.midi_ports_error.is_none();
        if probed {
            let attempted_at = device
                .runtime
                .last_attempt()
                .expect("the first attempt is recorded");
            device.pump(&mut state, now + Duration::from_millis(500));
            assert_eq!(
                device.runtime.last_attempt(),
                Some(attempted_at),
                "no attempt before the retry interval"
            );
            let mut clock = device.runtime.clone();
            assert_eq!(
                clock.attempt(attempted_at + std::time::Duration::from_millis(1999)),
                None,
                "not due one millisecond early"
            );
            assert!(
                clock
                    .attempt(attempted_at + holaf_ui::midi::RETRY_INTERVAL)
                    .is_some(),
                "due again after the interval"
            );
        }
        assert!(!state.midi_connected, "and nothing was opened in between");

        // Selecting "No controller" stops the loop altogether.
        state.apply(holaf_ui::app::UiAction::MidiPortSelected { name: None });
        device.pump(&mut state, now + Duration::from_secs(6));
        assert_eq!(device.runtime.desired(), None);
        assert_eq!(device.runtime.attempt(now + Duration::from_secs(8)), None);
        assert_eq!(state.midi_port_request, None);
    }
}
