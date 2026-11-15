//! System tray icon + "always on top" — the Rust twin of the Qt
//! `_init_tray_icon` / `WindowStaysOnTopHint` / `closeEvent` trio.
//!
//! # What the Qt UI does
//!
//! * `self.setWindowFlags(… | Qt.WindowStaysOnTopHint)` — the mixer floats above
//!   everything, unconditionally;
//! * a `QSystemTrayIcon` with `Show/Hide` and `Quit`;
//! * `closeEvent` hides the window instead of quitting **when the tray is
//!   visible** (otherwise closing the window would leave an unrecoverable app).
//!
//! # What is implemented in Rust
//!
//! * **always on top: done.** `egui::ViewportCommand::WindowLevel(
//!   WindowLevel::AlwaysOnTop)` is sent by [`crate::app::render`] whenever
//!   [`crate::app::AppState::always_on_top`] changes; the flag is stored under
//!   `ui_rust.always_on_top` (a Rust-only key, see [`crate::config`]) and
//!   defaults to `true`, like Qt.
//! * **tray: behind the `tray` cargo feature**, which the SHIPPED build enables
//!   (`scripts/build_rust.sh` passes `--features tray` unless `HOLAF_NO_TRAY=1`)
//!   but which is not in the crate's `default` feature set, so a bare
//!   `cargo build -p holaf-ui` / `cargo test --workspace` stays hermetic.
//!
//!   The backend is **`ksni`** (cargo feature `tray`): a pure D-Bus
//!   `org.kde.StatusNotifierItem`, i.e. **no GTK3, no `libappindicator`, no
//!   GMainLoop**.  That is the native mechanism of the user's desktop
//!   (KDE/Plasma on Wayland), and it removes `gtk 0.18` — a closed line with no
//!   possible update — from the dependency graph (see
//!   `docs/dependency-audit.md`).  `tray-gtk` (libappindicator + GTK3) is kept
//!   as an escape hatch: `cargo build -p holaf-ui --features tray-gtk`.
//!
//!   `tray-icon` is pulled with `default-features = false`, which drops its
//!   default `libappindicator` backend (and `libxdo`, only used by the
//!   predefined Copy/Paste items) — so the default tray links no GTK at all.
//!
//!   The *policy* (which entries the menu holds, what closing does), the
//!   **icon itself** ([`icon_rgba`], drawn pixel by pixel) and the event mapping
//!   (a menu id to a [`TrayEvent`]) are all pure and unit-tested here — in every
//!   feature configuration.  Only the tray-icon/D-Bus I/O lives in [`backend`],
//!   so wiring is one `poll` per frame and never blocks the UI thread.
//!
//! # StatusNotifier host detection
//!
//! A tray icon needs a StatusNotifier **host** on the session bus
//! (`kstatusnotifieritem`/plasma, waybar, `swaybar`'s tray,
//! `gnome-shell-extension-appindicator`).  Qt's `isSystemTrayAvailable()` asks
//! D-Bus for `org.kde.StatusNotifierWatcher`; [`host_probe::plan`] does the same
//! here, because **`tray-icon`'s ksni backend deliberately does not**: it spawns
//! with `assume_sni_available(true)`, so it "succeeds" with no host and nothing
//! would ever be drawn.  Without that check, "close hides the window" becomes a
//! trap: the window disappears and there is no icon to bring it back.
//!
//! So [`available`] is `false` — and closing therefore **quits** — when the
//! watcher name has no owner, or when the bus cannot be reached at all (no
//! `DBUS_SESSION_BUS_ADDRESS`, a headless CI run, a bare tty).  An inconclusive
//! probe (an error that says nothing about the host) still builds the icon:
//! only a definite "no host" is treated as no host.

/// The tray menu entries.
///
/// The window's right-click menu is **gone** (user request, 2026-11-15: “je ne
/// veux plus d'action au click droit sur la fenêtre, je veux que le contenu du
/// menu soit déplacé au click sur le tray icon”), so every entry it carried now
/// lives here: the MIDI port picker, `Restart audio engine`, `Debug window` and
/// `Open project page`.  The per-strip menus (`Rename`/`Delete`, FX settings,
/// MIDI learn) stay contextual — they are about one strip, not the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEntry {
    /// `Show/Hide` — toggles the window (`_toggle_window`).
    ShowHide,
    /// `MIDI port...` — the controller picker of the (removed) window menu.
    MidiPort,
    /// `Restart audio engine` — the same action as the context menu.
    RestartEngine,
    /// `Debug window` — the diagnostics dialog (`UiAction::OpenDiagnostics`).
    OpenDiagnostics,
    /// `Open project page` — the repository home page.
    OpenProjectPage,
    /// `Quit` — `_force_quit`: stop the engine, then leave.
    Quit,
}

