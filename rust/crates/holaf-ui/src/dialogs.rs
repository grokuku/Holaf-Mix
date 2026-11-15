//! The dialogs of the Rust UI.
//!
//! The PySide app reaches for `QInputDialog` (rename), `QMessageBox` (delete
//! confirmation), a bespoke `EffectSettingsDialog` (FX control ports) and a
//! `QComboBox` over `pipewire_utils.get_audio_nodes()` (device binding).  This
//! module draws the Rust equivalents with `egui::Window`, and — like the views —
//! **never mutates the model**: a widget only produces an
//! [`UiAction`](crate::app::UiAction), which the caller applies.
//!
//! State lives in [`AppState`] (`fx_dialog`, `add_dialog`, `delete_confirm`,
//! `device_picker`) so that a dialog survives frames and can be opened from a
//! test without a window:
//!
//! ```ignore
//! state.apply(UiAction::OpenFxSettings { uid: "in-mic".into(), effect: EffectKey::Eq });
//! assert!(state.fx_dialog.is_some());
//! ```
//!
//! # FX settings: what is reproduced from Qt, and what is not
//!
//! * the **range** of every control comes from [`crate::fx_ui::range_for_param`]
//!   (the Python keyword table), the **order** from
//!   [`crate::fx_ui::sorted_param_keys`] (EQ bands by frequency), and ports whose
//!   name contains `=` are hidden (the SPA formatter drops them);
//! * Qt applies the change **once on dialog close**; here every slider move
//!   updates the model immediately (the strip shows the value at once) and the
//!   engine push is debounced by [`AppState::apply`] — same end result, no
//!   "nothing happened" dead time while the dialog is open.

use egui::Ui;

use holaf_core::model::{EffectKey, StripType};

use crate::app::{AppState, UiAction};
use crate::fx_ui;

/// Which effect of which strip the settings dialog edits, plus its working copy
/// of the control ports.
#[derive(Debug, Clone, PartialEq)]
pub struct FxSettings {
    pub uid: String,
    pub effect: EffectKey,
    /// Strip label, shown in the title (`Settings: EQ — Mic`).
    pub label: String,
    /// Working copy: a slider writes here through `UiAction::FxParam`, so the
    /// dialog never borrows the model while drawing.
    pub params: indexmap::IndexMap<String, f32>,
}

impl FxSettings {
    pub fn title(&self) -> String {
        format!(
            "Settings: {} — {}",
            self.effect.as_str().to_uppercase(),
            self.label
        )
    }
}

/// "Add strip": the label field the Qt `QInputDialog.getText` offers, plus the
/// kind (the Qt dialog gets the kind from the "+" button that opened it).
#[derive(Debug, Clone, PartialEq)]
pub struct AddStripDialog {
    pub kind: StripType,
    pub label: String,
}

/// Delete confirmation (`QMessageBox.question` "Are you sure…", default No).
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteConfirm {
    pub uid: String,
    pub label: String,
}

/// Device binding popup — the Qt `QComboBox` drop-down, not a modal dialog.
///
/// Qt filled the strip's combo with `pipewire_utils.get_audio_nodes()` and the
/// list unfolded **under the button**; the Rust port first reproduced it as a
/// centred `egui::Window`, which meant two clicks and a window in the middle of
/// the mixer for a one-click choice.  This is the inline pop-up: every sink for
/// an output, every source for an input, plus the unbind entry
/// (`Apps / Virtual` on an input, `Virtual Sink (Bus)` on an output).
#[derive(Debug, Clone, PartialEq)]
pub struct DevicePicker {
    pub uid: String,
    pub kind: StripType,
    pub current: Option<String>,
    /// Frame time at which it opened.
    ///
    /// Same guard as [`crate::menus::ContextMenu::opened_at`]: the click that
    /// opened the pop-up lands *outside* it, so without this the outside-click
    /// rule would close it on the very frame it first appears.
    pub opened_at: f64,
}

/// One line of the device pop-up.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceChoice {
    /// The text the user sees (`Analog Stereo  (alsa_output.…)`).
    pub label: String,
    /// The node name to bind, or `None` for the "no device" entry.
    pub device: Option<String>,
}

/// MIDI port picker: the list of inputs the shell last saw, plus "No controller".
///
/// The Qt UI had no such dialog — it opened the first port whose name contained
/// `mix` at every start, which is fine until the controller is not plugged in and
/// a synth gets grabbed instead.  Selecting a port here stores it in
/// `ui_rust.midi_port` (`UiAction::MidiPortSelected`) and the shell connects to
/// it, then re-connects every 2 s if it disappears.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MidiPortPicker {
    /// What is selected in the list right now (`None` = no controller).
    pub selected: Option<String>,
}

impl DevicePicker {
    /// Does this picker offer `device`?  Outputs bind sinks, inputs bind sources
    /// — `_populate_devices` in the Python main window.
    pub fn accepts(&self, device: &crate::engine_json::DeviceInfo) -> bool {
        match self.kind {
            StripType::Input => device.is_source(),
            StripType::Output => device.is_sink(),
        }
    }

