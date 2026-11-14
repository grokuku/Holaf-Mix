//! [`AppState`] — everything the UI draws from — plus the action model.
//!
//! Two ideas keep this crate testable:
//!
//! 1. **`render` is the only entry point.** It takes `&mut AppState` and a
//!    `&mut Ui`; the eframe binary, the headless PNG snapshot and the tests all
//!    call the very same function, so what the snapshot shows is what the app
//!    draws.  There is no second code path for "preview" or "screenshot mode".
//! 2. **Views return [`UiAction`]s instead of mutating.** A view may only
//!    *describe* what the user asked for.  [`AppState::apply`] performs it on
//!    the model **and** queues the action for the engine bridge — the same
//!    contract the PySide signals (`volume_changed`, `effect_toggled`, …) had,
//!    except it is a plain `Vec` we can assert on in a unit test.
//!
//! The demo state mirrors the Python app's default configuration: four input
//! strips (Mic/Default/Web/Chat) and three outputs (Casque/HP/Mix Out), where
//! `Mix Out` is a **virtual bus** (`device_name = None`) and therefore purple.

use std::collections::HashMap;

use holaf_core::model::{EffectKey, Strip, StripType};

use crate::bridge::{EngineBridge, EngineCommand, MidiProperty};
use crate::theme::images::ImageCache;
use crate::theme::resolve::{resolve, ResolvedStripStyle, StripDescriptor};
use crate::theme::ThemeFile;

/// Everything the user can ask the UI to do.
///
/// One variant per Qt signal of `StripWidget` + `MainWindow`, plus the two
/// dialog openers.  Keeping them as data (instead of direct mutation) is what
/// lets `AppState::apply` stay a pure, testable function.
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    /// Fader moved (`volume_changed`).
    Volume { uid: String, volume: f32 },
    /// MUTE clicked (`mute_changed`).
    Mute { uid: String, mute: bool },
    /// MONO clicked (`mono_changed`).
    Mono { uid: String, mono: bool },
    /// FX toggle clicked (`effect_toggled`).
    ToggleFx {
        uid: String,
        effect: EffectKey,
        active: bool,
    },
    /// Routing chip clicked (`route_changed`).
    Route {
        uid: String,
        target: String,
        active: bool,
    },
    /// "Default Sink" checkbox (`default_changed`).
    DefaultSink { uid: String, is_default: bool },
    /// A device was picked in the combo (`device_changed`).
    DeviceChanged {
        uid: String,
        device: Option<String>,
    },
    /// MIDI button clicked (`midi_learn_requested` / learn mode toggled).
    MidiLearn { uid: String, learning: bool },
    /// "×" clicked (`delete_requested`).
    Delete { uid: String },
    /// Label double-clicked (`label_changed` dialog opened).
    BeginRename { uid: String },
    /// Rename dialog confirmed.
    Rename { uid: String, label: String },
    /// "+" in a section header (`on_add_clicked`).
    AddStrip { kind: StripType },
    /// "SELECT APPS" clicked (`app_selection_requested`).
    SelectApps { uid: String },
    /// Device combo clicked (opens the future device picker).
    OpenDevicePicker { uid: String },
}

/// The UI state: theme, strips, meter levels and the caches.
pub struct AppState {
    /// Active theme file (tokens + per-strip overrides).
    pub theme: ThemeFile,
    /// Strips, in `config.json` order (inputs and outputs interleaved like the
    /// model: the view partitions them into the two columns).
    pub strips: Vec<Strip>,
    /// Decoded theme images.
    pub image_cache: ImageCache,
    /// Last known level per strip uid (`get_meter_levels()` in Python).
    levels: HashMap<String, [f32; 2]>,
    /// Actions the views produced this frame, waiting for the engine.
    pending_actions: Vec<UiAction>,
    /// Last status message (status bar).
    pub status: Option<String>,
    /// `Some((uid, property))` while a MIDI learn is armed.
    pub midi_learning: Option<(String, MidiProperty)>,
    /// `Some(uid)` while the rename dialog is open.
    pub renaming: Option<String>,
    /// Rename dialog buffer.
    pub rename_buffer: String,
    /// Optional engine bridge (absent in the demo/snapshot builds).
    pub bridge: Option<EngineBridge>,
    /// Seconds since start, driven by `ui.input(|i| i.time)` — time-based
    /// animation so a snapshot at a fixed time is reproducible.
    pub time: f32,
}

impl Default for AppState {
    fn default() -> Self {
        Self::demo()
    }
}

