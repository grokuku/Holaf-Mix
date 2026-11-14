//! Context menus — the Rust twin of `MainWindow.build_app_context_menu` and of
//! the per-widget menus of the Qt build.
//!
//! The Qt UI had three kinds of menu, and they are reproduced with the same
//! entries in the same order (the Python `*_MENU_*` constants and the tests in
//! `tests/ui/test_context_menu.py` lock that order):
//!
//! | place | entries |
//! |---|---|
//! | the window (background, VU meters, empty areas) | `Restart audio engine`, `Debug window`, — , `Open project page` |
//! | a strip (label click) | `Rename`, `Delete`, — , then the three application entries |
//! | an FX button | `Effect settings`, — , then the three application entries |
//!
//! # Why the window menu is not a widget
//!
//! egui's hit test gives a click to the **last** widget registered under the
//! pointer, so `main_window::render` registers one "background" interaction for
//! the whole window *before* anything else is drawn: a click on a strip button is
//! taken by that button, and a click on a strip body, a VU meter or the empty
//! space falls through to the window menu.  That is exactly the Qt rule
//! (`contextMenuEvent` + an event filter for widgets whose policy is
//! `Qt.DefaultContextMenu`), without an event filter per widget.
//!
//! The menu itself is drawn by [`draw`] as a floating `egui::Area`; every entry
//! is a plain button, so the drawing is deterministic and a headless test can
//! assert on the produced entries without a window.

use egui::{Ui, Vec2};

use holaf_core::model::EffectKey;

use crate::app::{AppState, UiAction};
use crate::midi::MidiTarget;

/// Project home page opened by "Open project page" (`PROJECT_PAGE_URL` in
/// Python).
pub const PROJECT_PAGE_URL: &str = "https://github.com/grokuku/Holaf-Mix";

/// The three entries every menu carries (Python `MENU_RESTART_ENGINE`, …).
pub const MENU_RESTART_ENGINE: &str = "Restart audio engine";
pub const MENU_DEBUG_WINDOW: &str = "Debug window";
pub const MENU_OPEN_PROJECT_PAGE: &str = "Open project page";

/// The local entries of the strip menu.
pub const MENU_RENAME: &str = "Rename";
pub const MENU_DELETE: &str = "Delete";
/// The local entry of the FX button menu.
pub const MENU_FX_SETTINGS: &str = "Effect settings";

/// The entries of the strip's **MIDI** menu — the labels of the Qt
/// `_show_midi_menu` (`Learn Volume` / `Learn Mute` / `Learn Mono`, then
/// `Clear Mappings`), which is exactly what a MIDI Mix owner needs: one CC per
/// function of one strip, chosen by playing the control.
pub const MENU_LEARN_VOLUME: &str = "Learn Volume";
pub const MENU_LEARN_MUTE: &str = "Learn Mute";
pub const MENU_LEARN_MONO: &str = "Learn Mono";
pub const MENU_CANCEL_LEARN: &str = "Cancel learn";
pub const MENU_CLEAR_MAPPINGS: &str = "Clear Mappings";
/// The window menu's port entry.
pub const MENU_MIDI_PORT: &str = "MIDI port...";

/// Which menu is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextMenuKind {
    /// Right click on the window background (and on every widget without its
    /// own menu).
    Window,
    /// Right click on a strip.
    Strip { uid: String, label: String },
    /// Right click on one of the strip's FX buttons.
    Fx { uid: String, effect: EffectKey },
    /// The **MIDI** button of a strip (`_show_midi_menu`).  `armed` is the
    /// property a learn is waiting for, if any: the menu then offers to cancel
    /// it (Qt had no way back other than playing a control by accident).
    Midi {
        uid: String,
        label: String,
        armed: Option<MidiTarget>,
    },
}

