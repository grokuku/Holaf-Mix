//! One strip — the Rust twin of `src/ui/widgets/strip_widget.py`.
//!
//! Vertical order, exactly as in Qt:
//!
//! ```text
//! ┌──────────────────────────────┐  ← rounded body, kind gradient + border
//! │  Discorde                 ×  │  header: label (bold 11) + delete glyph
//! │         SOURCE IN            │  caption: tiny 8, #aaa
//! │  ┌────────────────────────┐  │  device combo (`QComboBox`, 18 px)
//! │  └────────────────────────┘  │
//! │  [x] Default Sink            │  inputs only: default-sink checkbox
//! │  [   SELECT APPS   ]         │  inputs only — the ROW is reserved on every
//! │                              │  input, the button is drawn on a virtual,
//! │                              │  non-default one (Qt hid it, kept the space)
//! │  [GT][RN][EQ][TB][CP]        │  FX toggles: 5 inputs / 3 outputs
//! │  ╭────────────────────────╮  │
//! │  │        [CASQ]          │  │  routing chips (inputs) — 20 px each
//! │  │        [HP  ]          │  │
//! │  ╰────────────────────────╯  │
//! │  ▮ │      │ ▮                │  VU left / fader / VU right
//! │  ▮ │      │ ▮                │
//! │  [MONO][ MUTE ]              │  two-state buttons
//! │  [        MIDI       ]       │  MIDI learn
//! └──────────────────────────────┘
//! ```
//!
//! Vertical sizing: every row above the fader and every row below it has a
//! **fixed** height from the theme; the **fader** is the only elastic element
//! and takes whatever the strip has left (see [`crate::layout::fader_height_for`]).
//! A taller window therefore means a longer fader — the rows never move or grow.
//!
//! Deviations from the Qt layout, and why:
//!
//! * **FX button width.** Qt gives each FX button `setFixedSize(22, 18)`
//!   *inside* a 100 px strip with 5 buttons — 118 px of buttons in a 90 px
//!   content box, so the real app clips the last one.  The Rust port shrinks
//!   them to fit (never below 14 px) instead of clipping: same visual row, no
//!   hidden control.
//! * **Rename.** Qt opens a `QInputDialog` on double click; here the view emits
//!   [`UiAction::BeginRename`] and the main window draws the dialog.
//! * **FX settings.** Qt opens a `customContextMenu` on right click whose first
//!   entry opens the settings dialog; the Rust view reproduces that (right click
//!   → [`crate::menus`], first entry "Effect settings") and *also* accepts a
//!   double click, which is the shortcut the old port had.
//!   Known quirk: the first click of a double click also toggles the effect (egui
//!   reports it as a click on the previous frame) — right click is the clean way.
//! * **Delete.** The `×` only *requests* the deletion ([`UiAction::RequestDelete`]);
//!   the confirmation dialog decides (`Delete`).
//! * **Device combo.** Qt fills it from `pipewire_utils.get_audio_nodes()`;
//!   the device list arrives with the engine bridge, so the combo displays the
//!   current device and emits [`UiAction::OpenDevicePicker`] on click, which
//!   unfolds the list **under the button** (`dialogs::draw_device_picker`, a
//!   non-modal pop-up since migration B3 — it used to be a centred dialog).  The
//!   button's rect is published to egui memory ([`device_combo_id`]) so the
//!   pop-up can anchor to it.  When the stored device is absent from the engine's
//!   list the combo shows `<name> (Not Found)`, exactly like `set_device_list` in
//!   Qt.

use egui::{Rect, Sense, Ui, UiBuilder, Vec2};

use holaf_core::model::{EffectKey, Strip, StripType};

use crate::app::UiAction;
use crate::devices::DevicePresence;
use crate::layout::{row_gap, CONTROL_ROW_HEIGHT, MIN_FADER_HEIGHT};
use crate::theme::images::ImageCache;
use crate::theme::paint;
use crate::theme::resolve::{resolve, StripDescriptor, StripKind};
use crate::theme::ThemeFile;
use crate::views::text_centered;
use crate::widgets::{
    combo_display, fader_with_vu, flat_button_in, fx_button_in, midi_button_in, route_button_in,
    state_button_in, FaderStyle, MidiButtonState,
};

/// Short label of an FX button (the Qt code uses `GT`/`RN`/`EQ`/`TB`/`CP`).
pub fn fx_label(effect: EffectKey) -> &'static str {
    match effect {
        EffectKey::Gate => "GT",
        EffectKey::NoiseCancel => "RN",
        EffectKey::Eq => "EQ",
        EffectKey::Tube => "TB",
        EffectKey::Compressor => "CP",
    }
}