impl AppState {
    /// The Python app's default configuration, as `holaf_core` model strips.
    pub fn demo() -> Self {
        let mut strips = Vec::new();

        // --- inputs ---------------------------------------------------
        let mut mic = Strip::with_uid("in-mic", "Mic", StripType::Input);
        mic.volume = 0.85;
        mic.is_mono = true;
        mic.effects.insert(
            EffectKey::Gate,
            holaf_core::model::EffectState {
                active: true,
                params: holaf_core::fx::default_params(EffectKey::Gate),
            },
        );
        mic.effects.insert(
            EffectKey::NoiseCancel,
            holaf_core::model::EffectState {
                active: true,
                params: holaf_core::fx::default_params(EffectKey::NoiseCancel),
            },
        );
        mic.effects.insert(
            EffectKey::Eq,
            holaf_core::model::EffectState {
                active: true,
                params: holaf_core::fx::default_params(EffectKey::Eq),
            },
        );
        mic.effects.insert(
            EffectKey::Compressor,
            holaf_core::model::EffectState {
                active: true,
                params: holaf_core::fx::default_params(EffectKey::Compressor),
            },
        );
        mic.routes = vec!["out-casque".to_string(), "out-mix".to_string()];
        strips.push(mic);

        let mut default = Strip::with_uid("in-default", "Default", StripType::Input);
        default.volume = 0.9;
        default.is_default = true;
        default
            .effects
            .insert(EffectKey::Eq, holaf_core::model::EffectState::inactive_with_defaults(EffectKey::Eq));
        default.routes = vec!["out-casque".to_string()];
        strips.push(default);

        let mut web = Strip::with_uid("in-web", "Web", StripType::Input);
        web.volume = 0.7;
        web.effects
            .insert(EffectKey::Eq, holaf_core::model::EffectState::inactive_with_defaults(EffectKey::Eq));
        web.routes = vec!["out-hp".to_string()];
        strips.push(web);

        let mut chat = Strip::with_uid("in-chat", "Chat", StripType::Input);
        chat.volume = 0.6;
        chat.effects.insert(
            EffectKey::NoiseCancel,
            holaf_core::model::EffectState {
                active: true,
                params: holaf_core::fx::default_params(EffectKey::NoiseCancel),
            },
        );
        chat.routes = vec!["out-casque".to_string()];
        strips.push(chat);

        // --- outputs --------------------------------------------------
        let mut casque = Strip::with_uid("out-casque", "Casque", StripType::Output);
        casque.device_name = Some("alsa_output.pci-0000_00_1f.3.analog-stereo".to_string());
        casque.volume = 0.75;
        casque.effects.insert(
            EffectKey::Eq,
            holaf_core::model::EffectState {
                active: true,
                params: holaf_core::fx::default_params(EffectKey::Eq),
            },
        );
        strips.push(casque);

        let mut hp = Strip::with_uid("out-hp", "HP", StripType::Output);
        hp.device_name = Some("bluez_output.AC_80_0B_1F_22_33_44.a2dp-sink".to_string());
        hp.volume = 0.55;
        strips.push(hp);

        // No device name → a virtual bus (purple).
        let mut mix = Strip::with_uid("out-mix", "Mix Out", StripType::Output);
        mix.device_name = None;
        mix.volume = 1.0;
        mix.effects.insert(
            EffectKey::Compressor,
            holaf_core::model::EffectState {
                active: true,
                params: holaf_core::fx::default_params(EffectKey::Compressor),
            },
        );
        strips.push(mix);

        let mut state = Self::from_strips(strips, ThemeFile::holaf_dark());
        // Deterministic, plausible meter levels (see `animate`).
        state.animate(1.0);
        state
    }

    /// Build a state from an existing strip list and theme (config.json load).
    pub fn from_strips(strips: Vec<Strip>, theme: ThemeFile) -> Self {
        Self {
            theme,
            strips,
            image_cache: ImageCache::new(),
            levels: HashMap::new(),
            pending_actions: Vec::new(),
            status: None,
            midi_learning: None,
            renaming: None,
            rename_buffer: String::new(),
            bridge: None,
            time: 0.0,
        }
    }

    /// Attach an engine bridge (drains its events on every frame).
    pub fn with_bridge(mut self, bridge: EngineBridge) -> Self {
        self.bridge = Some(bridge);
        self
    }

    /// `(uid, label)` of every output strip — the routing targets of an input.
    pub fn output_targets(&self) -> Vec<(String, String)> {
        self.strips
            .iter()
            .filter(|s| s.kind == StripType::Output)
            .map(|s| (s.uid.clone(), s.label.clone()))
            .collect()
    }