    /// The "no device" entry's text — the same words the strip's combo shows
    /// when nothing is bound (`device_combo_text`), so the two never disagree.
    pub fn unbind_label(&self) -> &'static str {
        match self.kind {
            StripType::Input => "Apps / Virtual",
            StripType::Output => "Virtual Sink (Bus)",
        }
    }

    /// The lines the pop-up lists: the unbind entry first, then every device the
    /// engine reported whose media class matches this strip.
    ///
    /// Pure on purpose: the filtering (`accepts`) and the ordering are what a
    /// reviewer wants to check, and they are testable without painting a single
    /// pixel.
    pub fn entries(&self, devices: &[crate::engine_json::DeviceInfo]) -> Vec<DeviceChoice> {
        let mut entries = vec![DeviceChoice {
            label: self.unbind_label().to_string(),
            device: None,
        }];
        entries.extend(
            devices
                .iter()
                .filter(|d| self.accepts(d))
                .map(|device| DeviceChoice {
                    label: format!("{}  ({})", device.display(), device.name),
                    device: Some(device.name.clone()),
                }),
        );
        entries
    }

    /// Is `choice` the binding the strip currently has?
    pub fn is_current(&self, choice: &DeviceChoice) -> bool {
        self.current == choice.device
    }
}

/// Draw every open dialog.  Called once per frame by [`crate::render`].
pub fn draw(state: &mut AppState, ui: &mut Ui) {
    draw_fx_settings(state, ui);
    draw_add_strip(state, ui);
    draw_delete_confirm(state, ui);
    draw_device_picker(state, ui);
    draw_apps(state, ui);
    draw_midi_port(state, ui);
    draw_diagnostics(state, ui);
}

/// "MIDI port" — pick the controller to listen to (or none).
///
/// The list comes from [`AppState::midi_ports`], which the shell refreshes every
/// retry interval, so a controller plugged in while the dialog is open appears
/// within two seconds without a Refresh button.  With no `midi` feature the list
/// is empty by construction and the dialog says why instead of showing a void.
fn draw_midi_port(state: &mut AppState, ui: &mut Ui) {
    let Some(dialog) = state.midi_port_picker.clone() else {
        return;
    };
    let mut selected = dialog.selected.clone();
    let mut open = true;
    let mut confirm = false;
    let mut cancel = false;
    let ports = state.midi_ports.clone();
    let error = state.midi_ports_error.clone();
    let connected = state.midi_connected;
    let active = state.midi_port_name.clone();
    egui::Window::new("MIDI controller")
        .id(egui::Id::unique("holaf-midi-port"))
        .collapsible(false)
        .resizable(false)
        .default_width(360.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            ui.label("Controller to listen to:");
            let hint = match &error {
                Some(error) => error.clone(),
                None if ports.is_empty() => {
                    "No MIDI input found (plug the controller in, or load the snd-seq module)"
                        .to_string()
                }
                None => "Learning and CC control apply to the selected port".to_string(),
            };
            ui.label(egui::RichText::new(hint).size(10.0).weak());
            ui.separator();
            ui.radio_value(&mut selected, None, "No controller");
            for name in &ports {
                let label = if connected && Some(name) == active.as_ref() {
                    format!("{name}  (connected)")
                } else {
                    name.clone()
                };
                ui.radio_value(&mut selected, Some(name.clone()), label);
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    confirm = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    if confirm {
        state.apply(UiAction::MidiPortSelected { name: selected });
    } else if cancel || !open {
        state.midi_port_picker = None;
    } else if let Some(current) = &mut state.midi_port_picker {
        current.selected = selected;
    }
}

/// "SELECT APPS" — the Qt `AppSelectionDialog`: one checkbox per application
/// name (`running ∪ assigned`, sorted) and an OK button.
///
/// The dialog never mutates the model: it writes to
/// [`AppState::apps_dialog`] while open and emits
/// [`UiAction::SelectAppsDone`] on OK, which is what stores `assigned_apps` and
/// triggers the first `app_move`.
fn draw_apps(state: &mut AppState, ui: &mut Ui) {
    let Some(dialog) = state.apps_dialog.clone() else {
        return;
    };
    let mut open = true;
    let mut confirmed = false;
    let mut cancelled = false;
    let mut toggled: Option<String> = None;
    egui::Window::new(format!("Assign applications — {}", dialog.label))
        .id(egui::Id::unique(("holaf-apps", dialog.uid.as_str())))
        .collapsible(false)
        .resizable(false)
        .default_width(300.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            ui.label("Select applications to route to this strip:");
            // The engine's own reason wins over the generic hint: an empty list
            // because the engine refused the question must not read as "nothing
            // is playing".
            let hint = state.apps_error.clone().unwrap_or_else(|| dialog.hint());
            ui.label(egui::RichText::new(hint).size(10.0).weak());
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    for name in &dialog.candidates {
                        let mut checked = dialog.is_selected(name);
                        if ui.checkbox(&mut checked, name).changed() {
                            toggled = Some(name.clone());
                        }
                    }
                });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });
    if let Some(name) = toggled {
        if let Some(current) = &mut state.apps_dialog {
            current.toggle(&name);
        }
        return;
    }
    if confirmed {
        let names = dialog.chosen();
        state.apply(UiAction::SelectAppsDone {
            uid: dialog.uid.clone(),
            names,
        });
    } else if cancelled || !open {
        state.apps_dialog = None;
    }
}