/// Routing chip label: `out_strip.label[:4].upper()` from `set_routing_targets`.
pub fn route_label(label: &str) -> String {
    label.chars().take(4).collect::<String>().to_uppercase()
}

/// Draw one strip inside `rect` and return the actions the user asked for.
///
/// The caller applies the actions (`AppState::apply`) once every strip has been
/// drawn, so the paint pass always sees a stable model.  `outputs` is the list
/// of `(uid, label)` routing targets (empty for output strips).
#[allow(clippy::too_many_arguments)]
pub fn strip_ui(
    ui: &mut Ui,
    rect: Rect,
    theme_file: &ThemeFile,
    strip: &Strip,
    outputs: &[(String, String)],
    levels: [f32; 2],
    image_cache: &mut ImageCache,
    midi_button: MidiButtonState<'_>,
    device_presence: DevicePresence,
) -> Vec<UiAction> {
    let theme = &theme_file.theme;
    let midi_learning = midi_button.learning();
    // The learning flag is part of the descriptor, not a post-paint override:
    // `_update_base_style` paints the whole frame orange while learning.
    let style = resolve(
        theme_file,
        &StripDescriptor::from_strip(strip).with_learning(midi_learning),
    );
    let dims = &theme.dimensions;
    let mut actions = Vec::new();

    let painter = ui.painter().with_clip_rect(rect);

    // --- body: gradient + optional image + border -----------------------
    paint::fill_rect(&painter, rect, style.body, dims.corner_radius);
    if let Some(spec) = &style.image {
        if let Some((handle, size)) = image_cache.get(ui.ctx(), spec) {
            paint::paint_texture(
                &painter,
                rect,
                handle.id(),
                size,
                spec.fit,
                spec.tint,
                spec.opacity(),
            );
        }
    }
    paint::stroke_rect(&painter, rect, dims.corner_radius, style.border);

    // --- content -------------------------------------------------------
    let pad = dims.strip_padding;
    let content = Rect::from_min_max(
        egui::pos2(rect.left() + pad, rect.top() + pad),
        egui::pos2(
            rect.right() - pad,
            rect.bottom() - dims.strip_padding_bottom,
        ),
    );
    if content.width() <= 4.0 || content.height() <= 4.0 {
        return actions;
    }
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(content)
            .layout(egui::Layout::top_down(egui::Align::Center)),
    );
    // egui adds `item_spacing.y` (3 px by default) *after every allocation*,
    // which would silently turn the theme's 5 px row spacing into 8.  Zero it:
    // the gaps below are explicit (`add_space(row_gap(..))`), so the drawn
    // geometry is exactly the height model of `layout::strip_fixed_height`, and
    // the strip matches Qt's `layout.setSpacing(5)`.
    child.spacing_mut().item_spacing.y = 0.0;
    let avail_w = content.width();

    // 1. header: label + delete glyph
    let (header_rect, _) = child.allocate_exact_size(
        Vec2::new(avail_w, theme.typography.body + 6.0),
        Sense::hover(),
    );
    let delete_size = 16.0;
    let label_rect = Rect::from_min_max(
        header_rect.left_top(),
        egui::pos2(header_rect.right() - delete_size, header_rect.bottom()),
    );
    text_centered(
        &painter,
        label_rect,
        &strip.label,
        theme.typography.body,
        style.text_primary,
        theme.typography.bold_extra,
    );
    let delete_rect = Rect::from_center_size(
        egui::pos2(
            header_rect.right() - delete_size / 2.0,
            header_rect.center().y,
        ),
        Vec2::splat(delete_size),
    );
    let delete_response = child.interact(
        delete_rect,
        child.scope_id().with(("delete", strip.uid.as_str())),
        Sense::click(),
    );
    let delete_color = if delete_response.hovered() {
        theme.colors.delete_hover
    } else {
        theme.colors.delete
    };
    painter.text(
        delete_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::proportional(theme.typography.heading - 1.0),
        delete_color.color32(),
    );
    if delete_response.clicked() {
        // Qt asks for confirmation first (`QMessageBox.question`, default No):
        // the view requests it, `dialogs::draw_delete_confirm` asks, and only
        // the confirmed action mutates the model.
        actions.push(UiAction::RequestDelete {
            uid: strip.uid.clone(),
        });
    }
    let label_response = child.interact(
        label_rect,
        child.scope_id().with(("label", strip.uid.as_str())),
        Sense::click(),
    );
    if label_response.double_clicked() {
        actions.push(UiAction::BeginRename {
            uid: strip.uid.clone(),
        });
    }
    // Right click on the label: the strip's own menu (Rename / Delete + the
    // application entries) — the Qt `StripWidget` context menu.
    if label_response.secondary_clicked() {
        actions.push(UiAction::OpenContextMenu {
            kind: crate::menus::ContextMenuKind::Strip {
                uid: strip.uid.clone(),
                label: strip.label.clone(),
            },
            pos: pointer_pos(ui, label_rect),
        });
    }
    child.add_space(row_gap(theme));

    // 2. caption (SOURCE IN / DEVICE OUT / VIRTUAL BUS)
    let (caption_rect, _) = child.allocate_exact_size(
        Vec2::new(avail_w, theme.typography.tiny + 4.0),
        Sense::hover(),
    );
    let (caption, caption_color, caption_bold) = match style.kind {
        StripKind::Input => ("SOURCE IN", style.text_secondary, 0.0),
        StripKind::Output => ("DEVICE OUT", style.text_secondary, 0.0),
        // `_refresh_device_ui_state`: a bus gets a lavender, bold caption.
        StripKind::Bus => (
            "VIRTUAL BUS",
            theme.colors.bus_caption,
            theme.typography.bold_extra,
        ),
    };
    text_centered(
        &painter,
        caption_rect,
        caption,
        theme.typography.tiny,
        caption_color,
        caption_bold,
    );
    child.add_space(row_gap(theme));

    // 3. device combo
    let device_text = match strip.device_name.as_deref() {
        Some(name) => device_combo_text(name, device_presence),
        None => match style.kind {
            StripKind::Input => "Apps / Virtual".to_string(),
            _ => "Virtual Sink (Bus)".to_string(),
        },
    };
    let combo = combo_display(&mut child, &device_text, theme);
    // The combo can only show the profile segment of the node name (see
    // `device_display_name`): the tooltip carries the identity, so a user can
    // always tell which node a strip is bound to and see a missing device.
    let combo = match strip.device_name.as_deref().filter(|name| !name.is_empty()) {
        Some(name) => combo.on_hover_text(device_tooltip(name, device_presence)),
        None => combo,
    };
    // Where the button ended up, published for the device pop-up
    // (`dialogs::draw_device_picker`): the list unfolds *under the button*, like
    // the Qt `QComboBox` drop-down.  egui memory is the channel because views
    // never mutate the model (see `views/mod.rs`).
    ui.ctx()
        .data_mut(|data| data.insert_temp(device_combo_id(&strip.uid), combo.rect));
    if combo.clicked() {
        actions.push(UiAction::OpenDevicePicker {
            uid: strip.uid.clone(),
        });
    }
    child.add_space(row_gap(theme));

    // 4. inputs only: default sink + app selection
    //
    // `_update_app_btn_visibility`: the "SELECT APPS" button only *shows* on a
    // virtual, non-default input — but its row is reserved on **every** input.
    // Qt called `setVisible(false)` on a widget that stayed in the layout, so all
    // input strips kept the same height and their faders lined up; laying the row
    // out only when the button is drawn made the other strips 23 px taller (a
    // longer fader, a VU scale that starts higher).  Same height, always: the row
    // is allocated, the button is painted into it only when it applies.
    let apps_row =
        style.kind == StripKind::Input && strip.device_name.is_none() && !strip.is_default;
    if style.kind == StripKind::Input {
        let (check_rect, _) =
            child.allocate_exact_size(Vec2::new(avail_w, dims.checkbox_height), Sense::hover());
        let response = child.interact(
            check_rect,
            child.scope_id().with(("default", strip.uid.as_str())),
            Sense::click(),
        );
        let box_rect = Rect::from_center_size(
            egui::pos2(check_rect.left() + 5.0, check_rect.center().y),
            Vec2::splat(10.0),
        );
        painter.rect_stroke(
            box_rect,
            paint::corner(2.0),
            egui::Stroke::new(1.0, theme.colors.text_secondary.color32()),
            egui::StrokeKind::Inside,
        );
        if strip.is_default {
            painter.rect_filled(
                box_rect.shrink(2.0),
                paint::corner(1.0),
                theme.colors.text_primary.color32(),
            );
        }
        painter.text(
            egui::pos2(box_rect.right() + 4.0, check_rect.center().y),
            egui::Align2::LEFT_CENTER,
            "Default Sink",
            egui::FontId::proportional(theme.typography.small),
            theme.colors.checkbox_text.color32(),
        );
        if response.clicked() {
            actions.push(UiAction::DefaultSink {
                uid: strip.uid.clone(),
                is_default: !strip.is_default,
            });
        }
        child.add_space(row_gap(theme));

        // `_update_app_btn_visibility`: only a virtual, non-default input gets
        // the button — but the space is reserved for every input (see above).
        let (apps_rect, _) =
            child.allocate_exact_size(Vec2::new(avail_w, dims.combo_height), Sense::hover());
        if apps_row && flat_button_in(&child, "SELECT APPS", apps_rect, theme).clicked() {
            actions.push(UiAction::SelectApps {
                uid: strip.uid.clone(),
            });
        }
        child.add_space(row_gap(theme));
    }

    // 5. FX row
    let effects: Vec<EffectKey> = EffectKey::for_strip(strip.kind);
    let fx_spacing = 2.0;
    let fx_count = effects.len().max(1) as f32;
    let fx_width =
        ((avail_w - fx_spacing * (fx_count - 1.0)) / fx_count).clamp(14.0, dims.fx_button_width);
    let (fx_rect, _) =
        child.allocate_exact_size(Vec2::new(avail_w, dims.fx_button_height), Sense::hover());
    for (idx, effect) in effects.iter().enumerate() {
        let active = strip
            .effects
            .get(effect)
            .map(|state| state.active)
            .unwrap_or(false);
        let button_rect = Rect::from_min_size(
            egui::pos2(
                fx_rect.left() + idx as f32 * (fx_width + fx_spacing),
                fx_rect.top(),
            ),
            Vec2::new(fx_width, dims.fx_button_height),
        );
        let response = fx_button_in(
            &child,
            strip.uid.as_str(),
            button_rect,
            fx_label(*effect),
            active,
            theme,
        );
        // Left click toggles; a double click opens the settings dialog; a right
        // click opens the FX context menu, whose first entry is "Effect
        // settings" — the Qt shape (`effect_toggled` + `customContextMenu`).
        if response.secondary_clicked() {
            actions.push(UiAction::OpenContextMenu {
                kind: crate::menus::ContextMenuKind::Fx {
                    uid: strip.uid.clone(),
                    effect: *effect,
                },
                pos: pointer_pos(ui, button_rect),
            });
        } else if response.double_clicked() {
            actions.push(UiAction::OpenFxSettings {
                uid: strip.uid.clone(),
                effect: *effect,
            });
        } else if response.clicked() {
            actions.push(UiAction::ToggleFx {
                uid: strip.uid.clone(),
                effect: *effect,
                active: !active,
            });
        }
    }
    child.add_space(row_gap(theme));

    // 6. routing area (inputs) / BUS MASTER label (outputs)
    match strip.kind {
        StripType::Input => {
            let plate_h = crate::layout::routing_plate_height(theme, outputs.len());
            let (plate, _) = child.allocate_exact_size(Vec2::new(avail_w, plate_h), Sense::hover());
            painter.rect_filled(
                plate,
                paint::corner(3.0),
                theme.colors.routing_area.color32(),
            );
            let inner = plate.shrink(2.0);
            if outputs.is_empty() {
                text_centered(
                    &painter,
                    inner,
                    "No Outputs",
                    theme.typography.small,
                    theme.colors.text_secondary,
                    0.0,
                );
            }
            for (idx, (uid, label)) in outputs.iter().enumerate() {
                let y = inner.top() + idx as f32 * (dims.route_button_height + 2.0);
                let chip_rect = Rect::from_min_size(
                    egui::pos2(inner.left(), y),
                    Vec2::new(inner.width(), dims.route_button_height),
                );
                let active = strip.routes.iter().any(|r| r == uid);
                if route_button_in(
                    &child,
                    strip.uid.as_str(),
                    chip_rect,
                    &route_label(label),
                    active,
                    theme,
                )
                .clicked()
                {
                    actions.push(UiAction::Route {
                        uid: strip.uid.clone(),
                        target: uid.clone(),
                        active: !active,
                    });
                }
            }
        }
        StripType::Output => {
            let (master, _) = child.allocate_exact_size(
                Vec2::new(avail_w, crate::layout::BUS_MASTER_HEIGHT),
                Sense::hover(),
            );
            text_centered(
                &painter,
                master,
                "BUS MASTER",
                theme.typography.small,
                theme.colors.text_secondary,
                0.0,
            );
        }
    }
    child.add_space(row_gap(theme));

    // 7. fader + VU: the elastic row.
    //
    // Everything else in the strip has a fixed height, so this allocation is the
    // whole "strips fill the window" rule: whatever the rows above left, minus
    // the rows below (gap + MONO/MUTE + gap + MIDI), goes to the fader.  It is
    // floored at `MIN_FADER_HEIGHT` so a squeezed window still shows a usable
    // grab area — the rows below are then clipped by the strip's clip rect
    // instead of the fader collapsing to a sliver.
    let controls_h = CONTROL_ROW_HEIGHT;
    let midi_h = dims.midi_button_height;
    let fader_style = FaderStyle::from_theme(theme);
    let reserved = crate::layout::strip_rows_after_fader(theme);
    let fader_h = (child.available_height() - reserved).max(MIN_FADER_HEIGHT);
    // The measurement above is the truth (it cannot drift); make sure the height
    // *model* -- which the window floors and the docs are built on -- agrees with
    // it.  Debug builds (tests, snapshot runs) fail loudly if a row is added to
    // one and not to the other.
    #[cfg(debug_assertions)]
    {
        let modeled =
            crate::layout::fader_height_for(theme, strip.kind, outputs.len(), rect.height());
        debug_assert!(
            (fader_h - modeled).abs() < 0.5,
            "{}: drawn fader {fader_h} vs layout model {modeled}",
            strip.label
        );
    }
    let mut volume = strip.volume;
    let fader = fader_with_vu(
        &mut child,
        ("fader", strip.uid.as_str()),
        &mut volume,
        levels,
        &fader_style,
        fader_h,
    );
    if fader.changed {
        actions.push(UiAction::Volume {
            uid: strip.uid.clone(),
            volume,
        });
    }
    // Right click on the fader / VU: the strip's menu (`Rename`, `Delete`, then
    // the application entries) — the Qt `StripWidget` context menu, since the
    // fader is a child of the strip.  The fader itself only reacts to the
    // primary button (`widgets::fader`), so the volume never moves on the way.
    if fader.response.secondary_clicked() {
        actions.push(UiAction::OpenContextMenu {
            kind: crate::menus::ContextMenuKind::Strip {
                uid: strip.uid.clone(),
                label: strip.label.clone(),
            },
            pos: pointer_pos(ui, fader.response.rect),
        });
    }
    child.add_space(row_gap(theme));

    // 8. MONO / MUTE
    let (controls_rect, _) =
        child.allocate_exact_size(Vec2::new(avail_w, controls_h), Sense::hover());
    let mono_w = dims.control_button_width;
    let mono_rect = Rect::from_min_size(controls_rect.min, Vec2::new(mono_w, controls_h));
    let mute_rect = Rect::from_min_size(
        egui::pos2(controls_rect.left() + mono_w + 2.0, controls_rect.top()),
        Vec2::new((controls_rect.width() - mono_w - 2.0).max(20.0), controls_h),
    );
    if state_button_in(
        &child,
        strip.uid.as_str(),
        mono_rect,
        "MONO",
        "MONO",
        strip.is_mono,
        theme.colors.mono,
        theme.colors.mono_text_idle,
        theme,
    )
    .clicked()
    {
        actions.push(UiAction::Mono {
            uid: strip.uid.clone(),
            mono: !strip.is_mono,
        });
    }
    if state_button_in(
        &child,
        strip.uid.as_str(),
        mute_rect,
        "MUTE",
        "MUTED",
        strip.mute,
        theme.colors.mute,
        theme.colors.mute_text_idle,
        theme,
    )
    .clicked()
    {
        actions.push(UiAction::Mute {
            uid: strip.uid.clone(),
            mute: !strip.mute,
        });
    }
    child.add_space(row_gap(theme));

    // 9. MIDI
    //
    // A click opens the strip's MIDI menu (`Learn Volume` / `Learn Mute` /
    // `Learn Mono`, `Clear Mappings`) — the Qt `_show_midi_menu`, which is where
    // a MIDI Mix owner assigns one fader/button per function.  Right-clicking
    // opens the same menu, which is what a user tries first.
    let (midi_rect, _) = child.allocate_exact_size(Vec2::new(avail_w, midi_h), Sense::hover());
    let midi_response = midi_button_in(&child, strip.uid.as_str(), midi_rect, midi_button, theme);
    if let Some(tooltip) = midi_button.tooltip {
        midi_response.clone().on_hover_text(tooltip);
    }
    if midi_response.clicked() || midi_response.secondary_clicked() {
        actions.push(UiAction::OpenContextMenu {
            kind: crate::menus::ContextMenuKind::Midi {
                uid: strip.uid.clone(),
                label: strip.label.clone(),
                armed: midi_button.armed,
            },
            pos: midi_response.rect.left_bottom(),
        });
    }

    actions
}