impl ContextMenuKind {
    /// A stable id so egui keeps the menu's state across frames.
    fn id(&self) -> egui::Id {
        match self {
            ContextMenuKind::Window => egui::Id::unique("holaf-context-menu-window"),
            ContextMenuKind::Strip { uid, .. } => {
                egui::Id::unique(("holaf-context-menu-strip", uid.as_str()))
            }
            ContextMenuKind::Fx { uid, effect } => {
                egui::Id::unique(("holaf-context-menu-fx", uid.as_str(), effect.as_str()))
            }
            ContextMenuKind::Midi { uid, .. } => {
                egui::Id::unique(("holaf-context-menu-midi", uid.as_str()))
            }
        }
    }
}

/// One entry of a menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    RestartEngine,
    OpenDiagnostics,
    Rename,
    Delete,
    FxSettings,
    OpenProjectPage,
    /// `MIDI port...` (window menu).
    MidiPort,
    LearnVolume,
    LearnMute,
    LearnMono,
    CancelLearn,
    ClearMappings,
}

impl MenuItem {
    /// The label the user sees.
    pub fn label(self) -> &'static str {
        match self {
            MenuItem::RestartEngine => MENU_RESTART_ENGINE,
            MenuItem::OpenDiagnostics => MENU_DEBUG_WINDOW,
            MenuItem::Rename => MENU_RENAME,
            MenuItem::Delete => MENU_DELETE,
            MenuItem::FxSettings => MENU_FX_SETTINGS,
            MenuItem::OpenProjectPage => MENU_OPEN_PROJECT_PAGE,
            MenuItem::MidiPort => MENU_MIDI_PORT,
            MenuItem::LearnVolume => MENU_LEARN_VOLUME,
            MenuItem::LearnMute => MENU_LEARN_MUTE,
            MenuItem::LearnMono => MENU_LEARN_MONO,
            MenuItem::CancelLearn => MENU_CANCEL_LEARN,
            MenuItem::ClearMappings => MENU_CLEAR_MAPPINGS,
        }
    }

    /// The learn target this entry arms, if it is one.
    pub fn learn_target(self) -> Option<MidiTarget> {
        match self {
            MenuItem::LearnVolume => Some(MidiTarget::Volume),
            MenuItem::LearnMute => Some(MidiTarget::Mute),
            MenuItem::LearnMono => Some(MidiTarget::Mono),
            _ => None,
        }
    }

    /// Does activating this entry need a live engine?  (Used by [`draw`] to grey
    /// the entry out instead of sending a command nobody can serve.)
    pub fn needs_engine(self) -> bool {
        matches!(self, MenuItem::RestartEngine)
    }
}

/// One line of a menu: an entry, or a separator (`item == None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub item: Option<MenuItem>,
}

impl Entry {
    fn separator() -> Self {
        Self { item: None }
    }

    fn item(item: MenuItem) -> Self {
        Self { item: Some(item) }
    }

    pub fn is_separator(&self) -> bool {
        self.item.is_none()
    }
}

/// The application entries (`Restart`, `Debug window`, — , `Open project page`),
/// in the Python order.
pub fn application_entries() -> Vec<Entry> {
    vec![
        Entry::item(MenuItem::RestartEngine),
        Entry::item(MenuItem::OpenDiagnostics),
        Entry::separator(),
        Entry::item(MenuItem::OpenProjectPage),
    ]
}

/// Every entry of `kind`, in order.
///
/// The strip and FX menus append the application entries **after a separator**,
/// which is what `build_app_context_menu(menu)` did when a child widget passed
/// its own `QMenu`.  The MIDI menu follows the Qt `_show_midi_menu` order —
/// `Learn Volume`, `Learn Mute`, `Learn Mono`, — , `Clear Mappings` — and only
/// adds `Cancel learn` (first) when a learn is actually armed.
pub fn entries(kind: &ContextMenuKind) -> Vec<Entry> {
    match kind {
        ContextMenuKind::Window => {
            let mut entries = vec![Entry::item(MenuItem::MidiPort), Entry::separator()];
            entries.extend(application_entries());
            entries
        }
        ContextMenuKind::Strip { .. } => {
            let mut entries = vec![Entry::item(MenuItem::Rename), Entry::item(MenuItem::Delete)];
            entries.push(Entry::separator());
            entries.extend(application_entries());
            entries
        }
        ContextMenuKind::Fx { .. } => {
            let mut entries = vec![Entry::item(MenuItem::FxSettings), Entry::separator()];
            entries.extend(application_entries());
            entries
        }
        ContextMenuKind::Midi { armed, .. } => {
            let mut entries = Vec::new();
            if armed.is_some() {
                entries.push(Entry::item(MenuItem::CancelLearn));
                entries.push(Entry::separator());
            }
            entries.extend([
                Entry::item(MenuItem::LearnVolume),
                Entry::item(MenuItem::LearnMute),
                Entry::item(MenuItem::LearnMono),
                Entry::separator(),
                Entry::item(MenuItem::ClearMappings),
                Entry::separator(),
            ]);
            entries.extend(application_entries());
            entries
        }
    }
}