    /// Current `[left, right]` levels of the strip at `index`, mono-collapsed
    /// like `StripWidget.update_vumeter`.
    pub fn levels_for(&self, index: usize) -> [f32; 2] {
        let Some(strip) = self.strips.get(index) else {
            return [0.0, 0.0];
        };
        let levels = self
            .levels
            .get(&strip.uid)
            .copied()
            .unwrap_or([0.0, 0.0]);
        if strip.is_mono {
            let peak = levels[0].max(levels[1]);
            [peak, peak]
        } else {
            levels
        }
    }

    /// Feed real meter values (from the engine bridge or the Python side).
    pub fn set_levels(&mut self, uid: impl Into<String>, levels: [f32; 2]) {
        self.levels.insert(
            uid.into(),
            [levels[0].clamp(0.0, 1.0), levels[1].clamp(0.0, 1.0)],
        );
    }

    /// Advance the time-based animation.
    ///
    /// With no engine attached the meters follow a deterministic function of
    /// `time` (two out-of-phase sines per strip, scaled by its volume), so the
    /// headless snapshot always shows the same, plausible picture and a test
    /// can assert on it.  With a bridge attached, real levels win and this is a
    /// no-op.
    pub fn animate(&mut self, time: f32) {
        self.time = time;
        if self.bridge.is_some() {
            return;
        }
        for index in 0..self.strips.len() {
            let (uid, volume, phase) = {
                let strip = &self.strips[index];
                (strip.uid.clone(), strip.volume, index as f32 * 1.7)
            };
            let left = 0.15 + 0.75 * (0.9 * (time + phase).sin().abs()) * volume;
            let right = 0.15 + 0.75 * (0.7 * (time + phase + 0.9).sin().abs()) * volume;
            self.set_levels(uid, [left, right]);
        }
    }

    /// Resolved style of the strip at `index` (override + opacity applied).
    pub fn strip_style(&self, index: usize) -> Option<ResolvedStripStyle> {
        self.strips
            .get(index)
            .map(|strip| resolve(&self.theme, &StripDescriptor::from_strip(strip)))
    }

    pub fn strip_index(&self, uid: &str) -> Option<usize> {
        self.strips.iter().position(|s| s.uid == uid)
    }