/// The diagnostics window ("Debug window"): five tabs, *Refresh* and *Copy
/// report*.
///
/// The report itself is built by [`crate::diagnostics`] (pure); this function
/// only paints it.  The **Journal** tab shows the log ring buffer
/// ([`crate::logbuf`]) — the only place a desktop-launched app can read its own
/// log — and is the one tab that auto-scrolls (a log is read at its tail).
/// "Copy report" goes through [`crate::clipboard::copy`], which reports whether
/// a system clipboard exists in this build: the status line must never claim
/// the text reached the desktop when only egui's in-app fallback got it (that
/// was the bug — the button said "copied" and pasted nothing).
fn draw_diagnostics(state: &mut AppState, ui: &mut Ui) {
    let Some(diagnostics) = state.diagnostics.clone() else {
        return;
    };
    let mut open = true;
    let mut tab = diagnostics.tab;
    let mut refresh = false;
    let mut copy = false;
    let mut close = false;
    egui::Window::new("Holaf-Mix — diagnostics")
        .id(egui::Id::unique("holaf-diagnostics"))
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .default_height(420.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                for candidate in crate::diagnostics::DiagTab::ALL {
                    if ui
                        .selectable_label(tab == candidate, candidate.title())
                        .clicked()
                    {
                        tab = candidate;
                    }
                }
            });
            ui.separator();
            let report = &diagnostics.report;
            egui::ScrollArea::vertical()
                .id_salt(("diag-tab", tab.title()))
                // The journal is followed at its tail; every other tab is read
                // from the top.
                .stick_to_bottom(tab == crate::diagnostics::DiagTab::Log)
                .max_height(ui.ctx().content_rect().height() * 0.55)
                .show(ui, |ui| match tab {
                    crate::diagnostics::DiagTab::Summary => {
                        ui.label(format!(
                            "{} error(s), {} warning(s), {} info",
                            report.errors(),
                            report.warnings(),
                            report.infos()
                        ));
                        if report.findings.is_empty() {
                            ui.label("(none — everything looks healthy)");
                        }
                        for finding in &report.findings {
                            ui.label(
                                egui::RichText::new(format!(
                                    "[{}] {} — {}",
                                    finding.severity.tag(),
                                    finding.id,
                                    finding.message
                                ))
                                .color(severity_color(finding.severity)),
                            );
                            if let Some(hint) = &finding.hint {
                                ui.label(
                                    egui::RichText::new(format!("   → {hint}"))
                                        .size(10.0)
                                        .weak(),
                                );
                            }
                        }
                    }
                    crate::diagnostics::DiagTab::Engine => {
                        egui::Grid::new("diag-engine")
                            .num_columns(2)
                            .show(ui, |ui| {
                                let engine = &report.engine;
                                for (key, value) in [
                                    ("available", engine.available.to_string()),
                                    ("running", engine.running.to_string()),
                                    ("degraded", engine.degraded.to_string()),
                                    (
                                        "default sink",
                                        engine
                                            .default_sink
                                            .clone()
                                            .unwrap_or_else(|| "?".to_string()),
                                    ),
                                    (
                                        "fx host",
                                        format!(
                                            "mode={} pid={} respawns={} chains={}",
                                            engine.fx_host.mode,
                                            engine
                                                .fx_host
                                                .pid
                                                .map(|pid| pid.to_string())
                                                .unwrap_or_else(|| "?".to_string()),
                                            engine.fx_host.respawns,
                                            engine.fx_host.chains.len()
                                        ),
                                    ),
                                    (
                                        "mainloop pumps",
                                        engine
                                            .mainloop_pumps
                                            .map(|pumps| pumps.to_string())
                                            .unwrap_or_else(|| "?".to_string()),
                                    ),
                                ] {
                                    ui.label(key);
                                    ui.label(value);
                                    ui.end_row();
                                }
                            });
                        // Rate/quantum + the `pw-top` verdict: the two numbers
                        // that explain crackling.  A wrapped label, not a Grid
                        // cell — a long line would otherwise stretch the dialog
                        // past the window it floats over.
                        ui.add(
                            egui::Label::new(format!("timing {}", report.engine.timing.summary()))
                                .wrap(),
                        );
                        ui.separator();
                        for strip in &report.engine.strips {
                            ui.label(format!(
                                "{} — bus {} — device {} — vol {:.2} — fx {}",
                                strip.label,
                                strip.node_name.as_deref().unwrap_or("?"),
                                strip.device_name.as_deref().unwrap_or("-"),
                                strip.volume,
                                if strip.active_effects.is_empty() {
                                    "-".to_string()
                                } else {
                                    strip.active_effects.join(",")
                                }
                            ));
                        }
                        for item in &report.engine.pending {
                            ui.label(
                                egui::RichText::new(format!(
                                    "pending {}: {}",
                                    item.key, item.message
                                ))
                                .color(egui::Color32::from_rgb(0xd0, 0x60, 0x60)),
                            );
                        }
                    }
                    crate::diagnostics::DiagTab::Graph => {
                        ui.label(format!("metering sources: {}", report.engine.meter_sources));
                        if !report.meter_missing.is_empty() {
                            ui.label(format!(
                                "no meter source: {}",
                                report.meter_missing.join(", ")
                            ));
                        }
                        ui.separator();
                        ui.label(format!(
                            "Holaf_* nodes ({})",
                            report.engine.holaf_nodes.len()
                        ));
                        for node in &report.engine.holaf_nodes {
                            ui.label(node);
                        }
                        ui.separator();
                        ui.label(format!("devices ({})", report.devices.len()));
                        for device in &report.devices {
                            ui.label(device);
                        }
                    }
                    crate::diagnostics::DiagTab::System => {
                        let system = &report.system;
                        for (key, value) in [
                            ("app version", system.app_version.clone()),
                            ("pid", system.app_pid.to_string()),
                            (
                                "PipeWire",
                                system
                                    .pipewire_version
                                    .clone()
                                    .unwrap_or_else(|| "?".to_string()),
                            ),
                            ("OS", system.os.clone()),
                            (
                                "config",
                                system
                                    .config_path
                                    .clone()
                                    .unwrap_or_else(|| "?".to_string()),
                            ),
                            (
                                "holafd pids",
                                if system.daemon_pids.is_empty() {
                                    "none".to_string()
                                } else {
                                    system
                                        .daemon_pids
                                        .iter()
                                        .map(u32::to_string)
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                },
                            ),
                            (
                                "generated",
                                crate::diagnostics::format_timestamp(report.generated_at),
                            ),
                        ] {
                            ui.label(format!("{key}: {value}"));
                        }
                    }
                    crate::diagnostics::DiagTab::Log => {
                        // The ring buffer as it is *now*: a snapshot, so the
                        // lines cannot change under the scroll position while
                        // the tab is being painted.
                        let lines = crate::logbuf::snapshot();
                        if lines.is_empty() {
                            ui.label("no log line yet");
                        }
                        for line in &lines {
                            // Monospace: the level column of
                            // `logbuf::format_line` only lines up in a fixed
                            // pitch font.
                            ui.label(egui::RichText::new(line).monospace().size(10.0));
                        }
                    }
                });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Refresh").clicked() {
                    refresh = true;
                }
                if ui.button("Copy report").clicked() {
                    copy = true;
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
    if let Some(current) = &mut state.diagnostics {
        current.tab = tab;
    }
    if copy {
        // One pasteable text: findings first, then the details.
        let text = state
            .diagnostics
            .as_ref()
            .map(|diagnostics| diagnostics.report.text())
            .unwrap_or_default();
        let outcome = crate::clipboard::copy(ui.ctx(), &text);
        state.status = Some(outcome.status("diagnostic report"));
    }
    if refresh {
        state.refresh_diagnostics();
    }
    if close || !open {
        state.diagnostics = None;
    }
}