/// The labels of `kind`, separators rendered as `"---"` (the shape the tests
/// assert on).
pub fn labels(kind: &ContextMenuKind) -> Vec<&'static str> {
    entries(kind)
        .into_iter()
        .map(|entry| match entry.item {
            Some(item) => item.label(),
            None => "---",
        })
        .collect()
}

/// The open menu: which one, where its top-left corner sits, and when it was
/// opened.
///
/// The timestamp matters: the very click that opens a menu is still "a click
/// outside the menu" during the same frame, so a dismissal rule without it would
/// close the menu before it is ever painted.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextMenu {
    pub kind: ContextMenuKind,
    pub pos: egui::Pos2,
    /// `ui.input(|i| i.time)` of the frame that opened it (`NaN` = unknown, in
    /// which case the menu is dismissible immediately).
    pub opened_at: f64,
}

impl ContextMenu {
    pub fn new(kind: ContextMenuKind, pos: egui::Pos2) -> Self {
        Self {
            kind,
            pos,
            opened_at: f64::NAN,
        }
    }

    /// The same menu, with the frame time it appeared at.
    pub fn opened_at(mut self, time: f64) -> Self {
        self.opened_at = time;
        self
    }
}

/// Turn an activated entry into the action it performs.
pub fn action_for(item: MenuItem, kind: &ContextMenuKind) -> Option<UiAction> {
    Some(match (item, kind) {
        (MenuItem::RestartEngine, _) => UiAction::RestartEngine,
        (MenuItem::OpenDiagnostics, _) => UiAction::OpenDiagnostics,
        (MenuItem::OpenProjectPage, _) => UiAction::OpenProjectPage,
        (MenuItem::MidiPort, _) => UiAction::OpenMidiPortPicker,
        (MenuItem::Rename, ContextMenuKind::Strip { uid, .. }) => {
            UiAction::BeginRename { uid: uid.clone() }
        }
        (MenuItem::Delete, ContextMenuKind::Strip { uid, .. }) => {
            UiAction::RequestDelete { uid: uid.clone() }
        }
        (MenuItem::FxSettings, ContextMenuKind::Fx { uid, effect }) => UiAction::OpenFxSettings {
            uid: uid.clone(),
            effect: *effect,
        },
        (MenuItem::LearnVolume | MenuItem::LearnMute | MenuItem::LearnMono, ContextMenuKind::Midi { uid, .. }) => {
            UiAction::MidiLearnTarget {
                uid: uid.clone(),
                target: item.learn_target().expect("a learn entry has a target"),
            }
        }
        (MenuItem::CancelLearn, ContextMenuKind::Midi { .. }) => UiAction::MidiLearnCancel,
        (MenuItem::ClearMappings, ContextMenuKind::Midi { uid, .. }) => {
            UiAction::ClearMidiMappings { uid: uid.clone() }
        }
        // A local entry in the wrong menu cannot happen through the UI (the
        // entries are built from the kind); ignoring it keeps this total.
        _ => return None,
    })
}

/// Activate one entry of the open menu.
pub fn activate(state: &mut AppState, item: MenuItem, kind: &ContextMenuKind) {
    match action_for(item, kind) {
        Some(action) => state.apply(action),
        None => log::debug!("menu: {item:?} has no action in {kind:?}"),
    }
    state.context_menu = None;
}

