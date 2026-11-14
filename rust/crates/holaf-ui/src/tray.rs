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
//!   `cargo build -p holaf-ui` / `cargo test --workspace` stays hermetic.  The
//!   cross-platform crate is `tray-icon`, and on Linux it links **GTK3 +
//!   libappindicator** (Arch: `gtk3` + `libayatana-appindicator` — Arch has no
//!   `libappindicator-gtk3`, that name is AUR-only; Debian: `libgtk-3-dev` +
//!   `libayatana-appindicator3-dev`).  When the libraries are missing the build
//!   script retries without the feature: [`available`] returns `false` and
//!   closing the window quits (see [`close_action`]).
//!
//!   `tray-icon` is pulled with `default-features = false`, which drops its
//!   `libxdo` feature (only the predefined Copy/Paste items use it) — so no
//!   `libxdo` build dep and no runtime `dlopen` of it.
//!
//!   The *policy* (which entries the menu holds, what closing does), the
//!   **icon itself** ([`icon_rgba`], drawn pixel by pixel) and the event mapping
//!   (a menu id to a [`TrayEvent`]) are all pure and unit-tested here — in every
//!   feature configuration.  Only the GTK/libappindicator I/O lives in
//!   [`gtk_impl`], so wiring is one `poll` per frame and never touches GTK from
//!   the UI thread.
//!
//! # Wayland caveat
//!
//! A tray icon needs an appindicator/StatusNotifier **host**:
//! `gnome-shell-extension-appindicator`, KDE's `kstatusnotifieritem`, or
//! `swaybar`'s `tray`.  On a bare Wayland session without one, libappindicator
//! still "succeeds" but nothing is drawn — and on GNOME/Wayland `TrayIconEvent`
//! clicks are not delivered at all (X11-only in `tray-icon`).  That is exactly
//! why the reliable interaction is the **menu** (Show/Hide, not a
//! toggle-on-click), why `with_menu_on_left_click(true)` opens that menu on a
//! left click, and why closing must NOT hide the window unless a tray is
//! genuinely there.

/// The tray menu entries, in the Qt order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEntry {
    /// `Show/Hide` — toggles the window (`_toggle_window`).
    ShowHide,
    /// `Restart audio engine` — the same action as the context menu.
    RestartEngine,
    /// `Quit` — `_force_quit`: stop the engine, then leave.
    Quit,
}

impl TrayEntry {
    /// The label the tray shows (Python `QAction` texts).
    pub fn label(self) -> &'static str {
        match self {
            TrayEntry::ShowHide => "Show/Hide",
            TrayEntry::RestartEngine => "Restart audio engine",
            TrayEntry::Quit => "Quit",
        }
    }
}

/// The tray menu (`_init_tray_icon`): `Show/Hide`, `Quit` in Qt, plus the
/// restart entry the context menu also offers (so the tray alone is enough to
/// recover from a dead engine without restoring the window).
pub fn entries() -> [TrayEntry; 3] {
    [
        TrayEntry::ShowHide,
        TrayEntry::RestartEngine,
        TrayEntry::Quit,
    ]
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

/// One thing the tray can ask the app to do.  The UI shell turns each into the
/// matching [`crate::app::AppState`] call / viewport command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// The window is hidden → show it; visible → hide it.
    ToggleWindow,
    /// Restart the audio engine (`AppState::restart_engine`).
    RestartEngine,
    /// Stop the engine and leave (`AppState::shutdown` + close the viewport).
    Quit,
}

