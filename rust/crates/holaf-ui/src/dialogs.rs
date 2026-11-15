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
    /// The text the user sees, on **one** line (`PreSonus Studio 26c ·
    /// pro-output-0`).  The pop-up never wraps it: one device = one row.
    pub label: String,
    /// The full `node.name` this row stands for — the hover text, so an elided
    /// label can always be resolved to the real node (empty for the "no device"
    /// entry).
    pub detail: String,
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
    /// Pure on purpose: the filtering (`accepts`), the labels and the ordering
    /// are what a reviewer wants to check, and they are testable without painting
    /// a single pixel.
    ///
    /// The label is [`crate::devices::readable_label`] — the *model* of the card
    /// (`PreSonus Studio 26c · pro-output-0`), never the bare profile the old
    /// rule kept (`analog-stereo` read the same for the C-Media **and** the
    /// built-in card).  When two devices of one family produce the same label
    /// (two identical USB interfaces with different serials) the instance
    /// segment is appended, so no two rows are ever identical: picking the wrong
    /// speakers is worse than a slightly longer label.
    pub fn entries(&self, devices: &[crate::engine_json::DeviceInfo]) -> Vec<DeviceChoice> {
        let accepted: Vec<&crate::engine_json::DeviceInfo> =
            devices.iter().filter(|d| self.accepts(d)).collect();
        let labels = distinct_labels(&accepted);
        let mut entries = vec![DeviceChoice {
            label: self.unbind_label().to_string(),
            detail: String::new(),
            device: None,
        }];
        entries.extend(
            accepted
                .iter()
                .zip(labels)
                .map(|(device, label)| DeviceChoice {
                    label,
                    detail: device.name.clone(),
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

/// Readable labels for a device list, guaranteed to be **unique**: a label that
/// appears twice gets its instance segment appended (`PreSonus Studio 26c ·
/// analog-stereo (usb-PreSonus_Studio_26c_01424548-00)`).
///
/// Two identical USB interfaces with different serials produce the same model
/// label; a list where both rows read the same is exactly the unusable menu the
/// user reported, with the extra hazard that one of the two cards is the wrong
/// choice.
pub fn distinct_labels(devices: &[&crate::engine_json::DeviceInfo]) -> Vec<String> {
    let labels: Vec<String> = devices
        .iter()
        .map(|device| device.label())
        .collect();
    labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let duplicated = labels
                .iter()
                .enumerate()
                .any(|(other, candidate)| other != index && candidate == label);
            if !duplicated {
                return label.clone();
            }
            let parts = crate::devices::name_parts(&devices[index].name);
            match parts.instance {
                Some(instance) => format!("{label}  ({instance})"),
                None => format!("{label}  ({})", devices[index].name),
            }
        })
        .collect()
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
                        // What the VU bars draw right now — the Python dialog
                        // printed “L=0.030 R=0.029” per strip, and without it a
                        // “VU stuck at full scale” report cannot be read (a real
                        // level and a capture that never arrived look identical).
                        if !report.meter_levels.is_empty() {
                            ui.separator();
                            ui.label(format!("meter levels ({})", report.meter_levels.len()));
                            for (uid, label, [left, right]) in &report.meter_levels {
                                let colour = if *left >= 0.999 && *right >= 0.999 {
                                    egui::Color32::from_rgb(0xd0, 0x60, 0x60)
                                } else {
                                    ui.visuals().text_color()
                                };
                                let source =
                                    if report.meter_missing.iter().any(|missing| missing == uid) {
                                        "  (no capture source)"
                                    } else {
                                        ""
                                    };
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{label:<20} L={left:.3} R={right:.3}{source}"
                                    ))
                                    .color(colour),
                                );
                            }
                            // The scale, so 0.73 is readable as “−12 dBFS RMS”.
                            ui.label(
                                egui::RichText::new(format!(
                                    "scale: {:.0} dBFS → 0.00, 0 dBFS → {:.2}, \
                                     +{:.0} dBFS → 1.00",
                                    crate::meters::LEVEL_FLOOR_DB,
                                    crate::meters::level_from_amplitude(1.0),
                                    crate::meters::OVERLOAD_HEADROOM_DB
                                ))
                                .size(10.0)
                                .weak(),
                            );
                        }
                        ui.separator();
                        // CAPTURES — the VU chain's own health, i.e. the answer to
                        // “the bars read 0.000, why?”.  Same content as the
                        // report's CAPTURES section: one block per capture, with
                        // the pid, the state, the block/reject counts and the
                        // `pw-cat` stderr tail.
                        ui.label(format!(
                            "captures — recorder: {}",
                            report.recorder.summary()
                        ));
                        if report.captures.is_empty() {
                            ui.label(
                                egui::RichText::new(
                                    "(no capture is running - no source was requested)",
                                )
                                .weak(),
                            );
                        } else {
                            let rejects = if report.captures.rejects.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    " ({})",
                                    report
                                        .captures
                                        .rejects
                                        .iter()
                                        .map(|(reason, count)| format!("{count} {reason}"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                            };
                            let colour = if report.captures.never_streamed() {
                                egui::Color32::from_rgb(0xd0, 0x60, 0x60)
                            } else {
                                ui.visuals().text_color()
                            };
                            ui.label(
                                egui::RichText::new(format!(
                                    "blocks: {} accepted, {} rejected{rejects}",
                                    report.captures.blocks, report.captures.rejected
                                ))
                                .color(colour),
                            );
                            for facts in &report.captures.sources {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} → {} · {}",
                                        facts.source,
                                        facts.target,
                                        facts.describe()
                                    ))
                                    .size(10.0)
                                    .color(if facts.dead() {
                                        egui::Color32::from_rgb(0xd0, 0x60, 0x60)
                                    } else {
                                        ui.visuals().text_color()
                                    }),
                                );
                                for line in &facts.stderr_tail {
                                    ui.label(
                                        egui::RichText::new(format!("    stderr: {line}"))
                                            .size(9.0)
                                            .weak(),
                                    );
                                }
                            }
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