/// Draw the open menu (a no-op when none is open) and apply what was clicked.
pub fn draw(state: &mut AppState, ui: &mut Ui) {
    let Some(menu) = state.context_menu.clone() else {
        return;
    };
    let entries = entries(&menu.kind);
    let mut clicked: Option<MenuItem> = None;
    let mut close = false;
    let engine_ready = state.bridge.is_some() && !state.is_disconnected();

    let inner = egui::Area::new(menu.kind.id().with("area"))
        .order(egui::Order::Foreground)
        .fixed_pos(menu.pos)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style())
                .inner_margin(egui::Margin::same(4))
                .show(ui, |ui| {
                    ui.set_min_width(180.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for entry in &entries {
                        let Some(item) = entry.item else {
                            ui.separator();
                            continue;
                        };
                        let enabled = !item.needs_engine() || engine_ready;
                        let button = ui.add_enabled(
                            enabled,
                            egui::Button::new(item.label()).min_size(Vec2::new(0.0, 18.0)),
                        );
                        if button.clicked() {
                            clicked = Some(item);
                        }
                    }
                });
        });

    // Dismissal: a click anywhere else, or Escape (egui reports both).  The
    // frame that *opened* the menu is skipped: the opening click is outside it,
    // so a rule without that guard would close the menu before it is painted.
    let now = ui.ctx().input(|input| input.time);
    let just_opened = !menu.opened_at.is_nan() && (now - menu.opened_at).abs() < 1e-6;
    let pointer_click = ui.ctx().input(|input| input.pointer.any_click());
    let dismissed_by_key = ui.ctx().input(|input| input.key_pressed(egui::Key::Escape));
    let dismissed_by_click =
        !just_opened && pointer_click && !inner.response.contains_pointer() && clicked.is_none();
    if dismissed_by_key || dismissed_by_click {
        close = true;
    }

    if let Some(item) = clicked {
        activate(state, item, &menu.kind);
    } else if close {
        state.context_menu = None;
    }
}

// ---------------------------------------------------------------------------
// "Open project page"
// ---------------------------------------------------------------------------

/// Open `url` with the desktop's default handler.
///
/// No `webbrowser`/`open` crate: the three commands every freedesktop system
/// has are tried in turn (`xdg-open`, `gio open`, `sensible-browser`), which is
/// also what a reviewer can see in the log when nothing works.  Returns the
/// command that was used, or the reason nothing could be launched — the menu
/// action turns that into a status line instead of a silent failure.
pub fn open_url(url: &str) -> Result<String, String> {
    open_url_with(
        url,
        &[
            ("xdg-open", &[]),
            ("gio", &["open"]),
            ("sensible-browser", &[]),
        ],
    )
}