/// Colour of a finding's label in the Summary tab.
fn severity_color(severity: crate::diagnostics::Severity) -> egui::Color32 {
    use crate::diagnostics::Severity;
    match severity {
        Severity::Error => egui::Color32::from_rgb(0xff, 0x55, 0x55),
        Severity::Warning => egui::Color32::from_rgb(0xff, 0xb4, 0x54),
        Severity::Info => egui::Color32::from_rgb(0x9a, 0xb0, 0xc8),
    }
}

fn draw_fx_settings(state: &mut AppState, ui: &mut Ui) {
    let Some(dialog) = state.fx_dialog.clone() else {
        return;
    };
    let mut open = true;
    let mut close_clicked = false;
    let mut actions: Vec<UiAction> = Vec::new();
    egui::Window::new(dialog.title())
        .id(egui::Id::unique((
            "holaf-fx-settings",
            dialog.uid.as_str(),
            dialog.effect.as_str(),
        )))
        .collapsible(false)
        .resizable(true)
        .default_width(460.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            // A long EQ (15 bands) gets a scroll area; a tube (2 ports) does not
            // need one but `max_height` only kicks in when it overflows.
            egui::ScrollArea::vertical()
                .max_height(ui.ctx().content_rect().height() * 0.7)
                .show(ui, |ui| {
                    for key in fx_ui::sorted_param_keys(&dialog.params) {
                        let value = dialog.params.get(&key).copied().unwrap_or(0.0);
                        let (min, max, step) = fx_ui::range_for_param(&key);
                        let mut current = fx_ui::snap(value, min, max, step);
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                egui::vec2(150.0, 18.0),
                                egui::Label::new(egui::RichText::new(&key).size(11.0)),
                            );
                            let changed = ui
                                .add(
                                    egui::Slider::new(&mut current, min..=max)
                                        .step_by(step as f64)
                                        .show_value(false),
                                )
                                .changed();
                            let typed = ui
                                .add(
                                    egui::DragValue::new(&mut current)
                                        .range(min..=max)
                                        .speed(step as f64)
                                        .max_decimals(3),
                                )
                                .changed();
                            if changed || typed {
                                actions.push(UiAction::FxParam {
                                    uid: dialog.uid.clone(),
                                    effect: dialog.effect,
                                    param: key.clone(),
                                    value: fx_ui::snap(current, min, max, step),
                                });
                            }
                        });
                    }
                });
            ui.separator();
            ui.horizontal(|ui| {
                // Qt's "Close" button (the dialog applies live here, so closing
                // never loses an edit).
                if ui.button("Close").clicked() {
                    close_clicked = true;
                }
                ui.label(
                    egui::RichText::new(if state.bridge.is_some() {
                        "changes apply live"
                    } else {
                        "no engine attached — changes are only saved"
                    })
                    .size(10.0)
                    .weak(),
                );
            });
        });
    for action in actions {
        state.apply(action);
    }
    if !open || close_clicked {
        state.fx_dialog = None;
    }
}

