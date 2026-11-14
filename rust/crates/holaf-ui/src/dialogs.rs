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

/// Device binding dialog (the Qt combo, expanded): every sink for an output,
/// every source for an input, plus "Virtual Bus" (= unbind).
#[derive(Debug, Clone, PartialEq)]
pub struct DevicePicker {
    pub uid: String,
    pub kind: StripType,
    pub current: Option<String>,
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

/// The diagnostics window ("Debug window"): four tabs, *Refresh* and *Copy
/// report*.
///
/// The report itself is built by [`crate::diagnostics`] (pure); this function
/// only paints it.  "Copy report" uses `ctx.copy_text`, which is what makes the
/// window useful in a bug thread — the text has the findings first.
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
        ui.ctx().copy_text(text);
        state.status = Some("diagnostic report copied to the clipboard".to_string());
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

fn draw_device_picker(state: &mut AppState, ui: &mut Ui) {
    let Some(dialog) = state.device_picker.clone() else {
        return;
    };
    let mut open = true;
    let mut cancel_clicked = false;
    let mut chosen: Option<Option<String>> = None;
    let devices: Vec<crate::engine_json::DeviceInfo> = state
        .engine_devices
        .iter()
        .filter(|device| dialog.accepts(device))
        .cloned()
        .collect();
    egui::Window::new(match dialog.kind {
        StripType::Input => "Input device",
        StripType::Output => "Output device",
    })
    .id(egui::Id::unique((
        "holaf-device-picker",
        dialog.uid.as_str(),
    )))
    .collapsible(false)
    .resizable(false)
    .default_width(320.0)
    .open(&mut open)
    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
    .show(ui.ctx(), |ui| {
        if ui
            .selectable_label(dialog.current.is_none(), "Virtual Bus (no device)")
            .clicked()
        {
            chosen = Some(None);
        }
        ui.separator();
        if devices.is_empty() {
            let reason = state
                .devices_error
                .clone()
                .unwrap_or_else(|| "the engine reported no device".to_string());
            ui.label(egui::RichText::new(reason).size(10.0).weak());
        }
        for device in &devices {
            let label = format!("{}  ({})", device.display(), device.name);
            let selected = dialog.current.as_deref() == Some(device.name.as_str());
            if ui.selectable_label(selected, label).clicked() {
                chosen = Some(Some(device.name.clone()));
            }
        }
        ui.separator();
        if ui.button("Cancel").clicked() {
            cancel_clicked = true;
        }
    });
    if let Some(device) = chosen {
        state.apply(UiAction::DeviceChanged {
            uid: dialog.uid,
            device,
        });
        state.device_picker = None;
    } else if !open || cancel_clicked {
        state.device_picker = None;
    }
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
        };
        assert!(out.accepts(&sink) && !out.accepts(&source));
        let input = DevicePicker {
            uid: "in-1".into(),
            kind: StripType::Input,
            current: None,
        };
        assert!(input.accepts(&source) && !input.accepts(&sink));
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