/// [`open_url`] with an explicit candidate list (the test uses programs that
/// cannot exist, so it never spawns anything real).
fn open_url_with(url: &str, candidates: &[(&str, &[&str])]) -> Result<String, String> {
    let mut errors: Vec<String> = Vec::new();
    for (program, args) in candidates.iter().copied() {
        match std::process::Command::new(program)
            .args(args)
            .arg(url)
            .spawn()
        {
            // The child is not waited for: `xdg-open` blocks until the browser
            // exits on some desktops, and a UI must not wait for a browser.
            Ok(_child) => return Ok(format!("{program} {url}")),
            Err(err) => errors.push(format!("{program}: {err}")),
        }
    }
    Err(errors.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_menu() -> ContextMenuKind {
        ContextMenuKind::Strip {
            uid: "in-mic".to_string(),
            label: "Mic".to_string(),
        }
    }

    fn fx_menu() -> ContextMenuKind {
        ContextMenuKind::Fx {
            uid: "in-mic".to_string(),
            effect: EffectKey::Eq,
        }
    }

    fn midi_menu(armed: Option<crate::midi::MidiTarget>) -> ContextMenuKind {
        ContextMenuKind::Midi {
            uid: "in-mic".to_string(),
            label: "Mic".to_string(),
            armed,
        }
    }

    #[test]
    fn the_window_menu_has_the_three_application_entries_in_order() {
        // The MIDI port picker is the one entry the Qt window menu did not have
        // (it auto-picked a port at every start); it comes first, before the
        // application entries.
        assert_eq!(
            labels(&ContextMenuKind::Window),
            [
                "MIDI port...",
                "---",
                "Restart audio engine",
                "Debug window",
                "---",
                "Open project page"
            ]
        );
        // The separators are real separators, not empty buttons.
        let entries = entries(&ContextMenuKind::Window);
        assert!(entries[1].is_separator());
        assert_eq!(
            application_entries().len(),
            4,
            "two entries, a separator, one entry"
        );
    }

    /// The strip's MIDI menu is the Qt `_show_midi_menu`, entry for entry — that
    /// is the whole point: a MIDI Mix owner assigns one control per function.
    #[test]
    fn the_midi_menu_offers_the_three_learns_then_clear() {
        assert_eq!(
            labels(&midi_menu(None)),
            [
                "Learn Volume",
                "Learn Mute",
                "Learn Mono",
                "---",
                "Clear Mappings",
                "---",
                "Restart audio engine",
                "Debug window",
                "---",
                "Open project page",
            ]
        );
        // While a learn is armed, "Cancel learn" appears first (Qt had no way
        // back other than playing a control by accident).
        assert_eq!(
            labels(&midi_menu(Some(crate::midi::MidiTarget::Mute))),
            [
                "Cancel learn",
                "---",
                "Learn Volume",
                "Learn Mute",
                "Learn Mono",
                "---",
                "Clear Mappings",
                "---",
                "Restart audio engine",
                "Debug window",
                "---",
                "Open project page",
            ]
        );
    }

    #[test]
    fn the_strip_menu_puts_its_own_entries_first() {
        assert_eq!(
            labels(&strip_menu()),
            [
                "Rename",
                "Delete",
                "---",
                "Restart audio engine",
                "Debug window",
                "---",
                "Open project page",
            ]
        );
    }

    #[test]
    fn the_fx_menu_opens_the_effect_settings_first() {
        assert_eq!(
            labels(&fx_menu()),
            [
                "Effect settings",
                "---",
                "Restart audio engine",
                "Debug window",
                "---",
                "Open project page",
            ]
        );
    }

    #[test]
    fn every_entry_maps_to_the_action_it_performs() {
        assert_eq!(
            action_for(MenuItem::RestartEngine, &ContextMenuKind::Window),
            Some(UiAction::RestartEngine)
        );
        assert_eq!(
            action_for(MenuItem::OpenDiagnostics, &ContextMenuKind::Window),
            Some(UiAction::OpenDiagnostics)
        );
        assert_eq!(
            action_for(MenuItem::OpenProjectPage, &ContextMenuKind::Window),
            Some(UiAction::OpenProjectPage)
        );
        assert_eq!(
            action_for(MenuItem::Rename, &strip_menu()),
            Some(UiAction::BeginRename {
                uid: "in-mic".to_string()
            })
        );
        assert_eq!(
            action_for(MenuItem::Delete, &strip_menu()),
            Some(UiAction::RequestDelete {
                uid: "in-mic".to_string()
            })
        );
        assert_eq!(
            action_for(MenuItem::FxSettings, &fx_menu()),
            Some(UiAction::OpenFxSettings {
                uid: "in-mic".to_string(),
                effect: EffectKey::Eq
            })
        );
        // Local entries do not exist in the window menu…
        assert_eq!(action_for(MenuItem::Rename, &ContextMenuKind::Window), None);
        assert_eq!(action_for(MenuItem::FxSettings, &strip_menu()), None);
        // …and the window entries exist in every menu (that is the whole point
        // of `build_app_context_menu(menu)`).
        for kind in [strip_menu(), fx_menu()] {
            for item in [
                MenuItem::RestartEngine,
                MenuItem::OpenDiagnostics,
                MenuItem::OpenProjectPage,
            ] {
                assert!(action_for(item, &kind).is_some(), "{item:?} in {kind:?}");
            }
        }
    }

    #[test]
    fn the_learn_entries_arm_the_property_they_name() {
        use crate::midi::MidiTarget;
        let kind = midi_menu(None);
        assert_eq!(
            action_for(MenuItem::LearnVolume, &kind),
            Some(UiAction::MidiLearnTarget {
                uid: "in-mic".to_string(),
                target: MidiTarget::Volume
            })
        );
        assert_eq!(
            action_for(MenuItem::LearnMute, &kind),
            Some(UiAction::MidiLearnTarget {
                uid: "in-mic".to_string(),
                target: MidiTarget::Mute
            })
        );
        assert_eq!(
            action_for(MenuItem::LearnMono, &kind),
            Some(UiAction::MidiLearnTarget {
                uid: "in-mic".to_string(),
                target: MidiTarget::Mono
            })
        );
        assert_eq!(
            action_for(MenuItem::CancelLearn, &kind),
            Some(UiAction::MidiLearnCancel)
        );
        assert_eq!(
            action_for(MenuItem::ClearMappings, &kind),
            Some(UiAction::ClearMidiMappings {
                uid: "in-mic".to_string()
            })
        );
        // The port entry is global: it exists in the window menu…
        assert_eq!(
            action_for(MenuItem::MidiPort, &ContextMenuKind::Window),
            Some(UiAction::OpenMidiPortPicker)
        );
        // …and a learn entry never fires from another menu.
        assert_eq!(action_for(MenuItem::LearnVolume, &ContextMenuKind::Window), None);
        assert_eq!(action_for(MenuItem::LearnVolume, &strip_menu()), None);
        assert_eq!(action_for(MenuItem::ClearMappings, &fx_menu()), None);
        // None of the MIDI entries needs a live engine.
        for item in [
            MenuItem::MidiPort,
            MenuItem::LearnVolume,
            MenuItem::LearnMute,
            MenuItem::LearnMono,
            MenuItem::CancelLearn,
            MenuItem::ClearMappings,
        ] {
            assert!(!item.needs_engine(), "{item:?}");
        }
    }

    #[test]
    fn the_midi_menu_paints_headless_and_opens_the_picker_when_activated() {
        let ctx = crate::test_support::headless_context();
        let mut state = AppState::demo();
        state.context_menu = Some(ContextMenu::new(
            midi_menu(None),
            egui::Pos2::new(40.0, 40.0),
        ));
        let _ = crate::test_support::run_pass(&ctx, egui::Vec2::new(900.0, 700.0), 0.0, |ui| {
            draw(&mut state, ui)
        });
        let primitives =
            crate::test_support::run_pass(&ctx, egui::Vec2::new(900.0, 700.0), 1.0, |ui| {
                draw(&mut state, ui)
            });
        assert!(!primitives.is_empty(), "the MIDI menu painted nothing");
        assert!(state.midi_learning.is_none(), "a paint pass must not act");
        // Activating the port entry opens the picker (it never blocks on ALSA).
        state.context_menu = Some(ContextMenu::new(
            ContextMenuKind::Window,
            egui::Pos2::ZERO,
        ));
        activate(
            &mut state,
            MenuItem::MidiPort,
            &ContextMenuKind::Window,
        );
        assert!(state.midi_port_picker.is_some());
    }

    #[test]
    fn activating_a_learn_entry_arms_that_property() {
        use crate::midi::MidiTarget;
        let mut state = AppState::demo();
        let uid = state.strips[0].uid.clone();
        let kind = ContextMenuKind::Midi {
            uid: uid.clone(),
            label: state.strips[0].label.clone(),
            armed: None,
        };
        activate(&mut state, MenuItem::LearnMute, &kind);
        assert_eq!(
            state.midi_learning.as_ref(),
            Some(&(uid.clone(), MidiTarget::Mute))
        );
        assert!(state.is_midi_learning(&uid));
        // "Clear Mappings" empties the strip's three mappings.
        state.apply(UiAction::MidiLearnTarget {
            uid: uid.clone(),
            target: MidiTarget::Volume,
        });
        activate(&mut state, MenuItem::ClearMappings, &kind);
        assert!(state.midi_bindings_for(&uid).is_empty());
        assert!(state.midi_learning.is_none(), "a learn has nothing left to learn");
    }

    #[test]
    fn only_the_restart_needs_a_live_engine() {
        assert!(MenuItem::RestartEngine.needs_engine());
        for item in [
            MenuItem::OpenDiagnostics,
            MenuItem::OpenProjectPage,
            MenuItem::Rename,
            MenuItem::Delete,
            MenuItem::FxSettings,
        ] {
            assert!(!item.needs_engine(), "{item:?}");
        }
    }

    #[test]
    fn the_project_page_is_the_github_home_page() {
        assert_eq!(PROJECT_PAGE_URL, "https://github.com/grokuku/Holaf-Mix");
    }

    #[test]
    fn activating_an_entry_performs_it_and_closes_the_menu() {
        let mut state = AppState::demo();
        state.context_menu = Some(ContextMenu::new(strip_menu(), egui::Pos2::ZERO));
        activate(&mut state, MenuItem::Rename, &strip_menu());
        assert!(state.context_menu.is_none(), "the menu closes");
        assert_eq!(state.renaming.as_deref(), Some("in-mic"));
        // Delete asks for confirmation, it does not delete.
        state.context_menu = Some(ContextMenu::new(strip_menu(), egui::Pos2::ZERO));
        activate(&mut state, MenuItem::Delete, &strip_menu());
        assert_eq!(state.strips.len(), 7);
        assert_eq!(
            state.delete_confirm.as_ref().map(|d| d.uid.as_str()),
            Some("in-mic")
        );
        // The FX entry opens the settings dialog for **that** effect.
        state.context_menu = Some(ContextMenu::new(fx_menu(), egui::Pos2::ZERO));
        activate(&mut state, MenuItem::FxSettings, &fx_menu());
        assert_eq!(
            state.fx_dialog.as_ref().map(|dialog| dialog.effect),
            Some(EffectKey::Eq)
        );
        // "Debug window" builds the report.
        state.context_menu = Some(ContextMenu::new(ContextMenuKind::Window, egui::Pos2::ZERO));
        activate(
            &mut state,
            MenuItem::OpenDiagnostics,
            &ContextMenuKind::Window,
        );
        assert!(state.diagnostics.is_some());
    }

    /// The menu paints headless (an `egui::Area` needs two passes to be laid
    /// out), and drawing alone activates nothing.
    #[test]
    fn the_menu_paints_without_activating_anything() {
        let ctx = crate::test_support::headless_context();
        let mut state = AppState::demo();
        state.context_menu = Some(ContextMenu::new(
            ContextMenuKind::Window,
            egui::Pos2::new(40.0, 40.0),
        ));
        let _ = crate::test_support::run_pass(&ctx, egui::Vec2::new(800.0, 600.0), 0.0, |ui| {
            draw(&mut state, ui)
        });
        let primitives =
            crate::test_support::run_pass(&ctx, egui::Vec2::new(800.0, 600.0), 1.0, |ui| {
                draw(&mut state, ui)
            });
        assert!(!primitives.is_empty(), "the menu painted nothing");
        assert!(state.diagnostics.is_none(), "a paint pass must not act");
        assert!(state.context_menu.is_some(), "and must not close the menu");
    }

    #[test]
    fn open_url_reports_why_nothing_could_be_launched() {
        // Programs that cannot exist: no process is spawned and every candidate
        // is named in the error (mutating `PATH` would race with the other
        // tests of this process).
        let error = open_url_with(
            PROJECT_PAGE_URL,
            &[
                ("holaf-no-such-browser", &[]),
                ("holaf-no-such-browser-gio", &["open"]),
            ],
        )
        .expect_err("nothing can be launched");
        assert!(error.contains("holaf-no-such-browser"), "{error}");
        assert!(error.contains("holaf-no-such-browser-gio"), "{error}");
        // The real candidate list is what `open_url` uses.
        assert!(open_url_with("/x", &[]).is_err());
    }
}