fn draw_add_strip(state: &mut AppState, ui: &mut Ui) {
    let Some(dialog) = state.add_dialog.clone() else {
        return;
    };
    let mut open = true;
    let mut confirmed = false;
    let mut cancelled = false;
    // The label buffer lives outside the closure: the dialog must not borrow the
    // model (`AppState`) while it is being drawn.
    let mut label = dialog.label.clone();
    egui::Window::new("Add strip")
        .id(egui::Id::unique(("holaf-add-strip", dialog.kind.as_str())))
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                ui.label("Name:");
                let response = ui.add(egui::TextEdit::singleline(&mut label));
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    confirmed = true;
                }
            });
            ui.label(
                egui::RichText::new(match dialog.kind {
                    StripType::Input => "an input strip (captures an app or a source)",
                    StripType::Output => "an output strip (starts as a virtual bus)",
                })
                .size(10.0)
                .weak(),
            );
            ui.horizontal(|ui| {
                if ui.button("Add").clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });
    if confirmed {
        let label = label.trim().to_string();
        state.apply(UiAction::AddStrip {
            kind: dialog.kind,
            label: (!label.is_empty()).then_some(label),
        });
    } else if cancelled || !open {
        state.add_dialog = None;
    } else if let Some(open_dialog) = &mut state.add_dialog {
        // Remember what was typed so the next frame starts from it.
        open_dialog.label = label;
    }
}

fn draw_delete_confirm(state: &mut AppState, ui: &mut Ui) {
    let Some(dialog) = state.delete_confirm.clone() else {
        return;
    };
    let mut open = true;
    let mut confirmed = false;
    let mut cancelled = false;
    egui::Window::new("Confirm Delete")
        .id(egui::Id::unique((
            "holaf-delete-confirm",
            dialog.uid.as_str(),
        )))
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            ui.label(format!(
                "Are you sure you want to remove '{}'?",
                dialog.label
            ));
            ui.horizontal(|ui| {
                // Qt's default button is "No".
                if ui.button("No").clicked() {
                    cancelled = true;
                }
                if ui.button("Yes").clicked() {
                    confirmed = true;
                }
            });
        });
    if confirmed {
        state.apply(UiAction::Delete { uid: dialog.uid });
    } else if cancelled || !open {
        state.delete_confirm = None;
    }
}