/// Device pop-up geometry.  The list is a **vertical** list of one-line rows:
/// it must be wide enough for the device names and tall enough to show several
/// of them at once.  Measured on the user's report: the old pop-up was
/// 240 × ~70 px for the whole list, so every name was cut mid-word and two
/// devices fit — “le menu de selection est tout petit”.
mod picker_geometry {
    /// Smallest pop-up width: a device name (`PreSonus Studio 26c ·
    /// pro-output-0`) needs roughly this much at 13 px.
    pub const MIN_WIDTH: f32 = 380.0;
    /// Widest the pop-up may get; beyond that the label is elided and the full
    /// `node.name` is in the tooltip.
    pub const MAX_WIDTH: f32 = 560.0;
    /// A pop-up with two or three devices still gets a breathing height (no
    /// scroll, but not a postage stamp either).
    pub const MIN_HEIGHT: f32 = 240.0;
    /// Tallest the pop-up may get before the list scrolls.
    pub const MAX_HEIGHT: f32 = 420.0;
    /// Height of one row: one device = one line, never a wrapped paragraph.
    pub const ROW_HEIGHT: f32 = 24.0;
    /// Row label font size.
    pub const ROW_FONT: f32 = 13.0;
    /// Space the frame margins, the row inset and the scrollbar take.
    pub const CHROME_WIDTH: f32 = 30.0;
    /// Frame margins (4 px top + 4 px bottom) plus the breathing room a short
    /// list gets on top of its rows.
    pub const CHROME_HEIGHT: f32 = 24.0;
    /// Space the pop-up keeps from the screen edge.
    pub const SCREEN_MARGIN: f32 = 4.0;
    /// Gap between the combo and the pop-up.
    pub const ANCHOR_GAP: f32 = 2.0;
}

/// Where the device pop-up opens and how big it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupGeometry {
    pub pos: egui::Pos2,
    pub size: egui::Vec2,
}