impl TrayEntry {
    /// The label the tray shows (the very strings the menus use, so the two can
    /// never drift).
    pub fn label(self) -> &'static str {
        match self {
            TrayEntry::ShowHide => "Show/Hide",
            TrayEntry::MidiPort => crate::menus::MENU_MIDI_PORT,
            TrayEntry::RestartEngine => crate::menus::MENU_RESTART_ENGINE,
            TrayEntry::OpenDiagnostics => crate::menus::MENU_DEBUG_WINDOW,
            TrayEntry::OpenProjectPage => crate::menus::MENU_OPEN_PROJECT_PAGE,
            TrayEntry::Quit => "Quit",
        }
    }
}

/// The tray menu, `None` being a separator: the window toggle alone at the top
/// (what a user reaches for on a hidden window), then the window menu's content,
/// then `Quit` kept apart so it is never a mis-click away from `Restart`.
pub fn menu_layout() -> Vec<Option<TrayEntry>> {
    vec![
        Some(TrayEntry::ShowHide),
        None,
        Some(TrayEntry::MidiPort),
        Some(TrayEntry::RestartEngine),
        Some(TrayEntry::OpenDiagnostics),
        Some(TrayEntry::OpenProjectPage),
        None,
        Some(TrayEntry::Quit),
    ]
}

/// The entries of [`menu_layout`], separators dropped (the order the labels are
/// asserted in).
pub fn entries() -> Vec<TrayEntry> {
    menu_layout().into_iter().flatten().collect()
}

/// What the window's close button must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Close the app (default: there is no tray to bring it back from).
    Quit,
    /// Hide the window; the tray keeps the app alive.
    Hide,
}

/// The Qt `closeEvent` rule: hide **only** when a tray icon is actually there.
///
/// Hiding the window with no way to restore it would leave a running process
/// nobody can reach — the exact bug the Qt code guards against with
/// `isSystemTrayAvailable()`.
pub fn close_action(tray_available: bool, hide_on_close: bool) -> CloseAction {
    if tray_available && hide_on_close {
        CloseAction::Hide
    } else {
        CloseAction::Quit
    }
}

/// [`close_action`] for a close request that may be an **explicit quit** (the
/// tray's `Quit` entry, `_force_quit` in Qt).
///
/// `explicit_quit` wins over everything: "quit" must quit even when closing the
/// window only hides it.  Without that rule the tray entry did nothing at all —
/// it asked the window to close, the close request came back, and the hide rule
/// turned it into "go back to the tray".
pub fn close_action_with(
    explicit_quit: bool,
    tray_available: bool,
    hide_on_close: bool,
) -> CloseAction {
    if explicit_quit {
        CloseAction::Quit
    } else {
        close_action(tray_available, hide_on_close)
    }
}

/// One thing the tray can ask the app to do.  The UI shell turns each into the
/// matching [`crate::app::AppState`] call / viewport command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// The window is hidden → show it; visible → hide it.
    ToggleWindow,
    /// Restart the audio engine (`AppState::restart_engine`).
    RestartEngine,
    /// Open the diagnostics window (`UiAction::OpenDiagnostics`); the window is
    /// shown first, otherwise the dialog would open behind a hidden window.
    OpenDiagnostics,
    /// Open the project page (`UiAction::OpenProjectPage`).
    OpenProjectPage,
    /// Open the MIDI port picker (`UiAction::OpenMidiPortPicker`) — shown first,
    /// like the diagnostics dialog (it is a dialog of the window).
    MidiPortPicker,
    /// Stop the engine and leave (`AppState::shutdown` + close the viewport).
    Quit,
}

impl TrayEntry {
    /// The stable menu id, used to recognise the click in `muda`'s event stream.
    /// Kept short and prefixed so it cannot collide with another item.
    pub fn id(self) -> &'static str {
        match self {
            TrayEntry::ShowHide => "holaf.tray.show_hide",
            TrayEntry::MidiPort => "holaf.tray.midi_port",
            TrayEntry::RestartEngine => "holaf.tray.restart_engine",
            TrayEntry::OpenDiagnostics => "holaf.tray.diagnostics",
            TrayEntry::OpenProjectPage => "holaf.tray.project_page",
            TrayEntry::Quit => "holaf.tray.quit",
        }
    }

    /// What activating this entry does to the app.
    pub fn event(self) -> TrayEvent {
        match self {
            TrayEntry::ShowHide => TrayEvent::ToggleWindow,
            TrayEntry::MidiPort => TrayEvent::MidiPortPicker,
            TrayEntry::RestartEngine => TrayEvent::RestartEngine,
            TrayEntry::OpenDiagnostics => TrayEvent::OpenDiagnostics,
            TrayEntry::OpenProjectPage => TrayEvent::OpenProjectPage,
            TrayEntry::Quit => TrayEvent::Quit,
        }
    }
}

/// Map a menu id back to an entry (pure): `None` for an id we did not create.
pub fn entry_for_id(id: &str) -> Option<TrayEntry> {
    entries().into_iter().find(|entry| entry.id() == id)
}