    /// Apply one action: mutate the model, queue it for the engine, and push it
    /// to the bridge when one is attached.
    ///
    /// Unknown uids are ignored (a strip deleted between two frames must not
    /// panic the next click), which is what `self.widgets.get(uid)` did in
    /// Python.
    pub fn apply(&mut self, action: UiAction) {
        match &action {
            UiAction::Volume { uid, volume } => {
                if let Some(index) = self.strip_index(uid) {
                    self.strips[index].volume = volume.clamp(0.0, 2.0);
                }
            }
            UiAction::Mute { uid, mute } => {
                if let Some(index) = self.strip_index(uid) {
                    self.strips[index].mute = *mute;
                }
            }
            UiAction::Mono { uid, mono } => {
                if let Some(index) = self.strip_index(uid) {
                    self.strips[index].is_mono = *mono;
                }
            }
            UiAction::ToggleFx { uid, effect, active } => {
                if let Some(index) = self.strip_index(uid) {
                    let entry = self.strips[index]
                        .effects
                        .entry(*effect)
                        .or_insert_with(|| holaf_core::model::EffectState::inactive_with_defaults(*effect));
                    entry.active = *active;
                }
            }
            UiAction::Route {
                uid,
                target,
                active,
            } => {
                if let Some(index) = self.strip_index(uid) {
                    let routes = &mut self.strips[index].routes;
                    if *active {
                        if !routes.iter().any(|r| r == target) {
                            routes.push(target.clone());
                        }
                    } else {
                        routes.retain(|r| r != target);
                    }
                }
            }
            UiAction::DefaultSink { uid, is_default } => {
                if let Some(index) = self.strip_index(uid) {
                    // Only one input can be the default sink (`_on_default_toggled`
                    // + `set_default_state` elsewhere in the list).
                    if *is_default {
                        for (i, strip) in self.strips.iter_mut().enumerate() {
                            if i != index && strip.kind == StripType::Input {
                                strip.is_default = false;
                            }
                        }
                    }
                    self.strips[index].is_default = *is_default;
                }
            }
            UiAction::DeviceChanged { uid, device } => {
                if let Some(index) = self.strip_index(uid) {
                    self.strips[index].device_name = device.clone();
                }
            }
            UiAction::MidiLearn { uid, learning } => {
                if *learning {
                    // The Qt menu offers volume/mute/mono; the strip's MIDI
                    // button arms volume, the most used one.  A future picker
                    // sends `MidiProperty::Mute` / `::Mono` instead.
                    self.midi_learning = Some((uid.clone(), MidiProperty::Volume));
                    self.status = Some(format!("MIDI learn armed on {uid}"));
                } else {
                    self.midi_learning = None;
                    self.status = Some("MIDI learn cancelled".to_string());
                }
            }
            UiAction::Delete { uid } => {
                if let Some(index) = self.strip_index(uid) {
                    let removed = self.strips.remove(index);
                    self.levels.remove(&removed.uid);
                    // Drop the routes pointing at the removed strip.
                    for strip in &mut self.strips {
                        strip.routes.retain(|r| r != &removed.uid);
                    }
                    self.status = Some(format!("Removed {}", removed.label));
                }
            }
            UiAction::BeginRename { uid } => {
                if let Some(index) = self.strip_index(uid) {
                    self.renaming = Some(uid.clone());
                    self.rename_buffer = self.strips[index].label.clone();
                }
            }
            UiAction::Rename { uid, label } => {
                if let Some(index) = self.strip_index(uid) {
                    let trimmed = label.trim();
                    if !trimmed.is_empty() {
                        self.strips[index].label = trimmed.to_string();
                    }
                }
                self.renaming = None;
            }
            UiAction::AddStrip { kind } => {
                // `on_add_clicked`: a fresh strip with a unique uid, appended at
                // the end of its column (the model keeps insertion order).
                let label = match kind {
                    StripType::Input => format!("Input {}", self.count_of(*kind) + 1),
                    StripType::Output => format!("Output {}", self.count_of(*kind) + 1),
                };
                let uid = self.unique_uid(*kind);
                let mut strip = Strip::with_uid(uid, label.clone(), *kind);
                if *kind == StripType::Output {
                    strip.device_name = None; // new outputs start as a virtual bus
                }
                strip.volume = 1.0;
                // Inputs go before the first output so the model stays grouped.
                let insert_at = if *kind == StripType::Input {
                    self.strips
                        .iter()
                        .position(|s| s.kind == StripType::Output)
                        .unwrap_or(self.strips.len())
                } else {
                    self.strips.len()
                };
                self.strips.insert(insert_at, strip);
                self.status = Some(format!("Added {label}"));
            }
            UiAction::SelectApps { uid } => {
                self.status = Some(format!("App selection requested for {uid}"));
            }
            UiAction::OpenDevicePicker { uid } => {
                self.status = Some(format!("Device picker requested for {uid}"));
            }
        }

        // Queue for the engine, then hand it to the bridge if there is one.
        // Structural changes (add / delete / rename) rebuild the graph plan, so
        // they send the whole strip list: `Backend::apply_snapshot`.
        let mut commands: Vec<EngineCommand> = Vec::new();
        if let Some(command) = engine_command(&action) {
            commands.push(command);
        }
        if matches!(
            action,
            UiAction::AddStrip { .. } | UiAction::Delete { .. } | UiAction::Rename { .. }
        ) {
            commands.push(EngineCommand::SyncSnapshot(self.strips.clone()));
        }
        if let Some(bridge) = &self.bridge {
            for command in commands {
                if let Err(err) = bridge.send(command) {
                    self.status = Some(format!("engine: {err}"));
                }
            }
        }
        if !matches!(action, UiAction::BeginRename { .. }) {
            log::debug!("ui action: {action:?}");
        }
        self.pending_actions.push(action);
    }

    fn count_of(&self, kind: StripType) -> usize {
        self.strips.iter().filter(|s| s.kind == kind).count()
    }

    fn unique_uid(&self, kind: StripType) -> String {
        let prefix = match kind {
            StripType::Input => "in",
            StripType::Output => "out",
        };
        for n in 1..=u32::MAX {
            let candidate = format!("{prefix}-{n}");
            if self.strip_index(&candidate).is_none() {
                return candidate;
            }
        }
        unreachable!("u32 uid space exhausted")
    }

    /// Take the actions accumulated since the last call (the Python app saved
    /// the config and pushed to the engine on each signal).
    pub fn take_actions(&mut self) -> Vec<UiAction> {
        std::mem::take(&mut self.pending_actions)
    }

    /// Actions queued but not yet drained (tests).
    pub fn pending_action_count(&self) -> usize {
        self.pending_actions.len()
    }

    /// Drain the bridge's events into the state (levels, errors).
    pub fn poll_engine(&mut self) {
        let Some(bridge) = &self.bridge else {
            return;
        };
        for event in bridge.drain_events() {
            match event {
                crate::bridge::EngineEvent::Ready => {
                    self.status = Some("engine ready".to_string());
                }
                crate::bridge::EngineEvent::Levels(levels) => {
                    for (uid, value) in levels {
                        self.set_levels(uid, value);
                    }
                }
                crate::bridge::EngineEvent::Error(reason) => {
                    self.status = Some(format!("engine error: {reason}"));
                }
                crate::bridge::EngineEvent::Applied(what) => {
                    log::trace!("engine applied {what}");
                }
            }
        }
    }
}