/// The device binding pop-up — a drop-down under the strip's combo button.
///
/// Anchoring: the strip view stores the combo's rect in egui memory
/// ([`crate::views::strip::device_combo_id`]) because views never mutate the
/// model, and a pop-up drawn from `dialogs` has no other way to learn where the
/// button it belongs to ended up.  When that rect is missing (a picker opened
/// from code, a headless test that draws only the dialog) it opens near the
/// top-left of the window instead of at (0, 0).
///
/// Dismissal mirrors [`crate::menus::draw`]: a click outside, Escape, or a
/// selection.  It is deliberately **non-modal** — nothing is dimmed, the strips
/// stay usable, and the choice costs one click instead of three.
fn draw_device_picker(state: &mut AppState, ui: &mut Ui) {
    let Some(dialog) = state.device_picker.clone() else {
        return;
    };
    let devices: Vec<crate::engine_json::DeviceInfo> = state.engine_devices.clone();
    let entries = dialog.entries(&devices);
    let devices_listed = entries.len() > 1;
    let empty_hint = state
        .devices_error
        .clone()
        .unwrap_or_else(|| "the engine reported no device".to_string());

    let screen = ui.ctx().content_rect();
    let anchor = ui.ctx().data(|memory| {
        memory.get_temp::<egui::Rect>(crate::views::strip::device_combo_id(&dialog.uid))
    });
    let pos = match anchor {
        Some(rect) => egui::pos2(rect.left(), rect.bottom() + 2.0),
        None => egui::pos2(screen.left() + 20.0, screen.top() + 60.0),
    };
    // At least as wide as the button it drops from (a device list narrower than
    // the combo that opened it looks like a bug), and never wider than a strip.
    let width = anchor.map_or(180.0, |rect| rect.width().clamp(150.0, 320.0));

    let mut chosen: Option<Option<String>> = None;
    let inner = egui::Area::new(egui::Id::unique((
        "holaf-device-popup",
        dialog.uid.as_str(),
    )))
    .order(egui::Order::Foreground)
    .fixed_pos(pos)
    .constrain(true)
    .show(ui.ctx(), |ui| {
        egui::Frame::popup(ui.style())
            .inner_margin(egui::Margin::same(4))
            .show(ui, |ui| {
                ui.set_min_width(width);
                ui.spacing_mut().item_spacing.y = 2.0;
                egui::ScrollArea::vertical()
                    .max_height(280.0)
                    .show(ui, |ui| {
                        for choice in &entries {
                            let selected = dialog.is_current(choice);
                            if ui.selectable_label(selected, &choice.label).clicked() {
                                chosen = Some(choice.device.clone());
                            }
                        }
                        if !devices_listed {
                            // Same wording as before: never an empty box, the
                            // reason the list is empty is the payload.
                            ui.label(egui::RichText::new(empty_hint.clone()).size(10.0).weak());
                        }
                    });
            });
    });

    // Dismissal (see `menus::draw`): skipping the frame that opened the pop-up is
    // what keeps the opening click from closing it immediately.
    let now = ui.ctx().input(|input| input.time);
    let just_opened = (now - dialog.opened_at).abs() < 1e-6;
    let pointer_click = ui.ctx().input(|input| input.pointer.any_click());
    let escaped = ui.ctx().input(|input| input.key_pressed(egui::Key::Escape));
    let dismissed = !just_opened
        && ((pointer_click && !inner.response.contains_pointer() && chosen.is_none()) || escaped);

    if let Some(device) = chosen {
        choose_device(state, &dialog.uid, device);
    } else if dismissed {
        state.device_picker = None;
    }
}