/// Map a menu id to the action the app must perform (pure).
///
/// This is the whole "tray backend → AppState" adapter: the shell polls
/// `muda`'s event
/// receiver each frame, looks the id up here, and never has to know about menu
/// widgets.  An unknown id (another application's tray, or a future item) is
/// ignored instead of mishandled.
pub fn menu_action(id: &str) -> Option<TrayEvent> {
    entry_for_id(id).map(TrayEntry::event)
}

/// The result of asking D-Bus whether a StatusNotifier **host** exists.
///
/// Qt's `QSystemTrayIcon::isSystemTrayAvailable()` is the behaviour being
/// ported; the check matters because `tray-icon`'s ksni backend spawns with
/// `assume_sni_available(true)` — it reports success even with no host, and an
/// app that then hides its window on close is unreachable forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostProbe {
    /// `org.kde.StatusNotifierWatcher` has an owner: an icon will be drawn.
    Present,
    /// The name has no owner, or the session bus could not be reached: no host
    /// can exist, so no icon can be drawn.
    Absent,
    /// The lookup failed in a way that says nothing about the host (a D-Bus
    /// quirk): build the icon and let `tray-icon` decide.
    Unknown,
}

impl HostProbe {
    /// Classify a D-Bus name-owner lookup — **pure**, hence unit-tested.
    ///
    /// `Ok(true)` = the watcher name is owned, `Ok(false)` = it is not,
    /// `Err(())` = the question could not be asked (no session bus, timeout,
    /// permission).  A bus we cannot reach means no host exists, which is why
    /// that case is [`HostProbe::Absent`] and not `Unknown`.
    pub fn from_lookup(result: Result<bool, ()>) -> Self {
        match result {
            Ok(true) => HostProbe::Present,
            Ok(false) => HostProbe::Absent,
            Err(()) => HostProbe::Absent,
        }
    }

    /// Is the probe inconclusive (build anyway, let `tray-icon` decide)?
    ///
    /// Reachable from the backend when the bus exists but answering the
    /// question fails; `from_lookup` never returns it (it is conservative).
    pub fn is_unknown(self) -> bool {
        matches!(self, HostProbe::Unknown)
    }

    /// Should the icon be built?  Only a **definite** "no host" skips it.
    pub fn build_icon(self) -> bool {
        !matches!(self, HostProbe::Absent)
    }
}

/// Is a tray icon available in this build (and did it initialise)?
///
/// `false` unless the `tray` feature is compiled **and** the icon was actually
/// created at startup (see [`backend::install`]).  Call sites never change.
pub fn available() -> bool {
    #[cfg(any(feature = "tray", feature = "tray-gtk"))]
    {
        backend::available()
    }
    #[cfg(not(any(feature = "tray", feature = "tray-gtk")))]
    {
        false
    }
}

/// The viewport commands that really show or hide the window, in order.
///
/// `ViewportCommand::Visible(false)` **alone is a no-op on Wayland** (a client
/// cannot unmap its own toplevel: winit has no protocol for it), so “hide to the
/// tray” that only sent it cancelled the close request and left the window on
/// screen — the user report “la croix ne ferme plus la fenêtre”.  `Minimized` is
/// supported by both backends, so the pair is what actually clears the screen;
/// restoring sends the mirror pair and then focuses, otherwise the window comes
/// back behind whatever has focus.
///
/// One list, used by the shell (`main::send_visible`): a second copy of this
/// rule is how the Wayland regression came back the first time.
pub fn visibility_commands(visible: bool) -> Vec<egui::ViewportCommand> {
    let mut commands = vec![
        egui::ViewportCommand::Visible(visible),
        egui::ViewportCommand::Minimized(!visible),
    ];
    if visible {
        commands.push(egui::ViewportCommand::Focus);
    }
    commands
}

/// Try to start the tray icon (no-op without the `tray` feature).
///
/// Returns once the outcome is known: never panics, never blocks beyond a short
/// handshake.  A failure (no StatusNotifier host, no session bus, headless) is
/// logged as a `warn` and [`available`] stays `false`, so closing still quits.
pub fn install() {
    #[cfg(any(feature = "tray", feature = "tray-gtk"))]
    backend::install();
    #[cfg(not(any(feature = "tray", feature = "tray-gtk")))]
    log::debug!("tray: not compiled (rebuild with --features tray for the tray icon)");
}

/// Drain pending tray events (no-op without the `tray` feature).  Called once
/// per UI frame, like the MIDI pump; it only reads `muda`'s channel, so it never
/// blocks.
pub fn poll() -> Vec<TrayEvent> {
    #[cfg(any(feature = "tray", feature = "tray-gtk"))]
    {
        backend::poll()
    }
    #[cfg(not(any(feature = "tray", feature = "tray-gtk")))]
    {
        Vec::new()
    }
}