/// Map a UI action onto the engine command it implies (`None` for pure-UI
/// actions like opening a dialog).
pub fn engine_command(action: &UiAction) -> Option<EngineCommand> {
    Some(match action {
        UiAction::Volume { uid, volume } => EngineCommand::SetVolume {
            uid: uid.clone(),
            volume: *volume,
        },
        UiAction::Mute { uid, mute } => EngineCommand::SetMute {
            uid: uid.clone(),
            mute: *mute,
        },
        UiAction::Mono { uid, mono } => EngineCommand::SetMono {
            uid: uid.clone(),
            mono: *mono,
        },
        UiAction::ToggleFx { uid, effect, active } => EngineCommand::SetEffect {
            uid: uid.clone(),
            effect: *effect,
            active: *active,
        },
        UiAction::Route {
            uid,
            target,
            active,
        } => EngineCommand::SetRoute {
            source: uid.clone(),
            target: target.clone(),
            active: *active,
        },
        UiAction::DefaultSink { uid, is_default } => {
            if *is_default {
                EngineCommand::SetDefaultSink { uid: uid.clone() }
            } else {
                return None;
            }
        }
        UiAction::DeviceChanged { uid, device } => EngineCommand::SetDevice {
            uid: uid.clone(),
            device: device.clone(),
        },
        UiAction::Delete { uid } => EngineCommand::RemoveStrip { uid: uid.clone() },
        // Structural: `apply` follows these with a full `SyncSnapshot`, so they
        // have no command of their own.
        UiAction::AddStrip { .. } | UiAction::Rename { .. } => return None,
        UiAction::MidiLearn { .. }
        | UiAction::BeginRename { .. }
        | UiAction::SelectApps { .. }
        | UiAction::OpenDevicePicker { .. } => return None,
    })
}

/// The single entry point: lock the width, animate, poll the engine, draw.
pub fn render(state: &mut AppState, ui: &mut egui::Ui) {
    let time = ui.input(|i| i.time).max(0.0);
    // Horizontal rule: the width is derived from the strip count, not chosen by
    // the user.  Done here (and not in the eframe shell) so the app, the
    // headless snapshot and the tests all go through the same geometry code.
    crate::layout::enforce_window_width(ui.ctx(), &state.strips);
    state.poll_engine();
    state.animate(time as f32);
    crate::views::main_window::render(state, ui);
    draw_rename_dialog(state, ui);
}