/// egui-memory key of a strip's device-combo rect.
///
/// The strip view writes it every frame (`strip_ui`) and the device pop-up reads
/// it (`dialogs::draw_device_picker`) so the list opens exactly under the button
/// that owns it, at any window size and for any strip count.
pub fn device_combo_id(uid: &str) -> egui::Id {
    egui::Id::unique(("holaf-device-combo", uid))
}

/// Shorten a PipeWire node name the way the Qt combo does (`desc[:15] +
/// "..."`), but on the most informative part:
///
/// * `alsa_output.pci-0000_00_1f.3.analog-stereo` → `analog-stereo` (the last
///   segment names the profile);
/// * `bluez_output.AC_80_0B_1F_22_33_44.a2dp-sink` → `a2dp-sink`;
/// * `bluez_output.AC_80_0B_1F_22_33_44.1` → `bluez_output` (a one-character
///   last segment says nothing, so fall back to the head);
/// * anything longer than 15 chars is truncated with an ellipsis.
pub fn device_display_name(device_name: &str) -> String {
    if device_name.is_empty() {
        return String::new();
    }
    let segments: Vec<&str> = device_name.split('.').collect();
    let candidate = match segments.last() {
        Some(last) if last.chars().count() >= 4 => *last,
        _ => segments.first().copied().unwrap_or(device_name),
    };
    if candidate.chars().count() <= 15 {
        candidate.to_string()
    } else {
        let mut short: String = candidate.chars().take(14).collect();
        short.push('…');
        short
    }
}

