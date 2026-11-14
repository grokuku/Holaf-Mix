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
//! # What is implemented in Rust, and what is not
//!
//! * **always on top: done.** `egui::ViewportCommand::WindowLevel(
//!   WindowLevel::AlwaysOnTop)` is sent by [`crate::app::render`] whenever
//!   [`crate::app::AppState::always_on_top`] changes; the flag is stored under
//!   `ui_rust.always_on_top` (a Rust-only key, see [`crate::config`]) and
//!   defaults to `true`, like Qt.
//! * **tray: not compiled.** The cross-platform crate is `tray-icon`, and on
//!   Linux it links **GTK3 + libappindicator**; this project's container has no
//!   GTK3 at all (`pkg-config gtk+-3.0` fails), so the dependency cannot even be
//!   built here — and a tray that is claimed but absent is worse than one that
//!   is honestly missing.  [`available`] therefore returns `false`, and the
//!   *policy* (which entries the menu has, what closing does) is pure and tested
//!   so wiring the real icon later is a drop-in:
//!
//!   ```text
//!   TODO(tray): add `tray-icon = "0.19"` behind a `tray` cargo feature,
//!   build it in `main.rs` (`TrayIconBuilder` + `Menu`), forward the events
//!   through a channel and call `tray::menu_action(…)` on each of them; then
//!   flip `close_action()`'s first argument to `tray::available()`.
//!   Wayland caveat: the icon only appears with an appindicator host
//!   (`gnome-shell-extension-appindicator`, `kstatusnotifieritem`,
//!   `swaybar`'s `tray`); on a bare Wayland session without one there is no
//!   tray, which is exactly why closing must NOT hide the window by default.
//!   ```

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

/// Is a tray icon available in this build?
///
/// Always `false` today (see the module docs: `tray-icon` needs GTK3, absent
/// here).  Kept as a function so the call sites never have to change.
pub fn available() -> bool {
    false
}

/// The command line the tray would use to re-show the window.  egui hides a
/// window with `ViewportCommand::Visible`; this documents the pair.
pub fn show_commands(visible: bool) -> [egui::ViewportCommand; 1] {
    [egui::ViewportCommand::Visible(visible)]
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
    fn this_build_has_no_tray() {
        // Documented limitation: `tray-icon` needs GTK3/libappindicator, absent
        // in the build environment.  When it is wired, this test becomes the
        // assertion that flips.
        assert!(!available());
        assert_eq!(close_action(available(), true), CloseAction::Quit);
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
}