/// Draw the tray icon as raw RGBA pixels — **generated in code, no file, no
/// decoder**.
///
/// The repo's PNGs live under the git-ignored `dist/` (produced by
/// `assets/icons/build_icons.sh`), so a fresh CI checkout has none: an
/// `include_bytes!("…/dist/icons/holaf-mix-32.png")` would make
/// `--features tray` fail there.  Drawing the glyph here keeps the tray
/// self-contained and spares the crate an `image` decode path it only needs for
/// a 32 px icon.
///
/// The drawing is a mixer: a rounded slate tile with three vertical faders and a
/// mint knob on each — readable at 22×22 on any panel, and identical on every
/// platform.  `size` is clamped to `8..=256` so a caller can never make the
/// buffer absurd; the returned dimensions are the ones to hand to
/// `tray_icon::Icon::from_rgba`.
///
/// Pure and deterministic: no globals, no I/O — hence unit-tested below, in
/// every feature configuration.
pub fn icon_rgba(size: u32) -> (Vec<u8>, u32, u32) {
    // The glyph is authored on a 32×32 grid and scaled by `scale`.
    const REF: f32 = 32.0;
    const BG: [u8; 4] = [0x1e, 0x2a, 0x3a, 0xff]; // slate tile
    const TRACK: [u8; 4] = [0x8a, 0x99, 0xad, 0xff]; // fader slot
    const KNOB: [u8; 4] = [0x36, 0xd3, 0x9b, 0xff]; // mint cap

    let n = size.clamp(8, 256);
    let scale = n as f32 / REF;
    let px = |v: f32| (v * scale).round() as u32;

    let mut rgba = vec![0u8; (n * n * 4) as usize];

    // 1. Rounded tile: a 1 px transparent margin, 2 px corner radius at 32 px.
    let margin = px(1.0).max(1);
    let radius = (2.0 * scale).max(1.0);
    for y in margin..n.saturating_sub(margin) {
        for x in margin..n.saturating_sub(margin) {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Distance to the rounded-rectangle's corner circle centres.
            let cx = fx.clamp(radius, n as f32 - radius);
            let cy = fy.clamp(radius, n as f32 - radius);
            let (dx, dy) = (fx - cx, fy - cy);
            if dx * dx + dy * dy <= radius * radius {
                set_px(&mut rgba, n, x, y, BG);
            }
        }
    }

    // 2. Three faders: a slot plus a cap at a different height (so the icon reads
    //    as a mixer, not as barcodes).  Positions are fractions of the grid, so
    //    the glyph survives any `size`.
    let slot_x = [px(9.0), px(16.0), px(23.0)];
    let knob_y = [px(19.0), px(11.0), px(23.0)];
    let slot_half = px(1.0).max(1);
    // The cap is 7×3 px on the 32 px grid (±3, ±1) and scales with it.
    let cap_half_w = ((3.0 * scale).round() as u32).max(1);
    let cap_half_h = (scale.round() as u32).max(1);
    let (slot_top, slot_bottom) = (px(6.0), px(26.0));
    for (cx, cy) in slot_x.iter().zip(knob_y) {
        // The slot, as a 3 px-wide column (1 px at the smallest size).
        for y in slot_top..slot_bottom {
            for x in cx.saturating_sub(slot_half)..=cx.saturating_add(slot_half) {
                set_px(&mut rgba, n, x, y, TRACK);
            }
        }
        // The cap, drawn on top so it is visible even where it overlaps.
        for y in cy.saturating_sub(cap_half_h)..=cy.saturating_add(cap_half_h) {
            for x in cx.saturating_sub(cap_half_w)..=cx.saturating_add(cap_half_w) {
                set_px(&mut rgba, n, x, y, KNOB);
            }
        }
    }

    (rgba, n, n)
}

/// One pixel, bounds-checked: a scaled glyph edge must never panic on an
/// off-grid coordinate.
fn set_px(rgba: &mut [u8], n: u32, x: u32, y: u32, color: [u8; 4]) {
    if x >= n || y >= n {
        return;
    }
    let i = ((y * n + x) * 4) as usize;
    rgba[i..i + 4].copy_from_slice(&color);
}

/// The real `tray-icon` implementation (compiled under `tray` = ksni/D-Bus, or
/// under `tray-gtk` = libappindicator).
///
/// # Threading
///
/// eframe owns the process's main thread, so the icon is built and **kept
/// alive** on a dedicated `holaf-tray` thread:
///
/// * with `tray` (ksni) that thread only has to hold the `TrayIcon` handle —
///   `ksni` runs its own D-Bus worker — so it parks;
/// * with `tray-gtk` (libappindicator) `gtk::init()` and a **running
///   `gtk::main()`** are required on that same thread.
///
/// The only cross-thread traffic is `muda`'s global `MenuEvent` channel (which
/// is `Send`), the [`HostProbe`] verdict and the `available` flag (an atomic).
/// Nothing here ever touches the UI thread.
#[cfg(any(feature = "tray", feature = "tray-gtk"))]
pub mod backend {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{entries, menu_action, HostProbe, TrayEvent};