impl TrayEntry {
    /// The stable menu id, used to recognise the click in `muda`'s event stream.
    /// Kept short and prefixed so it cannot collide with another item.
    pub fn id(self) -> &'static str {
        match self {
            TrayEntry::ShowHide => "holaf.tray.show_hide",
            TrayEntry::RestartEngine => "holaf.tray.restart_engine",
            TrayEntry::Quit => "holaf.tray.quit",
        }
    }

    /// What activating this entry does to the app.
    pub fn event(self) -> TrayEvent {
        match self {
            TrayEntry::ShowHide => TrayEvent::ToggleWindow,
            TrayEntry::RestartEngine => TrayEvent::RestartEngine,
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
/// This is the whole "GTK → AppState" adapter: the shell polls `muda`'s event
/// receiver each frame, looks the id up here, and never has to know about menu
/// widgets.  An unknown id (another application's tray, or a future item) is
/// ignored instead of mishandled.
pub fn menu_action(id: &str) -> Option<TrayEvent> {
    entry_for_id(id).map(TrayEntry::event)
}

/// Is a tray icon available in this build (and did it initialise)?
///
/// `false` unless the `tray` feature is compiled **and** the icon was actually
/// created at startup (see [`gtk_impl::install`]).  Call sites never change.
pub fn available() -> bool {
    #[cfg(feature = "tray")]
    {
        gtk_impl::available()
    }
    #[cfg(not(feature = "tray"))]
    {
        false
    }
}

/// The command line the tray would use to re-show the window.  egui hides a
/// window with `ViewportCommand::Visible`; this documents the pair.
pub fn show_commands(visible: bool) -> [egui::ViewportCommand; 1] {
    [egui::ViewportCommand::Visible(visible)]
}

/// Try to start the tray icon (no-op without the `tray` feature).
///
/// Returns once the outcome is known: never panics, never blocks beyond a short
/// handshake.  A failure (no GTK, no libappindicator, no display, headless) is
/// logged as a `warn` and [`available`] stays `false`, so closing still quits.
pub fn install() {
    #[cfg(feature = "tray")]
    gtk_impl::install();
    #[cfg(not(feature = "tray"))]
    log::debug!("tray: not compiled (rebuild with --features tray for the tray icon)");
}

/// Drain pending tray events (no-op without the `tray` feature).  Called once
/// per UI frame, like the MIDI pump; it only reads `muda`'s channel, so it never
/// touches GTK from the UI thread.
pub fn poll() -> Vec<TrayEvent> {
    #[cfg(feature = "tray")]
    {
        gtk_impl::poll()
    }
    #[cfg(not(feature = "tray"))]
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

/// The real `tray-icon` + GTK implementation (compiled only under `--features
/// tray`).
///
/// On Linux `tray-icon` is libappindicator, which needs a **running GTK main
/// loop on its own thread** — eframe owns the process's main thread, so the icon
/// is built and kept alive on a dedicated thread that calls `gtk::main()`.  The
/// only cross-thread traffic is `muda`'s global `MenuEvent` channel, which is
/// `Send`, and the `available` flag (an atomic).
#[cfg(feature = "tray")]
pub mod gtk_impl {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{entries, menu_action, TrayEvent};

    /// Whether the icon was created successfully (read by [`super::available`]).
    static AVAILABLE: AtomicBool = AtomicBool::new(false);

    /// Did the icon get created?
    pub fn available() -> bool {
        AVAILABLE.load(Ordering::Relaxed)
    }

    /// Start the tray thread and wait (briefly) for its verdict.
    ///
    /// The thread inits GTK, builds the icon + menu, reports success/failure on
    /// a channel, then parks the thread in `gtk::main()`.  A missing GTK or
    /// libappindicator makes `gtk::init`/`build` fail: the error is returned to
    /// the caller, logged as a warning there, and `available()` stays `false`.
    pub fn install() {
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
                log::info!("tray: icon created ({} menu entries)", entries().len());
            }
            Ok(Err(err)) => {
                log::warn!(
                    "tray: unavailable ({err}) - closing the window will quit, not hide"
                );
            }
            Err(_) => {
                log::warn!(
                    "tray: the tray thread did not answer in 5 s - assuming unavailable"
                );
            }
        }
    }

    /// Drain `muda`'s menu events into [`TrayEvent`]s.
    ///
    /// `MenuEvent::receiver` is a global, `Send` channel filled by the GTK
    /// thread; reading it here (UI thread) is the supported pattern.  Unknown ids
    /// are dropped.
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

    /// The GTK thread body.  Reports its verdict once, then serves the icon.
    fn run_tray(tx: mpsc::Sender<Result<(), String>>) {
        // 1. A GTK context on *this* thread — libappindicator requires it, and it
        //    must be the same thread that runs the main loop.
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

        // 3. Keep the icon alive and the loop running.  `gtk::main` blocks until
        //    the process ends; holding `tray` in this scope keeps the handle
        //    (and its AppIndicator) alive for the whole run.
        let _keep_alive = tray;
        gtk::main();
    }

    /// Create the tray icon with the Qt menu (Show/Hide, Restart, Quit).
    fn build_tray() -> Result<tray_icon::TrayIcon, String> {
        use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
        use tray_icon::TrayIconBuilder;

        let icon = load_icon()?;

        let menu = Menu::new();
        let mut items: Vec<Box<dyn tray_icon::menu::IsMenuItem>> = Vec::new();
        for entry in entries() {
            // A separator above Quit keeps "quit" visually apart from the
            // toggles, like the Qt menu.
            if entry == super::TrayEntry::Quit {
                items.push(Box::new(PredefinedMenuItem::separator()));
            }
            let item = MenuItem::with_id(entry.id(), entry.label(), true, None);
            items.push(Box::new(item));
        }
        let refs: Vec<&dyn tray_icon::menu::IsMenuItem> =
            items.iter().map(|item| item.as_ref()).collect();
        menu.append_items(&refs)
            .map_err(|err| format!("menu append failed: {err}"))?;

        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Holaf-Mix")
            .with_icon(icon)
            // Linux delivers no click events, so a left click opens the menu
            // with Show/Hide (the only reliable interaction there).
            .with_menu_on_left_click(true)
            .build()
            .map_err(|err| format!("TrayIconBuilder::build failed: {err}"))
    }

    /// Wrap [`super::icon_rgba`] into the icon type `tray-icon` expects.
    ///
    /// 32 px is the size AppIndicator/StatusNotifier hosts ask for; the host
    /// scales it for HiDPI itself.
    fn load_icon() -> Result<tray_icon::Icon, String> {
        let (rgba, width, height) = super::icon_rgba(32);
        tray_icon::Icon::from_rgba(rgba, width, height)
            .map_err(|err| format!("icon from_rgba failed: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tray_menu_matches_the_qt_one() {
        assert_eq!(
            entries().map(TrayEntry::label),
            ["Show/Hide", "Restart audio engine", "Quit"]
        );
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

    #[test]
    fn the_menu_ids_and_actions_round_trip() {
        // Every entry has a stable, prefixed id, and the id maps back to the
        // same entry and to the action the shell performs.
        for entry in entries() {
            assert!(entry.id().starts_with("holaf.tray."), "{}", entry.id());
            assert_eq!(entry_for_id(entry.id()), Some(entry));
            assert_eq!(menu_action(entry.id()), Some(entry.event()));
        }
        assert_eq!(menu_action(entries()[0].id()), Some(TrayEvent::ToggleWindow));
        assert_eq!(menu_action(entries()[1].id()), Some(TrayEvent::RestartEngine));
        assert_eq!(menu_action(entries()[2].id()), Some(TrayEvent::Quit));
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
        // event pump is harmless.  With `--features tray` but no display/GTK
        // host, `install()` fails gracefully and `available()` stays false too;
        // either way nothing here panics and `poll()` never blocks.
        #[cfg(not(feature = "tray"))]
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

    #[test]
    fn visibility_is_a_single_viewport_command() {
        assert!(matches!(
            show_commands(true)[0],
            egui::ViewportCommand::Visible(true)
        ));
        assert!(matches!(
            show_commands(false)[0],
            egui::ViewportCommand::Visible(false)
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
    #[cfg(feature = "tray")]
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
        assert!(
            !available(),
            "with no display the tray cannot be created"
        );
        assert_eq!(close_action(available(), true), CloseAction::Quit);
        // And the pump is a harmless no-op.
        assert!(poll().is_empty());
    }
}