/// Rename modal (the Rust twin of `QInputDialog.getText` in `_rename_strip`).
fn draw_rename_dialog(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(uid) = state.renaming.clone() else {
        return;
    };
    let mut open = true;
    let mut confirmed = false;
    let mut cancelled = false;
    egui::Window::new("Rename strip")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                ui.label("New name:");
                let response = ui.add(egui::TextEdit::singleline(&mut state.rename_buffer));
                response.request_focus();
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    confirmed = true;
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Rename").clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });
    if confirmed {
        state.apply(UiAction::Rename {
            uid,
            label: state.rename_buffer.clone(),
        });
    } else if cancelled || !open {
        state.renaming = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn state() -> AppState {
        AppState::demo()
    }

    #[test]
    fn demo_state_matches_the_shipped_configuration() {
        let s = state();
        assert_eq!(s.strips.len(), 7);
        let labels: Vec<&str> = s.strips.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Mic", "Default", "Web", "Chat", "Casque", "HP", "Mix Out"]
        );
        assert_eq!(
            s.strips.iter().filter(|s| s.kind == StripType::Input).count(),
            4
        );
        assert_eq!(s.output_targets().len(), 3);
        // Three visual kinds are represented: input, physical output, bus.
        let kinds: Vec<_> = s
            .strips
            .iter()
            .map(crate::theme::StripKind::from_strip)
            .collect();
        assert!(kinds.contains(&crate::theme::StripKind::Input));
        assert!(kinds.contains(&crate::theme::StripKind::Output));
        assert!(kinds.contains(&crate::theme::StripKind::Bus));
    }

    #[test]
    fn demo_state_uses_the_default_theme() {
        let s = state();
        assert_eq!(s.theme, ThemeFile::holaf_dark());
        assert_eq!(s.theme.theme, Theme::holaf_dark());
    }

    #[test]
    fn volume_action_updates_the_model_and_queues_an_engine_command() {
        let mut s = state();
        s.apply(UiAction::Volume {
            uid: "in-mic".to_string(),
            volume: 0.42,
        });
        assert!((s.strips[0].volume - 0.42).abs() < 1e-6);
        assert_eq!(s.pending_action_count(), 1);
        let actions = s.take_actions();
        assert_eq!(engine_command(&actions[0]), Some(EngineCommand::SetVolume {
            uid: "in-mic".to_string(),
            volume: 0.42,
        }));
        assert_eq!(s.pending_action_count(), 0);
    }

    #[test]
    fn volume_is_clamped_to_a_sane_range() {
        let mut s = state();
        s.apply(UiAction::Volume {
            uid: "in-mic".to_string(),
            volume: 99.0,
        });
        assert_eq!(s.strips[0].volume, 2.0);
        s.apply(UiAction::Volume {
            uid: "in-mic".to_string(),
            volume: -3.0,
        });
        assert_eq!(s.strips[0].volume, 0.0);
    }

    #[test]
    fn mute_and_mono_toggle_independently() {
        let mut s = state();
        s.apply(UiAction::Mute {
            uid: "in-web".to_string(),
            mute: true,
        });
        assert!(s.strips[2].mute);
        assert!(!s.strips[2].is_mono);
        s.apply(UiAction::Mono {
            uid: "in-web".to_string(),
            mono: true,
        });
        assert!(s.strips[2].is_mono);
        assert_eq!(
            engine_command(&s.take_actions()[1]),
            Some(EngineCommand::SetMono {
                uid: "in-web".to_string(),
                mono: true
            })
        );
    }

    #[test]
    fn toggling_an_effect_flips_only_that_effect() {
        let mut s = state();
        s.apply(UiAction::ToggleFx {
            uid: "out-hp".to_string(),
            effect: EffectKey::Tube,
            active: true,
        });
        let hp = s.strips.iter().find(|s| s.uid == "out-hp").unwrap();
        assert!(hp.effects[&EffectKey::Tube].active);
        assert!(!hp.effects[&EffectKey::Eq].active);
        // The other strips are untouched.
        let mic = s.strips.iter().find(|s| s.uid == "in-mic").unwrap();
        assert!(!mic.effects[&EffectKey::Tube].active);
    }

    #[test]
    fn routing_toggle_adds_then_removes_the_target_once() {
        let mut s = state();
        s.apply(UiAction::Route {
            uid: "in-web".to_string(),
            target: "out-casque".to_string(),
            active: true,
        });
        let web = s.strips.iter().find(|s| s.uid == "in-web").unwrap();
        assert_eq!(web.routes, ["out-hp", "out-casque"]);
        // Clicking again must not duplicate it.
        s.apply(UiAction::Route {
            uid: "in-web".to_string(),
            target: "out-casque".to_string(),
            active: true,
        });
        let web = s.strips.iter().find(|s| s.uid == "in-web").unwrap();
        assert_eq!(web.routes.iter().filter(|r| *r == "out-casque").count(), 1);
        s.apply(UiAction::Route {
            uid: "in-web".to_string(),
            target: "out-casque".to_string(),
            active: false,
        });
        let web = s.strips.iter().find(|s| s.uid == "in-web").unwrap();
        assert_eq!(web.routes, ["out-hp"]);
    }

    #[test]
    fn default_sink_is_exclusive() {
        let mut s = state();
        assert!(s.strips.iter().find(|s| s.uid == "in-default").unwrap().is_default);
        s.apply(UiAction::DefaultSink {
            uid: "in-web".to_string(),
            is_default: true,
        });
        let defaults: Vec<&str> = s
            .strips
            .iter()
            .filter(|s| s.is_default)
            .map(|s| s.uid.as_str())
            .collect();
        assert_eq!(defaults, ["in-web"]);
        // Unchecking is allowed and leaves nobody default.
        s.apply(UiAction::DefaultSink {
            uid: "in-web".to_string(),
            is_default: false,
        });
        assert!(s.strips.iter().all(|s| !s.is_default));
    }

    #[test]
    fn device_change_moves_a_strip_between_output_and_bus() {
        let mut s = state();
        let mix = s.strip_index("out-mix").unwrap();
        assert_eq!(
            crate::theme::StripKind::from_strip(&s.strips[mix]),
            crate::theme::StripKind::Bus
        );
        s.apply(UiAction::DeviceChanged {
            uid: "out-mix".to_string(),
            device: Some("alsa_output.hdmi".to_string()),
        });
        assert_eq!(
            crate::theme::StripKind::from_strip(&s.strips[mix]),
            crate::theme::StripKind::Output
        );
        // …and back to a bus.
        s.apply(UiAction::DeviceChanged {
            uid: "out-mix".to_string(),
            device: None,
        });
        assert_eq!(
            crate::theme::StripKind::from_strip(&s.strips[mix]),
            crate::theme::StripKind::Bus
        );
    }

    #[test]
    fn deleting_a_strip_drops_its_routes_and_levels() {
        let mut s = state();
        s.set_levels("out-hp", [0.5, 0.5]);
        s.apply(UiAction::Delete {
            uid: "out-hp".to_string(),
        });
        assert_eq!(s.strips.len(), 6);
        assert!(s.strips.iter().all(|st| st.uid != "out-hp"));
        // `in-web` used to route to `out-hp`.
        let web = s.strips.iter().find(|s| s.uid == "in-web").unwrap();
        assert!(web.routes.is_empty(), "{:?}", web.routes);
        assert!(s.levels_for(s.strip_index("in-web").unwrap()) != [0.0, 0.0]);
        assert_eq!(s.status.as_deref(), Some("Removed HP"));
    }

    #[test]
    fn unknown_uids_are_ignored_instead_of_panicking() {
        let mut s = state();
        let before = s.strips.len();
        s.apply(UiAction::Volume {
            uid: "ghost".to_string(),
            volume: 0.3,
        });
        s.apply(UiAction::Delete {
            uid: "ghost".to_string(),
        });
        s.apply(UiAction::ToggleFx {
            uid: "ghost".to_string(),
            effect: EffectKey::Eq,
            active: true,
        });
        assert_eq!(s.strips.len(), before);
        assert_eq!(s.levels_for(999), [0.0, 0.0]);
    }

    #[test]
    fn adding_an_input_inserts_it_before_the_outputs() {
        let mut s = state();
        s.apply(UiAction::AddStrip {
            kind: StripType::Input,
        });
        assert_eq!(s.strips.len(), 8);
        assert_eq!(s.strips[4].label, "Input 5");
        assert_eq!(s.strips[4].kind, StripType::Input);
        assert_eq!(s.strips[5].label, "Casque");
        // The uid is unique and not a demo one.
        assert_eq!(s.strips[4].uid, "in-1");
    }

    #[test]
    fn adding_an_output_appends_a_virtual_bus() {
        let mut s = state();
        s.apply(UiAction::AddStrip {
            kind: StripType::Output,
        });
        let last = s.strips.last().unwrap();
        assert_eq!(last.label, "Output 4");
        assert_eq!(last.device_name, None);
        assert_eq!(
            crate::theme::StripKind::from_strip(last),
            crate::theme::StripKind::Bus
        );
    }

    #[test]
    fn renaming_sets_the_label_and_rejects_blank_names() {
        let mut s = state();
        s.apply(UiAction::BeginRename {
            uid: "in-mic".to_string(),
        });
        assert_eq!(s.renaming.as_deref(), Some("in-mic"));
        assert_eq!(s.rename_buffer, "Mic");
        s.apply(UiAction::Rename {
            uid: "in-mic".to_string(),
            label: "  Micro  ".to_string(),
        });
        assert_eq!(s.strips[0].label, "Micro");
        assert_eq!(s.renaming, None);

        s.apply(UiAction::Rename {
            uid: "in-mic".to_string(),
            label: "   ".to_string(),
        });
        assert_eq!(s.strips[0].label, "Micro", "a blank name must not apply");
    }

    #[test]
    fn midi_learn_arms_and_cancels() {
        let mut s = state();
        s.apply(UiAction::MidiLearn {
            uid: "in-chat".to_string(),
            learning: true,
        });
        assert!(s.midi_learning.is_some());
        assert!(s.status.as_deref().unwrap().contains("MIDI learn"));
        s.apply(UiAction::MidiLearn {
            uid: "in-chat".to_string(),
            learning: false,
        });
        assert!(s.midi_learning.is_none());
    }

    #[test]
    fn dialog_openers_queue_no_engine_command() {
        for action in [
            UiAction::SelectApps {
                uid: "in-mic".to_string(),
            },
            UiAction::OpenDevicePicker {
                uid: "in-mic".to_string(),
            },
            UiAction::BeginRename {
                uid: "in-mic".to_string(),
            },
            UiAction::MidiLearn {
                uid: "in-mic".to_string(),
                learning: true,
            },
        ] {
            assert_eq!(engine_command(&action), None, "{action:?}");
        }
    }

    #[test]
    fn levels_are_clamped_and_mono_collapses_the_two_channels() {
        let mut s = state();
        s.set_levels("in-mic", [2.0, -1.0]);
        assert_eq!(s.levels_for(0), [1.0, 1.0], "mono in-mic takes the peak");
        s.set_levels("in-web", [0.2, 0.8]);
        assert_eq!(s.levels_for(2), [0.2, 0.8], "stereo keeps both channels");
    }

    #[test]
    fn demo_animation_is_deterministic_for_a_fixed_time() {
        let mut a = state();
        let mut b = state();
        a.animate(3.5);
        b.animate(3.5);
        assert_eq!(a.levels_for(0), b.levels_for(0));
        assert_ne!(a.levels_for(0), [0.0, 0.0], "the meters must be alive");
        // Every level stays inside 0..=1.
        for index in 0..a.strips.len() {
            let [l, r] = a.levels_for(index);
            assert!((0.0..=1.0).contains(&l) && (0.0..=1.0).contains(&r));
        }
    }

    #[test]
    fn engine_commands_cover_every_audio_action() {
        let cases = [
            UiAction::Volume {
                uid: "u".into(),
                volume: 0.5,
            },
            UiAction::Mute {
                uid: "u".into(),
                mute: true,
            },
            UiAction::Mono {
                uid: "u".into(),
                mono: true,
            },
            UiAction::ToggleFx {
                uid: "u".into(),
                effect: EffectKey::Eq,
                active: true,
            },
            UiAction::Route {
                uid: "u".into(),
                target: "t".into(),
                active: true,
            },
            UiAction::DefaultSink {
                uid: "u".into(),
                is_default: true,
            },
            UiAction::DeviceChanged {
                uid: "u".into(),
                device: Some("dev".into()),
            },
            UiAction::Delete { uid: "u".into() },
        ];
        for case in cases {
            assert!(
                engine_command(&case).is_some(),
                "{case:?} must reach the engine"
            );
        }
        // Unchecking "Default Sink" has no engine counterpart (the daemon only
        // accepts "make this the default").
        assert_eq!(
            engine_command(&UiAction::DefaultSink {
                uid: "u".into(),
                is_default: false
            }),
            None
        );
        // Structural changes carry no single command: they push a snapshot.
        assert_eq!(
            engine_command(&UiAction::AddStrip {
                kind: StripType::Input
            }),
            None
        );
        assert_eq!(
            engine_command(&UiAction::Rename {
                uid: "u".into(),
                label: "x".into()
            }),
            None
        );
    }

    #[test]
    fn structural_actions_push_a_full_snapshot_to_the_bridge() {
        // With no bridge nothing panics, and the action is still queued for the
        // config writer.
        let mut s = state();
        s.apply(UiAction::AddStrip {
            kind: StripType::Output,
        });
        assert_eq!(s.pending_action_count(), 1);

        // With a mock engine the snapshot must arrive as a `SyncSnapshot`.
        let mut s = state();
        s.bridge = Some(EngineBridge::spawn_mock());
        s.apply(UiAction::Rename {
            uid: "in-mic".to_string(),
            label: "Micro".to_string(),
        });
        let bridge = s.bridge.take().unwrap();
        let mut seen = Vec::new();
        for _ in 0..200 {
            seen.extend(bridge.drain_events());
            if seen.iter().any(|e| {
                matches!(e, crate::bridge::EngineEvent::Applied(what) if what.contains("snapshot"))
            }) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            seen.iter().any(|e| matches!(
                e,
                crate::bridge::EngineEvent::Applied(what) if what == "snapshot 7 strips"
            )),
            "{seen:?}"
        );
        bridge.shutdown();
    }

    #[test]
    fn strip_style_reflects_the_resolved_kind() {
        let mut s = state();
        let bus = s.strip_index("out-mix").unwrap();
        let style = s.strip_style(bus).unwrap();
        assert_eq!(style.kind, crate::theme::StripKind::Bus);
        assert!(style.body.is_gradient());
        s.apply(UiAction::DeviceChanged {
            uid: "out-mix".to_string(),
            device: Some("alsa_output.x".to_string()),
        });
        assert_eq!(
            s.strip_style(bus).unwrap().kind,
            crate::theme::StripKind::Output
        );
        assert_eq!(s.strip_style(999), None);
    }

    #[test]
    fn actions_push_to_the_bridge_when_one_is_attached() {
        let mut s = state();
        let bridge = EngineBridge::spawn_mock();
        let events = bridge.drain_events();
        assert!(matches!(
            events.as_slice(),
            [crate::bridge::EngineEvent::Ready] | []
        ));
        s.bridge = Some(bridge);
        s.apply(UiAction::Volume {
            uid: "in-mic".to_string(),
            volume: 0.33,
        });
        // No error surfaced in the status bar.
        assert!(s.status.is_none(), "{:?}", s.status);
        s.bridge.take().unwrap().shutdown();
    }
}