/// Size and position of the device pop-up — pure, so the geometry is unit-tested
/// without painting a pixel.
///
/// * **width** — as wide as the widest row (`widest_label` is already measured
///   with egui's font layout), clamped to 380–560 px, never narrower than the
///   combo that opened it;
/// * **height** — one row per device, floored at 240 px (a two-device list must
///   not be a postage stamp) and capped at 420 px, past which the list scrolls;
/// * **position** — under the combo, flipped above it when that side has more
///   room (a strip near the bottom of the window).
///
/// `anchor` is the combo's rect (published by `views::strip`); with no anchor the
/// pop-up opens near the top-left of the window so a headless draw still shows
/// something sane.
pub fn popup_geometry(
    anchor: Option<egui::Rect>,
    screen: egui::Rect,
    widest_label: f32,
    rows: usize,
) -> PopupGeometry {
    use picker_geometry as geo;

    let width = (widest_label + geo::CHROME_WIDTH)
        .clamp(geo::MIN_WIDTH, geo::MAX_WIDTH)
        .max(anchor.map_or(0.0, |rect| rect.width().min(geo::MAX_WIDTH)));

    let rows = rows.max(1) as f32;
    let content_height = rows * geo::ROW_HEIGHT + (rows - 1.0) * 2.0 + geo::CHROME_HEIGHT;
    let wanted_height = content_height.clamp(geo::MIN_HEIGHT, geo::MAX_HEIGHT);

    let Some(rect) = anchor else {
        return PopupGeometry {
            pos: egui::pos2(screen.left() + 20.0, screen.top() + 60.0),
            size: egui::vec2(width, wanted_height),
        };
    };
    let below = (screen.bottom() - rect.bottom() - geo::ANCHOR_GAP - geo::SCREEN_MARGIN).max(0.0);
    let above = (rect.top() - screen.top() - geo::ANCHOR_GAP - geo::SCREEN_MARGIN).max(0.0);
    // Drop below the combo; flip above it only when the list cannot fit below
    // and there is more room up there (the last strips of the window).
    let can_fit_below = below >= wanted_height.min(geo::MIN_HEIGHT);
    let use_above = !can_fit_below && above > below;
    let room = if use_above { above } else { below };
    // Never taller than the screen allows: with neither side able to hold the
    // list, it takes the roomier side and scrolls.
    let height = wanted_height.min(room).max(geo::ROW_HEIGHT);
    let pos = if use_above {
        egui::pos2(rect.left(), rect.top() - geo::ANCHOR_GAP - height)
    } else {
        egui::pos2(rect.left(), rect.bottom() + geo::ANCHOR_GAP)
    };
    PopupGeometry {
        pos,
        size: egui::vec2(width, height),
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
/// Geometry (see [`picker_geometry`]): the pop-up is sized from its **content**
/// — as wide as the longest label (clamped to 380–560 px), as tall as the rows
/// need (clamped to 240–420 px, scrolling past that) — and it drops **below**
/// the combo, flipping above it when there is more room there.  It used to be
/// 240 px wide and content-height, which is what made the device names break
/// mid-word on the user's screen.
///
/// Every row is one line: the label is elided with `…` inside the row, and the
/// full `node.name` is the hover text.  No wrapping, ever — a wrapped row makes
/// the list unreadable (and it looks like two devices).
///
/// Dismissal mirrors [`crate::menus::draw`]: a click outside, Escape, or a
/// selection.  It is deliberately **non-modal** — nothing is dimmed, the strips
/// stay usable, and the choice costs one click instead of three.
fn draw_device_picker(state: &mut AppState, ui: &mut Ui) {
    use picker_geometry as geo;

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

    // --- width: as wide as the widest row, within bounds ----------------
    // Measured with egui's own font layout (exact, not an estimate) so a label
    // is only ever elided when the pop-up would otherwise leave the screen.
    let font = egui::FontId::proportional(geo::ROW_FONT);
    let painter = ui.painter().clone();
    let widest = entries
        .iter()
        .map(|choice| choice.label.clone())
        .chain(std::iter::once(empty_hint.clone()))
        .map(|text| {
            painter
                .layout_no_wrap(text, font.clone(), egui::Color32::WHITE)
                .size()
                .x
        })
        .fold(0.0_f32, f32::max);

    // --- geometry: content-sized, floored, flipped above when needed -----
    let geometry = popup_geometry(anchor, screen, widest, entries.len());
    let pos = geometry.pos;
    let width = geometry.size.x;
    let height = geometry.size.y;

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
                ui.set_max_width(width);
                ui.set_min_height(height - 8.0);
                ui.set_max_height(height - 8.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                // One device = one line.  `Truncate` keeps a long label from
                // wrapping into a paragraph that reads like several devices.
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                let row_width = width - 8.0;
                egui::ScrollArea::vertical()
                    .max_height(height - 8.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for choice in &entries {
                            let selected = dialog.is_current(choice);
                            let row = egui::Button::selectable(
                                selected,
                                egui::RichText::new(choice.label.clone()).size(geo::ROW_FONT),
                            );
                            let response = ui.add_sized([row_width, geo::ROW_HEIGHT], row);
                            // The full node name, one hover away: the label is a
                            // shortened form by construction.
                            let response = if choice.detail.is_empty() {
                                response
                            } else {
                                response.on_hover_text(choice.detail.clone())
                            };
                            if response.clicked() {
                                chosen = Some(choice.device.clone());
                            }
                        }
                        if !devices_listed {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(empty_hint.clone()).size(11.0).weak(),
                                )
                                .wrap(),
                            );
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

    /// The labels of the pop-up are the user's own devices: the model of the
    /// card, never the bare profile the old rule kept (`analog-stereo` read the
    /// same for the C-Media **and** the built-in card).
    #[test]
    fn the_popup_labels_name_the_card_not_its_profile() {
        let devices = vec![
            sink(
                "alsa_output.usb-PreSonus_Studio_26c_01424548-00.pro-output-0",
                None,
            ),
            sink(
                "alsa_output.usb-C-Media_Electronics_Inc._USB_Audio_Device-00.analog-stereo",
                None,
            ),
            sink("alsa_output.pci-0000_00_1f.3.analog-stereo", None),
        ];
        let picker = DevicePicker {
            uid: "out-1".into(),
            kind: StripType::Output,
            current: None,
            opened_at: 0.0,
        };
        let entries = picker.entries(&devices);
        assert_eq!(
            entries[1].label, "PreSonus Studio 26c · pro-output-0",
            "{entries:?}"
        );
        assert_eq!(
            entries[2].label,
            "C-Media Electronics Inc. USB Audio Device · analog-stereo"
        );
        assert_eq!(entries[3].label, "alsa_output · analog-stereo");
        // One row = one device: no label is a wrapped paragraph and none carries
        // the raw node name inline any more (that is the tooltip's job).
        for entry in &entries[1..] {
            assert!(!entry.label.contains('\n'), "{entry:?}");
            assert!(!entry.label.contains("usb-PreSonus"), "{entry:?}");
        }
        // The full node name — what a hover shows — is exact for every row.
        assert_eq!(
            entries[1].detail,
            "alsa_output.usb-PreSonus_Studio_26c_01424548-00.pro-output-0"
        );
        assert!(entries[0].detail.is_empty(), "the unbind row has no node");
        // No two rows ever read the same.
        let unique: std::collections::HashSet<&String> =
            entries.iter().map(|entry| &entry.label).collect();
        assert_eq!(unique.len(), entries.len(), "{entries:?}");
    }

    /// Two identical USB interfaces (different serials) produce the same model
    /// label; the instance segment is appended so one can still be told from the
    /// other — picking the wrong speakers is worse than a longer label.
    #[test]
    fn two_identical_models_are_still_distinguishable() {
        let devices = vec![
            sink("alsa_output.usb-Kingston_2.analog-stereo", None),
            sink("alsa_output.usb-Kingston_2_other_serial.analog-stereo", None),
        ];
        let picker = DevicePicker {
            uid: "out-1".into(),
            kind: StripType::Output,
            current: None,
            opened_at: 0.0,
        };
        let entries = picker.entries(&devices);
        assert_ne!(entries[1].label, entries[2].label, "{entries:?}");
        assert!(entries[1].label.contains("Kingston"), "{entries:?}");
        assert!(!entries[1].label.contains('\n'));
    }

    /// The pop-up is a **vertical list**: wide enough for the names, tall enough
    /// for several devices, capped with a scroll, and it drops below the combo
    /// (flipping above it at the bottom of the window).
    #[test]
    fn the_device_popup_is_sized_for_a_vertical_list() {
        use picker_geometry as geo;
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 600.0));
        let combo = egui::Rect::from_min_size(egui::pos2(120.0, 60.0), egui::vec2(90.0, 18.0));

        // 3 devices with short names: still a generous pop-up (380 × 240), under
        // the combo.
        let three = popup_geometry(Some(combo), screen, 120.0, 3);
        assert_eq!(three.size, egui::vec2(geo::MIN_WIDTH, geo::MIN_HEIGHT));
        assert_eq!(three.pos, egui::pos2(combo.left(), combo.bottom() + 2.0));

        // A long name widens it, up to the cap.
        let wide = popup_geometry(Some(combo), screen, 300.0, 3);
        assert_eq!(wide.size.x, 330.0_f32.max(geo::MIN_WIDTH));
        let absurd = popup_geometry(Some(combo), screen, 1200.0, 3);
        assert_eq!(absurd.size.x, geo::MAX_WIDTH, "the width is capped");

        // 10 devices: the list grows with the content and does not scroll yet.
        let ten = popup_geometry(Some(combo), screen, 300.0, 10);
        assert_eq!(
            ten.size.y,
            10.0 * geo::ROW_HEIGHT + 9.0 * 2.0 + geo::CHROME_HEIGHT
        );
        assert!(ten.size.y > geo::MIN_HEIGHT && ten.size.y < geo::MAX_HEIGHT);

        // 40 devices: capped, the rest is a scroll.
        let many = popup_geometry(Some(combo), screen, 300.0, 40);
        assert_eq!(many.size.y, geo::MAX_HEIGHT);

        // A combo at the bottom of the window flips the list above it.
        let low = egui::Rect::from_min_size(egui::pos2(120.0, 560.0), egui::vec2(90.0, 18.0));
        let flipped = popup_geometry(Some(low), screen, 300.0, 10);
        assert!(
            flipped.pos.y + flipped.size.y <= low.top() + 0.5,
            "{flipped:?} vs {low:?}"
        );

        // No anchor (drawn from code): it opens near the top-left, never at (0,0).
        let floating = popup_geometry(None, screen, 300.0, 3);
        assert_eq!(floating.pos, egui::pos2(20.0, 60.0));
        assert_eq!(floating.size.x, geo::MIN_WIDTH);
    }

    /// The **rendered** pop-up is exactly what the geometry says — measured on
    /// egui's own area rect, for the two lists the user asked about (3 devices and
    /// 10 devices), so a drawing regression (a lost `set_min_height`, a scroll
    /// area that shrinks) cannot hide behind a green model test.
    #[test]
    fn the_rendered_popup_matches_the_computed_geometry() {
        use picker_geometry as geo;
        let devices = |count: usize| -> Vec<DeviceInfo> {
            (0..count)
                .map(|index| {
                    sink(
                        &format!("alsa_output.usb-Holaf_Test_Interface_{index:02}-00.analog-stereo"),
                        None,
                    )
                })
                .collect()
        };
        let run = |count: usize| -> egui::Rect {
            let ctx = crate::test_support::headless_context();
            let mut state = AppState::demo();
            state.engine_devices = devices(count);
            let combo = egui::Rect::from_min_size(egui::pos2(60.0, 40.0), egui::vec2(90.0, 18.0));
            ctx.data_mut(|data| {
                data.insert_temp(crate::views::strip::device_combo_id("out-mix"), combo)
            });
            state.apply(UiAction::OpenDevicePicker {
                uid: "out-mix".into(),
            });
            for time in [0.0, 1.0] {
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
            }
            assert!(state.device_picker.is_some(), "the picker must stay open");
            let id = egui::Id::unique(("holaf-device-popup", "out-mix"));
            ctx.memory(|memory| memory.area_rect(id))
                .expect("the pop-up is an Area: it has a rect")
        };

        let three = run(3);
        assert!(
            (geo::MIN_WIDTH..=geo::MAX_WIDTH).contains(&three.width()),
            "3 devices: {} wide (between {} and {})",
            three.width(),
            geo::MIN_WIDTH,
            geo::MAX_WIDTH
        );
        assert!(
            (three.height() - geo::MIN_HEIGHT).abs() <= 2.0,
            "3 devices: {} tall (a short list still breathes)",
            three.height()
        );

        let ten = run(10);
        // 10 devices + the "no device" row.
        let expected = 11.0 * geo::ROW_HEIGHT + 10.0 * 2.0 + geo::CHROME_HEIGHT;
        assert!(
            (ten.height() - expected).abs() <= 4.0,
            "10 devices: {} tall, expected {expected}",
            ten.height()
        );
        assert!(ten.width() >= three.width(), "the list never gets narrower");
        assert!(ten.height() > three.height(), "a longer list is taller");
    }

    /// **A real click on a row binds that device.**  The rows are `add_sized`
    /// buttons (full-width highlight, one line each), so the hit area and the
    /// action are worth a pointer test: an unclickable list would look right in
    /// every snapshot and be useless.
    #[test]
    fn clicking_a_row_binds_that_device() {
        let ctx = crate::test_support::headless_context();
        let mut state = AppState::demo();
        state.engine_devices = vec![
            sink(
                "alsa_output.usb-PreSonus_Studio_26c_01424548-00.pro-output-0",
                None,
            ),
            sink("alsa_output.pci-0000_00_1f.3.analog-stereo", Some("Built-in")),
        ];
        let combo = egui::Rect::from_min_size(egui::pos2(60.0, 40.0), egui::vec2(90.0, 18.0));
        ctx.data_mut(|data| {
            data.insert_temp(crate::views::strip::device_combo_id("out-mix"), combo)
        });
        state.time = 5.0;
        state.apply(UiAction::OpenDevicePicker {
            uid: "out-mix".into(),
        });
        let run = |state: &mut AppState, time: f64, events: Vec<egui::Event>| {
            state.time = time as f32;
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 600.0),
                )),
                time: Some(time),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| draw(state, ui));
            output.textures_delta.clear();
        };
        // Two passes so the area exists, then aim at the **second** row (the
        // first device; row 0 is the "no device" entry).
        run(&mut state, 5.0, Vec::new());
        run(&mut state, 5.1, Vec::new());
        let area = ctx
            .memory(|memory| {
                memory.area_rect(egui::Id::unique(("holaf-device-popup", "out-mix")))
            })
            .expect("the pop-up has a rect");
        let row = egui::pos2(
            area.left() + area.width() / 2.0,
            area.top() + 4.0 + picker_geometry::ROW_HEIGHT + 2.0 + picker_geometry::ROW_HEIGHT / 2.0,
        );
        let button = |pressed: bool| egui::Event::PointerButton {
            pos: row,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        run(&mut state, 5.2, vec![egui::Event::PointerMoved(row)]);
        run(&mut state, 5.3, vec![button(true)]);
        run(&mut state, 5.4, vec![button(false)]);
        assert!(state.device_picker.is_none(), "a click must close the pop-up");
        let strip = state
            .strips
            .iter()
            .find(|strip| strip.uid == "out-mix")
            .expect("the strip is still there");
        assert_eq!(
            strip.device_name.as_deref(),
            Some("alsa_output.usb-PreSonus_Studio_26c_01424548-00.pro-output-0"),
            "clicking a row must bind the device of that row"
        );
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