/// Where a context menu must open: under the pointer when egui knows it (a real
/// click), else at the bottom-left of the widget it belongs to (so a keyboard or
/// synthetic interaction still gets a usable position).
pub fn pointer_pos(ui: &Ui, fallback: Rect) -> egui::Pos2 {
    ui.ctx()
        .input(|input| input.pointer.interact_pos())
        .unwrap_or_else(|| fallback.left_bottom())
}

/// What the device combo says for a bound strip.
///
/// A device that the engine does not list is prefixed with `!` instead of the
/// Qt `<name> (Not Found)` suffix.  The reason is measurable: the combo is a
/// fixed 90 px box and the label is elided to 15 characters, so
/// `<full node name> (Not Found)` — which is always longer than 15 characters —
/// lost the **whole** marker and showed a meaningless fragment
/// (`alsa_output.go…`).  The strip looked bound while nothing was routed, and
/// the user had no way to see that the device was gone.  The marker now comes
/// first so it can never be elided; the exact node name is one hover away
/// (`views::strip` puts it in the tooltip).
///
/// The rest keeps the Qt `desc[:15] + "..."` rule, so a long node name cannot
/// widen the combo.
pub fn device_combo_text(device_name: &str, presence: DevicePresence) -> String {
    let mut text = match presence {
        DevicePresence::Missing => format!("! {}", device_display_name(device_name)),
        DevicePresence::Found | DevicePresence::Unknown => device_display_name(device_name),
    };
    if text.chars().count() > 15 {
        text = text.chars().take(14).collect();
        text.push('…');
    }
    text
}