    /// Whether the icon was created successfully (read by [`super::available`]).
    static AVAILABLE: AtomicBool = AtomicBool::new(false);

    /// Which backend this build links — logged once, and handy in a bug report.
    /// `ksni` wins when both features are on, matching `tray-icon`'s own `cfg`.
    pub const BACKEND: &str = if cfg!(feature = "tray") {
        "ksni (D-Bus StatusNotifierItem, no GTK)"
    } else {
        "libappindicator (GTK3)"
    };

    /// The StatusNotifier watcher's well-known name (SNI spec, and what
    /// `isSystemTrayAvailable()` uses): its owner *is* the host that draws the
    /// icon.
    const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";

    /// Did the icon get created?
    pub fn available() -> bool {
        AVAILABLE.load(Ordering::Relaxed)
    }

    /// Ask D-Bus whether a StatusNotifier **host** exists.
    ///
    /// Only compiled for the ksni backend: `tray-icon`'s ksni impl spawns with
    /// `assume_sni_available(true)`, so it would otherwise claim success with no
    /// host at all.  The libappindicator backend does its own availability
    /// check inside GTK, so this is not needed there.
    #[cfg(feature = "tray")]
    fn probe_host() -> HostProbe {
        // Step 1: a session bus at all.  Without one no host can exist (a
        // StatusNotifier host is itself a D-Bus client), and the distinction
        // matters for the log: "no bus" is a headless/CI situation, "no host"
        // is a desktop without a tray host.  Checking it separately also keeps
        // the classification independent of `zbus`'s error variants, which is
        // what a single `match` on the whole chain got wrong.
        let connection = match zbus::blocking::Connection::session() {
            Ok(connection) => connection,
            Err(err) => {
                log::debug!("tray: no D-Bus session bus ({err})");
                return HostProbe::from_lookup(Err(()));
            }
        };

        // Step 2: `NameHasOwner` on the standard `org.freedesktop.DBus`
        // interface — the cheapest question that answers "is a host there?"
        // (no activation, no introspection, no traffic to the host itself).
        let owned = (|| -> zbus::Result<bool> {
            let dbus = zbus::blocking::fdo::DBusProxy::new(&connection)?;
            let name = zbus::names::BusName::try_from(WATCHER_NAME)
                .map_err(|err| zbus::Error::Failure(err.to_string()))?;
            Ok(dbus.name_has_owner(name)?)
        })();
        match owned {
            Ok(owned) => HostProbe::from_lookup(Ok(owned)),
            Err(err) => {
                // The bus exists but would not answer: say nothing about the
                // host and let `tray-icon` try (see `HostProbe::Unknown`).
                log::debug!("tray: could not ask D-Bus for {WATCHER_NAME}: {err}");
                HostProbe::Unknown
            }
        }
    }

    /// The libappindicator backend needs no probe of its own: GTK reports it.
    #[cfg(all(feature = "tray-gtk", not(feature = "tray")))]
    fn probe_host() -> HostProbe {
        HostProbe::Unknown
    }