/// Apply a choice made in the device pop-up: bind the device (`None` = unbind)
/// and close the pop-up.
///
/// Separate from the drawing so the "a selection performs the right action"
/// contract is testable without simulating a pointer click on a floating area.
fn choose_device(state: &mut AppState, uid: &str, device: Option<String>) {
    state.apply(UiAction::DeviceChanged {
        uid: uid.to_string(),
        device,
    });
    state.device_picker = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;
    use crate::engine_json::DeviceInfo;

    #[test]
    fn the_fx_dialog_title_names_the_effect_and_the_strip() {
        let dialog = FxSettings {
            uid: "in-mic".into(),
            effect: EffectKey::Eq,
            label: "Mic".into(),
            params: Default::default(),
        };
        assert_eq!(dialog.title(), "Settings: EQ — Mic");
    }

    #[test]
    fn the_device_picker_only_accepts_the_matching_media_class() {
        let sink = DeviceInfo {
            id: 1,
            name: "alsa_output.x".into(),
            media_class: "Audio/Sink".into(),
            description: None,
        };
        let source = DeviceInfo {
            id: 2,
            name: "alsa_input.x".into(),
            media_class: "Audio/Source".into(),
            description: None,
        };
        let out = DevicePicker {
            uid: "out-1".into(),
            kind: StripType::Output,
            current: None,
            opened_at: 0.0,
        };
        assert!(out.accepts(&sink) && !out.accepts(&source));
        let input = DevicePicker {
            uid: "in-1".into(),
            kind: StripType::Input,
            current: None,
            opened_at: 0.0,
        };
        assert!(input.accepts(&source) && !input.accepts(&sink));
    }

    fn sink(name: &str, description: Option<&str>) -> DeviceInfo {
        DeviceInfo {
            id: 1,
            name: name.to_string(),
            media_class: "Audio/Sink".into(),
            description: description.map(str::to_string),
        }
    }

    fn source(name: &str) -> DeviceInfo {
        DeviceInfo {
            id: 2,
            name: name.to_string(),
            media_class: "Audio/Source".into(),
            description: None,
        }
    }

    /// The pop-up lists the unbind entry first, then only the devices whose media
    /// class the strip can use — and it says which one is the current binding.
    #[test]
    fn the_device_popup_lists_the_unbind_entry_then_the_filtered_devices() {
        let devices = vec![
            sink("alsa_output.hp", Some("Headphones")),
            source("alsa_input.mic"),
            sink("alsa_output.spk", None),
        ];
        let out = DevicePicker {
            uid: "out-hp".into(),
            kind: StripType::Output,
            current: Some("alsa_output.spk".to_string()),
            opened_at: 0.0,
        };
        let entries = out.entries(&devices);
        // 1 unbind entry + the two sinks, in the engine's order.
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert_eq!(entries[0].device, None);
        assert_eq!(entries[0].label, "Virtual Sink (Bus)");
        assert_eq!(
            entries[1..]
                .iter()
                .map(|entry| entry.device.clone().unwrap())
                .collect::<Vec<_>>(),
            vec!["alsa_output.hp", "alsa_output.spk"]
        );
        // The label carries the human description when the engine has one.
        assert!(
            entries[1].label.starts_with("Headphones"),
            "{}",
            entries[1].label
        );
        // …and exactly one entry is marked as the current binding.
        let current: Vec<_> = entries.iter().filter(|e| out.is_current(e)).collect();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].device.as_deref(), Some("alsa_output.spk"));

        // An input lists its own unbind wording and only the sources.
        let input = DevicePicker {
            uid: "in-mic".into(),
            kind: StripType::Input,
            current: None,
            opened_at: 0.0,
        };
        let entries = input.entries(&devices);
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].label, "Apps / Virtual");
        assert!(input.is_current(&entries[0]), "unbound = the unbind entry");
        assert_eq!(entries[1].device.as_deref(), Some("alsa_input.mic"));
        // An empty engine answer still offers the way back to a virtual bus.
        assert_eq!(input.entries(&[]).len(), 1);
    }

    /// A selection in the pop-up performs the binding and closes it — the
    /// contract the widget itself relies on (`draw_device_picker` calls exactly
    /// this).
    #[test]
    fn choosing_a_device_binds_it_and_closes_the_popup() {
        let mut state = AppState::demo();
        state.apply(UiAction::OpenDevicePicker {
            uid: "out-mix".into(),
        });
        let dialog = state.device_picker.clone().expect("popup open");
        choose_device(&mut state, &dialog.uid, Some("alsa_output.hp".into()));
        assert!(state.device_picker.is_none(), "a choice closes the pop-up");
        let node = state
            .strips
            .iter()
            .find(|strip| strip.uid == "out-mix")
            .expect("the strip is still there");
        assert_eq!(node.device_name.as_deref(), Some("alsa_output.hp"));

        // Choosing the unbind entry turns it back into a virtual bus.
        state.apply(UiAction::OpenDevicePicker {
            uid: "out-mix".into(),
        });
        choose_device(&mut state, "out-mix", None);
        let node = state
            .strips
            .iter()
            .find(|strip| strip.uid == "out-mix")
            .expect("the strip is still there");
        assert_eq!(node.device_name, None);
    }

    /// The pop-up anchors under the combo button the view published, and it
    /// closes on Escape and on a click outside — while a click *inside* keeps it
    /// open (the list is what the user is aiming at).
    #[test]
    fn the_device_popup_is_anchored_under_the_combo_and_dismissed_by_escape() {
        let ctx = crate::test_support::headless_context();
        let mut state = AppState::demo();
        state.engine_devices = vec![sink("alsa_output.hp", None), source("alsa_input.mic")];
        let combo = egui::Rect::from_min_size(egui::pos2(120.0, 60.0), egui::vec2(90.0, 18.0));
        ctx.data_mut(|data| {
            data.insert_temp(crate::views::strip::device_combo_id("out-mix"), combo)
        });
        // Open it *as the click that triggers it would*: `render` ticks the
        // clock before drawing, so `opened_at` equals the release frame's time.
        state.time = 1.2;
        state.apply(UiAction::OpenDevicePicker {
            uid: "out-mix".into(),
        });

        // Frame 0 (the opening frame): the outside-click rule must not fire.
        let run = |state: &mut AppState, time: f64, events: Vec<egui::Event>| {
            // `render` does this before drawing (`tick`); the pop-up's
            // `opened_at` comes from it, so drawing `dialogs` alone must keep the
            // clock honest.
            state.time = time as f32;
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                time: Some(time),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| draw(state, ui));
            output.textures_delta.clear();
        };
        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        run(
            &mut state,
            1.0,
            vec![egui::Event::PointerMoved(combo.center())],
        );
        run(&mut state, 1.1, vec![click(combo.center(), true)]);
        run(&mut state, 1.2, vec![click(combo.center(), false)]);
        assert!(
            state.device_picker.is_some(),
            "the click that opened the pop-up must not close it"
        );

        // Escape closes it.
        run(
            &mut state,
            2.0,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
        );
        assert!(
            state.device_picker.is_none(),
            "Escape must dismiss the pop-up"
        );

        // A click far away (the other column) closes it too.
        state.apply(UiAction::OpenDevicePicker {
            uid: "out-mix".into(),
        });
        state.time = 10.0;
        if let Some(dialog) = &mut state.device_picker {
            dialog.opened_at = 10.0;
        }
        let far = egui::pos2(600.0, 500.0);
        run(&mut state, 10.1, vec![egui::Event::PointerMoved(far)]);
        run(&mut state, 10.2, vec![click(far, true)]);
        run(&mut state, 10.3, vec![click(far, false)]);
        assert!(
            state.device_picker.is_none(),
            "a click outside must dismiss the pop-up"
        );
    }

    /// Drawing every dialog state headless must not panic — including with no
    /// engine and no devices.
    #[test]
    fn dialogs_paint_headless() {
        let ctx = crate::test_support::headless_context();
        let mut state = AppState::demo();
        state.apply(UiAction::OpenFxSettings {
            uid: "in-mic".into(),
            effect: EffectKey::Eq,
        });
        state.apply(UiAction::BeginAddStrip {
            kind: StripType::Output,
        });
        state.apply(UiAction::RequestDelete {
            uid: "out-hp".into(),
        });
        state.apply(UiAction::OpenDevicePicker {
            uid: "out-mix".into(),
        });
        state.apply(UiAction::SelectApps {
            uid: "in-mic".into(),
        });
        state.apply(UiAction::OpenDiagnostics);
        assert!(state.fx_dialog.is_some());
        assert!(state.add_dialog.is_some());
        assert!(state.delete_confirm.is_some());
        assert!(state.device_picker.is_some());
        assert!(state.apps_dialog.is_some());
        assert!(state.diagnostics.is_some());
        // egui measures a new `Area` in one pass and paints it in the next, so
        // two passes are needed to see the windows.
        let _ = crate::test_support::run_pass(&ctx, egui::vec2(800.0, 600.0), 0.0, |ui| {
            draw(&mut state, ui)
        });
        let primitives = crate::test_support::run_pass(&ctx, egui::vec2(800.0, 600.0), 1.0, |ui| {
            draw(&mut state, ui)
        });
        assert!(!primitives.is_empty(), "the dialogs painted nothing");
        // …and nothing was confirmed by simply drawing them.
        assert_eq!(state.strips.len(), 7);
        assert!(state.apps_dialog.is_some() && state.diagnostics.is_some());
    }

    /// The "Journal" tab must show what the log buffer holds — this is the only
    /// place a desktop-launched app can read its own log, so a tab that paints
    /// nothing would be a silent regression of exactly the feature it exists for.
    #[test]
    fn the_journal_tab_shows_the_buffered_log_lines() {
        use crate::diagnostics::DiagTab;

        /// Every text an egui pass produced (recursing into shape groups).
        fn texts(shape: &egui::epaint::Shape, out: &mut Vec<String>) {
            match shape {
                egui::epaint::Shape::Text(text) => out.push(text.galley.text().to_string()),
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        texts(shape, out);
                    }
                }
                _ => {}
            }
        }

        // The log buffer is process-wide: hold its test guard so no other test
        // clears it while this one paints.
        let _guard = crate::logbuf::test_lock();
        let ctx = crate::test_support::headless_context();
        let mut state = AppState::demo();
        crate::logbuf::clear();
        crate::logbuf::push("12:00:00 INFO  holaf_ui: a journal line".to_string());
        state.apply(UiAction::OpenDiagnostics);
        if let Some(diagnostics) = &mut state.diagnostics {
            diagnostics.tab = DiagTab::Log;
        }
        assert_eq!(DiagTab::Log.title(), "Journal");

        let mut painted: Vec<String> = Vec::new();
        // Two passes: a new `Window` is measured in the first and painted in the
        // second (see `dialogs_paint_headless`).
        for time in [0.0, 1.0] {
            painted.clear();
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 600.0),
                )),
                time: Some(time),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| draw(&mut state, ui));
            output.textures_delta.clear();
            for clipped in &output.shapes {
                texts(&clipped.shape, &mut painted);
            }
        }
        assert!(
            painted.iter().any(|text| text.contains("a journal line")),
            "the Journal tab did not paint the buffered line: {painted:?}"
        );
        // The tab does not touch the model (no strip vanished, the window stays
        // open) — like every other tab.
        assert_eq!(state.strips.len(), 7);
        assert!(state.diagnostics.is_some());
        crate::logbuf::clear();
    }

    /// The diagnostics window paints every tab (the report of a *degraded*
    /// engine, so the Summary tab has something to show).
    #[test]
    fn the_diagnostics_window_paints_every_tab() {
        use crate::diagnostics::DiagTab;

        let ctx = crate::test_support::headless_context();
        let mut state = AppState::demo();
        let status = serde_json::json!({
            "running": true,
            "degraded": true,
            "pending": [{"key": "strip:x", "message": "device not found"}],
            "mainloop_pumps": 0,
            "fx_host": {"mode": "inprocess", "pid": 1, "respawns": 4},
            "holaf_nodes": ["Holaf_Bus_Mic"],
            "strips": [
                {"uid": "in-mic", "label": "Mic", "node_name": "Holaf_Bus_Mic",
                 "device_name": null, "volume": 0.5, "effects": {}}
            ]
        });
        state.backend_status = Some(status);
        state.apply(UiAction::OpenDiagnostics);
        let report = &state.diagnostics.as_ref().unwrap().report;
        assert!(report.errors() > 0);
        for tab in DiagTab::ALL {
            if let Some(diagnostics) = &mut state.diagnostics {
                diagnostics.tab = tab;
            }
            let _ = crate::test_support::run_pass(&ctx, egui::vec2(800.0, 600.0), 1.0, |ui| {
                draw(&mut state, ui)
            });
            let primitives =
                crate::test_support::run_pass(&ctx, egui::vec2(800.0, 600.0), 2.0, |ui| {
                    draw(&mut state, ui)
                });
            assert!(!primitives.is_empty(), "{tab:?} painted nothing");
        }
        assert!(state.diagnostics.is_some(), "drawing must not close it");
    }
}