/// The hover text of the device combo: the **exact** `node.name` a strip is
/// bound to, plus what the engine thinks of it.
///
/// The combo itself can only show ~15 characters (`device_display_name` keeps
/// the profile segment: `analog-stereo`, `pro-output-0`), which is ambiguous as
/// soon as two devices of the same family are present — the user could not tell
/// *which* node a strip was really on, nor notice that the stored name no
/// longer exists.
pub fn device_tooltip(device_name: &str, presence: DevicePresence) -> String {
    match presence {
        DevicePresence::Missing => format!(
            "{device_name}\n(device not found: nothing is routed to it, and its meter stays dark)"
        ),
        DevicePresence::Found => device_name.to_string(),
        DevicePresence::Unknown => format!("{device_name}\n(no device list from the engine yet)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;

    #[test]
    fn fx_labels_match_the_qt_buttons() {
        assert_eq!(fx_label(EffectKey::Gate), "GT");
        assert_eq!(fx_label(EffectKey::NoiseCancel), "RN");
        assert_eq!(fx_label(EffectKey::Eq), "EQ");
        assert_eq!(fx_label(EffectKey::Tube), "TB");
        assert_eq!(fx_label(EffectKey::Compressor), "CP");
    }

    #[test]
    fn route_labels_are_four_uppercase_chars() {
        assert_eq!(route_label("Casque"), "CASQ");
        assert_eq!(route_label("HP"), "HP");
        assert_eq!(route_label("Mix Out"), "MIX ");
    }

    #[test]
    fn device_names_are_shortened_to_the_last_segment() {
        assert_eq!(
            device_display_name("alsa_output.pci-0000_00_1f.3.analog-stereo"),
            "analog-stereo"
        );
        assert_eq!(
            device_display_name("bluez_output.AC_80_0B_1F_22_33_44.a2dp-sink"),
            "a2dp-sink"
        );
        // A useless one-character last segment falls back to the head.
        assert_eq!(
            device_display_name("bluez_output.AC_80_0B_1F_22_33_44.1"),
            "bluez_output"
        );
        assert_eq!(device_display_name(""), "");
        // Long single-segment names are truncated with an ellipsis.
        let long = device_display_name("a_very_long_device_name_without_dots");
        assert_eq!(long.chars().count(), 15);
        assert!(long.ends_with('…'), "{long}");
    }

    /// **The marker must survive the elision.**  The combo is 90 px wide and
    /// the label is cut at 15 characters, so the Qt wording
    /// `<name> (Not Found)` — always longer than that — lost the *whole*
    /// warning and showed `alsa_output.go…`: a strip whose device is gone
    /// looked perfectly bound while nothing was routed to it.
    #[test]
    fn a_missing_device_is_shown_as_not_found() {
        assert_eq!(
            device_combo_text("alsa_output.gone.analog-stereo", DevicePresence::Missing),
            "! analog-stereo"
        );
        assert_eq!(
            device_combo_text("alsa_output.gone.analog-stereo", DevicePresence::Found),
            "analog-stereo"
        );
        // Unknown (no engine answered) never claims the device is gone.
        assert_eq!(
            device_combo_text("alsa_output.gone.analog-stereo", DevicePresence::Unknown),
            "analog-stereo"
        );
        // A long profile segment is still elided, but after the marker.
        let long = device_combo_text(
            "alsa_output.usb-X-00.analog-surround-40",
            DevicePresence::Missing,
        );
        assert!(long.starts_with("! "), "{long}");
        assert_eq!(long.chars().count(), 15, "{long}");
        assert!(long.ends_with('…'), "{long}");
        // A short name fits entirely.
        assert_eq!(device_combo_text("hp", DevicePresence::Missing), "! hp");
    }

    /// The tooltip always carries the **whole** `node.name`, which is the only
    /// way to tell two devices of the same family apart (the combo shows the
    /// profile segment: `analog-stereo` for every analog card).
    #[test]
    fn the_device_tooltip_shows_the_full_node_name() {
        let name = "alsa_output.usb-PreSonus_Studio_26c_01424548-00.pro-output-0";
        assert_eq!(device_tooltip(name, DevicePresence::Found), name);
        let missing = device_tooltip(name, DevicePresence::Missing);
        assert!(missing.starts_with(name), "{missing}");
        assert!(missing.contains("not found"), "{missing}");
        let unknown = device_tooltip(name, DevicePresence::Unknown);
        assert!(unknown.starts_with(name), "{unknown}");
    }

    /// Draw one strip headless in several heights: the layout maths must never
    /// panic (a negative height would panic inside `Rect`/`Ui::allocate`).
    #[test]
    fn strip_paints_headless_in_every_state() {
        let ctx = crate::test_support::headless_context();
        let state = AppState::demo();
        let outputs = state.output_targets();
        for strip in &state.strips {
            for height in [200.0_f32, 430.0, 900.0] {
                let mut cache = ImageCache::new();
                let primitives =
                    crate::test_support::run_pass(&ctx, Vec2::new(120.0, height), 1.0, |ui| {
                        let rect = Rect::from_min_size(
                            egui::pos2(10.0, 10.0),
                            Vec2::new(100.0, height - 20.0),
                        );
                        let actions = strip_ui(
                            ui,
                            rect,
                            &state.theme,
                            strip,
                            &outputs,
                            [0.5, 0.6],
                            &mut cache,
                            MidiButtonState::default(),
                            DevicePresence::Unknown,
                        );
                        assert!(actions.is_empty(), "a paint-only pass must not act");
                    });
                assert!(!primitives.is_empty(), "the strip painted nothing");
            }
        }
    }

    /// Every input strip draws the same rows — `SELECT APPS` included, shown or
    /// not — so they all have the same height and their faders line up.  This is
    /// the regression the user reported (Qt kept the hidden widget in the layout;
    /// the Rust port laid the row out only when the button was drawn).
    #[test]
    fn every_input_gets_the_same_fader_height() {
        let state = AppState::demo();
        let outputs = state.output_targets();
        let theme = &state.theme.theme;
        let strip_h = 532.0;
        let heights: Vec<f32> = state
            .strips
            .iter()
            .filter(|strip| strip.kind == StripType::Input)
            .map(|strip| crate::layout::fader_height_for(theme, strip.kind, outputs.len(), strip_h))
            .collect();
        assert!(heights.len() >= 2);
        let first = heights[0];
        assert!(
            heights.iter().all(|h| (h - first).abs() < 0.01),
            "inputs must share one fader height: {heights:?}"
        );
        // The demo really contains both cases (a physical/default input *and* a
        // virtual non-default one), otherwise the assertion above proves nothing.
        let has_button = state.strips.iter().any(|strip| {
            strip.kind == StripType::Input && strip.device_name.is_none() && !strip.is_default
        });
        let has_no_button = state.strips.iter().any(|strip| {
            strip.kind == StripType::Input && (strip.device_name.is_some() || strip.is_default)
        });
        assert!(
            has_button && has_no_button,
            "the demo must cover both cases"
        );
    }

    /// A right click on the fader / VU opens the strip's context menu and must
    /// **not** move the volume — egui gives the drag to any button, so the old
    /// `Sense` handling swallowed the click and the menu never opened.
    #[test]
    fn a_right_click_on_the_fader_opens_the_strip_menu_without_moving_it() {
        let ctx = crate::test_support::headless_context();
        let state = AppState::demo();
        let outputs = state.output_targets();
        let strip = state
            .strips
            .iter()
            .find(|strip| strip.kind == StripType::Input)
            .expect("the demo has inputs");
        let height = 500.0_f32;
        let strip_rect = Rect::from_min_size(egui::pos2(10.0, 10.0), Vec2::new(100.0, height));
        // Middle of the fader row, computed from the same model the view uses.
        let theme = &state.theme.theme;
        let fader_h =
            crate::layout::fader_height_for(theme, strip.kind, outputs.len(), strip_rect.height());
        let fader_top = strip_rect.top()
            + theme.dimensions.strip_padding
            + crate::layout::rows_before_fader(theme, strip.kind, outputs.len());
        let pos = egui::pos2(strip_rect.center().x, fader_top + fader_h / 2.0);

        let mut actions: Vec<UiAction> = Vec::new();
        let mut run = |time: f64, events: Vec<egui::Event>| {
            let mut cache = ImageCache::new();
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(200.0, 600.0),
                )),
                time: Some(time),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                actions.extend(strip_ui(
                    ui,
                    strip_rect,
                    &state.theme,
                    strip,
                    &outputs,
                    [0.5, 0.5],
                    &mut cache,
                    MidiButtonState::default(),
                    DevicePresence::Unknown,
                ));
            });
            output.textures_delta.clear();
        };
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        run(1.0, vec![egui::Event::PointerMoved(pos)]);
        run(1.1, vec![button(true)]);
        run(1.2, vec![button(false)]);
        assert!(
            actions.iter().any(|action| matches!(
                action,
                UiAction::OpenContextMenu {
                    kind: crate::menus::ContextMenuKind::Strip { .. },
                    ..
                }
            )),
            "a right click on the fader must open the strip menu: {actions:?}"
        );
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, UiAction::Volume { .. })),
            "and must not move the volume: {actions:?}"
        );
    }

    #[test]
    fn the_demo_state_contains_a_virtual_bus() {
        let state = AppState::demo();
        assert!(
            state
                .strips
                .iter()
                .any(|s| s.device_name.is_none() && s.kind == StripType::Output),
            "the demo state must contain a virtual bus (Mix Out)"
        );
    }
}