    /// Start the tray thread and wait (briefly) for its verdict.
    ///
    /// A missing StatusNotifier host, a missing session bus or a failing build
    /// is logged as a `warn`: `available()` then stays `false` and closing the
    /// window quits.
    pub fn install() {
        let probe = probe_host();
        if !probe.build_icon() {
            log::warn!(
                "tray: unavailable (no StatusNotifier host: {WATCHER_NAME} has no \
                 owner) - closing the window will quit, not hide. Hosts: KDE Plasma, \
                 waybar, swaybar's tray, gnome-shell-extension-appindicator"
            );
            return;
        }
        if probe.is_unknown() {
            log::debug!("tray: host presence unknown ({BACKEND}) - building the icon anyway");
        }

        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("holaf-tray".to_string())
            .spawn(move || run_tray(tx))
            .map(|_| ())
            .unwrap_or_else(|err| {
                log::warn!("tray: cannot spawn the tray thread: {err}");
            });

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                AVAILABLE.store(true, Ordering::Relaxed);
                log::info!(
                    "tray: icon created with {BACKEND} ({} menu entries)",
                    entries().len()
                );
            }
            Ok(Err(err)) => {
                log::warn!("tray: unavailable ({err}) - closing the window will quit, not hide");
            }
            Err(_) => {
                log::warn!("tray: the tray thread did not answer in 5 s - assuming unavailable");
            }
        }
    }

    /// Drain `muda`'s menu events into [`TrayEvent`]s.
    ///
    /// `MenuEvent::receiver` is a global, `Send` channel filled by the backend
    /// (the ksni D-Bus worker or the GTK thread); reading it here (UI thread) is
    /// the supported pattern.  Unknown ids are dropped.
    pub fn poll() -> Vec<TrayEvent> {
        use tray_icon::menu::MenuEvent;
        let mut events = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match menu_action(event.id().0.as_str()) {
                Some(action) => events.push(action),
                None => log::debug!("tray: ignored menu event {:?}", event.id().0),
            }
        }
        events
    }

    /// The tray thread body: build the icon, report its verdict once, then serve
    /// it until the process exits.
    fn run_tray(tx: mpsc::Sender<Result<(), String>>) {
        // 1. libappindicator needs a GTK context on *this* thread, and it must be
        //    the same thread that runs the main loop.  The ksni backend needs
        //    neither (no GTK in the build at all).
        #[cfg(all(feature = "tray-gtk", not(feature = "tray")))]
        if let Err(err) = gtk::init() {
            let _ = tx.send(Err(format!("gtk::init failed: {err}")));
            return;
        }

        // 2. Build the icon + menu.  Any failure is reported, not panicked.
        let tray = match build_tray() {
            Ok(tray) => {
                let _ = tx.send(Ok(()));
                tray
            }
            Err(err) => {
                let _ = tx.send(Err(err));
                return;
            }
        };

        // 3. Keep the icon alive for the whole run: dropping `tray` unregisters
        //    the StatusNotifier item (and, with libappindicator, destroys the
        //    AppIndicator).  With GTK we must also pump the loop.
        let _keep_alive = tray;
        #[cfg(feature = "tray")]
        loop {
            // Nothing to run: ksni's D-Bus worker is its own thread.  `park`
            // can return spuriously, hence the loop.
            std::thread::park();
        }
        #[cfg(all(feature = "tray-gtk", not(feature = "tray")))]
        gtk::main();
    }

    /// Create the tray icon with its menu ([`super::menu_layout`]).
    fn build_tray() -> Result<tray_icon::TrayIcon, String> {
        use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
        use tray_icon::TrayIconBuilder;

        let icon = load_icon()?;

        let menu = Menu::new();
        let mut items: Vec<Box<dyn tray_icon::menu::IsMenuItem>> = Vec::new();
        for entry in super::menu_layout() {
            match entry {
                Some(entry) => {
                    let item = MenuItem::with_id(entry.id(), entry.label(), true, None);
                    items.push(Box::new(item));
                }
                None => items.push(Box::new(PredefinedMenuItem::separator())),
            }
        }
        let refs: Vec<&dyn tray_icon::menu::IsMenuItem> =
            items.iter().map(|item| item.as_ref()).collect();
        menu.append_items(&refs)
            .map_err(|err| format!("menu append failed: {err}"))?;

        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Holaf-Mix")
            .with_icon(icon)
            // Linux delivers no click events through this backend, so a left
            // click opens the menu with Show/Hide (the only reliable
            // interaction there).
            .with_menu_on_left_click(true)
            .build()
            .map_err(|err| format!("TrayIconBuilder::build failed: {err}"))
    }

    /// Wrap [`super::icon_rgba`] into the icon type `tray-icon` expects.
    ///
    /// 32 px is the size StatusNotifier hosts ask for; the host scales it for
    /// HiDPI itself.
    fn load_icon() -> Result<tray_icon::Icon, String> {
        let (rgba, width, height) = super::icon_rgba(32);
        tray_icon::Icon::from_rgba(rgba, width, height)
            .map_err(|err| format!("icon from_rgba failed: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tray now carries **everything** the window's right-click menu used to
    /// carry (that menu is gone — user request).  The order is: the window
    /// toggle alone, then the window menu's content, then `Quit` apart.
    #[test]
    fn the_tray_menu_carries_the_whole_window_menu() {
        assert_eq!(
            entries()
                .iter()
                .map(|entry| entry.label())
                .collect::<Vec<_>>(),
            [
                "Show/Hide",
                "MIDI port...",
                "Restart audio engine",
                "Debug window",
                "Open project page",
                "Quit",
            ]
        );
        // The separators are at both ends of the block that moved here: one
        // under `Show/Hide`, one above `Quit`.
        let layout = menu_layout();
        assert_eq!(layout[1], None);
        assert_eq!(layout[layout.len() - 2], None);
        // Nothing from the window menu was lost in the move.
        for label in crate::menus::labels(&crate::menus::ContextMenuKind::Window)
            .into_iter()
            .filter(|label| *label != "---")
        {
            assert!(
                entries().iter().any(|entry| entry.label() == label),
                "{label} is in no menu at all"
            );
        }
    }

    #[test]
    fn closing_never_hides_the_window_without_a_tray() {
        // No tray ⇒ closing must quit, otherwise the app becomes unreachable.
        assert_eq!(close_action(false, true), CloseAction::Quit);
        assert_eq!(close_action(false, false), CloseAction::Quit);
        // A tray + the preference ⇒ hide (Qt behaviour).
        assert_eq!(close_action(true, true), CloseAction::Hide);
        // A tray but the preference off ⇒ quit.
        assert_eq!(close_action(true, false), CloseAction::Quit);
    }

    /// The tray's `Quit` must quit, even when the window's close button only
    /// hides (the bug: the entry sent `ViewportCommand::Close`, the close request
    /// came back, and the hide rule swallowed it — the app stayed in the tray).
    #[test]
    fn an_explicit_quit_wins_over_the_hide_on_close_rule() {
        // A tray is available and the preference says "hide on close"…
        assert_eq!(close_action(true, true), CloseAction::Hide);
        // …but an explicit Quit still quits.
        assert_eq!(close_action_with(true, true, true), CloseAction::Quit);
        // A plain close (no explicit quit) keeps the Qt rule.
        assert_eq!(close_action_with(false, true, true), CloseAction::Hide);
        assert_eq!(close_action_with(false, true, false), CloseAction::Quit);
        // No tray: both quit.
        assert_eq!(close_action_with(false, false, true), CloseAction::Quit);
        assert_eq!(close_action_with(true, false, true), CloseAction::Quit);
    }

    #[test]
    fn the_menu_ids_and_actions_round_trip() {
        // Every entry has a stable, prefixed id, and the id maps back to the
        // same entry and to the action the shell performs.
        for entry in entries() {
            assert!(entry.id().starts_with("holaf.tray."), "{}", entry.id());
            assert_eq!(entry_for_id(entry.id()), Some(entry));
            assert_eq!(menu_action(entry.id()), Some(entry.event()));
        }
        assert_eq!(
            menu_action(entries()[0].id()),
            Some(TrayEvent::ToggleWindow)
        );
        assert_eq!(
            menu_action(entries()[1].id()),
            Some(TrayEvent::MidiPortPicker)
        );
        assert_eq!(
            menu_action(entries()[2].id()),
            Some(TrayEvent::RestartEngine)
        );
        assert_eq!(
            menu_action(entries()[3].id()),
            Some(TrayEvent::OpenDiagnostics)
        );
        assert_eq!(
            menu_action(entries()[4].id()),
            Some(TrayEvent::OpenProjectPage)
        );
        assert_eq!(menu_action(entries()[5].id()), Some(TrayEvent::Quit));
    }

    #[test]
    fn the_ids_are_unique_and_unknown_ids_are_ignored() {
        let ids: Vec<&str> = entries().iter().map(|entry| entry.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ids must be unique: {ids:?}");
        // A menu id we did not create (another app's tray, a future item) is
        // dropped, never mishandled.
        assert_eq!(entry_for_id("some.other.menu"), None);
        assert_eq!(menu_action("some.other.menu"), None);
        assert_eq!(menu_action(""), None);
    }

    #[test]
    fn without_the_feature_there_is_no_tray_and_polling_is_empty() {
        // The policy must hold in both builds: no tray ⇒ close quits, and the
        // event pump is harmless.  With `--features tray` but no host/session
        // bus, `install()` fails gracefully and `available()` stays false too;
        // either way nothing here panics and `poll()` never blocks.
        #[cfg(not(any(feature = "tray", feature = "tray-gtk")))]
        {
            assert!(!available());
            assert!(poll().is_empty());
        }
        // The invariant: whenever the tray is deemed unavailable, closing quits
        // (the app must never become unreachable).
        if !available() {
            assert_eq!(close_action(available(), true), CloseAction::Quit);
        }
    }

    /// A tray is only claimed when a **host** can draw it.
    ///
    /// `tray-icon`'s ksni backend spawns with `assume_sni_available(true)`, so
    /// "build returned Ok" does not mean "an icon is visible".  Without this
    /// rule the app would hide its window on close while no icon exists — the
    /// unreachable-app bug the Qt `isSystemTrayAvailable()` check prevents.
    #[test]
    fn only_a_definite_absence_of_a_host_skips_the_icon() {
        // The watcher name has an owner ⇒ build, no second-guessing.
        assert_eq!(HostProbe::from_lookup(Ok(true)), HostProbe::Present);
        assert!(HostProbe::Present.build_icon());
        assert!(!HostProbe::Present.is_unknown());
        // No owner ⇒ no host ⇒ no icon (and closing will quit).
        assert_eq!(HostProbe::from_lookup(Ok(false)), HostProbe::Absent);
        assert!(!HostProbe::Absent.build_icon());
        // No session bus at all: no host can exist.  Conservative on purpose.
        assert_eq!(HostProbe::from_lookup(Err(())), HostProbe::Absent);
        assert!(!HostProbe::from_lookup(Err(())).build_icon());
        // An inconclusive probe still builds: only a definite "no host" skips.
        assert!(HostProbe::Unknown.build_icon());
        assert!(HostProbe::Unknown.is_unknown());
    }

    #[test]
    fn visibility_is_a_single_viewport_command() {
        // Showing: un-minimize, un-hide, then take the focus back.
        let shown = visibility_commands(true);
        assert!(matches!(
            shown[0],
            egui::ViewportCommand::Visible(true)
        ));
        assert!(matches!(
            shown[1],
            egui::ViewportCommand::Minimized(false)
        ));
        assert!(matches!(shown[2], egui::ViewportCommand::Focus));
        // Hiding: `Visible(false)` **and** `Minimized(true)` — the first is a
        // no-op on Wayland, the second is what actually clears the screen.
        let hidden = visibility_commands(false);
        assert_eq!(hidden.len(), 2, "nothing to focus when hiding");
        assert!(matches!(
            hidden[0],
            egui::ViewportCommand::Visible(false)
        ));
        assert!(matches!(
            hidden[1],
            egui::ViewportCommand::Minimized(true)
        ));
    }

    /// The pixel at `(x, y)` in an [`icon_rgba`] buffer.
    fn pixel(rgba: &[u8], n: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * n + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn the_icon_is_generated_not_loaded_from_a_file() {
        // The whole point of the generator: the tray must not depend on a PNG in
        // the git-ignored `dist/` (a fresh CI checkout has none).  So the buffer
        // is produced here, with the dimensions the caller asks for.
        let (rgba, w, h) = icon_rgba(32);
        assert_eq!((w, h), (32, 32));
        assert_eq!(rgba.len(), 32 * 32 * 4, "4 bytes per pixel, no padding");
        // Opaque where the tile is drawn, fully transparent outside it.
        assert_eq!(pixel(&rgba, 32, 0, 0)[3], 0, "1 px transparent margin");
        assert_eq!(pixel(&rgba, 32, 31, 31)[3], 0);
        assert_eq!(pixel(&rgba, 32, 3, 3)[3], 255, "rounded tile is opaque");
        // Deterministic: the same size always yields the same pixels.
        assert_eq!(rgba, icon_rgba(32).0);
    }

    #[test]
    fn the_icon_reads_as_a_mixer() {
        // Three fader slots (a light column) with a mint cap on each: the glyph
        // must actually be drawn, not just a plain tile.
        let (rgba, n, _) = icon_rgba(32);
        let tile = pixel(&rgba, n, 3, 3);
        let slot = pixel(&rgba, n, 16, 8);
        let cap = pixel(&rgba, n, 16, 11);
        assert_ne!(slot, tile, "the fader slot differs from the background");
        assert_ne!(cap, slot, "the cap differs from its slot");
        assert_eq!(cap[3], 255);
        // The cap is centred on the slot (mid fader, its lowest knob height).
        assert_eq!(pixel(&rgba, n, 16, 11), pixel(&rgba, n, 15, 11));
        // All three columns carry a cap, at *different* heights.
        for x in [9u32, 16, 23] {
            let column_has_cap = (0..32).any(|y| pixel(&rgba, n, x, y) == cap);
            assert!(column_has_cap, "no cap on the fader at x={x}");
        }
    }

    #[test]
    fn the_icon_scales_to_any_size_and_never_panics() {
        // Hosts ask for 16…64 px; the clamp keeps a silly size from allocating
        // an absurd buffer, and every in-between size must stay well-formed.
        for size in [0u32, 1, 7, 8, 16, 22, 24, 32, 48, 64, 128, 256, 1024] {
            let (rgba, w, h) = icon_rgba(size);
            assert_eq!(w, h, "the icon is square (size {size})");
            assert!((8..=256).contains(&w), "size {size} clamped to 8..=256");
            assert_eq!(rgba.len(), (w * w * 4) as usize);
            assert!(rgba.chunks(4).any(|px| px[3] != 0), "blank icon at {size}");
        }
    }

    /// With the feature on but **no display** (the CI/headless case, and a bare
    /// Wayland session), `install()` must fail *gracefully*: a warning, no panic,
    /// and `available()` still `false` so closing quits.
    #[cfg(any(feature = "tray", feature = "tray-gtk"))]
    #[test]
    fn install_without_a_display_degrades_to_no_tray_instead_of_panicking() {
        // Only meaningful where there is genuinely no display server; skip on a
        // desktop where a tray host might exist (that path needs a live session).
        if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
        {
            eprintln!("SKIP: a display is present, cannot assert the headless fallback");
            return;
        }
        // Must not panic, and must report "no tray" so `close_action` quits.
        install();
        // A headless run has no session bus (or, with one, no StatusNotifier
        // host): either way there is nothing to draw an icon with.
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            assert!(
                !available(),
                "with no display and no session bus the tray cannot be created"
            );
            assert_eq!(close_action(available(), true), CloseAction::Quit);
            // And the pump is a harmless no-op.
            assert!(poll().is_empty());
        }
    }
}
