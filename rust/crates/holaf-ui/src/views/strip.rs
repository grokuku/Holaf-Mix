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
//! │  [   SELECT APPS   ]         │  inputs only: virtual & not default
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
//! * **Device combo.** Qt fills it from `pipewire_utils.get_audio_nodes()`;
//!   the device list arrives with the engine bridge, so the combo displays the
//!   current device and emits [`UiAction::OpenDevicePicker`] on click.

use egui::{Rect, Sense, Ui, UiBuilder, Vec2};

use holaf_core::model::{EffectKey, Strip, StripType};

use crate::app::UiAction;
use crate::layout::{row_gap, CONTROL_ROW_HEIGHT, MIN_FADER_HEIGHT};
use crate::theme::images::ImageCache;
use crate::theme::paint;
use crate::theme::resolve::{resolve, StripDescriptor, StripKind};
use crate::theme::ThemeFile;
use crate::views::text_centered;
use crate::widgets::{
    combo_display, fader_with_vu, flat_button, fx_button_in, midi_button_in, route_button_in,
    state_button_in, FaderStyle,
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
    midi_learning: bool,
) -> Vec<UiAction> {
    let theme = &theme_file.theme;
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
        egui::pos2(rect.right() - pad, rect.bottom() - dims.strip_padding_bottom),
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
    let (header_rect, _) =
        child.allocate_exact_size(Vec2::new(avail_w, theme.typography.body + 6.0), Sense::hover());
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
        actions.push(UiAction::Delete {
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
    child.add_space(row_gap(theme));

    // 2. caption (SOURCE IN / DEVICE OUT / VIRTUAL BUS)
    let (caption_rect, _) =
        child.allocate_exact_size(Vec2::new(avail_w, theme.typography.tiny + 4.0), Sense::hover());
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
        Some(name) => device_display_name(name),
        None => match style.kind {
            StripKind::Input => "Apps / Virtual".to_string(),
            _ => "Virtual Sink (Bus)".to_string(),
        },
    };
    if combo_display(&mut child, &device_text, theme).clicked() {
        actions.push(UiAction::OpenDevicePicker {
            uid: strip.uid.clone(),
        });
    }
    child.add_space(row_gap(theme));

    // 4. inputs only: default sink + app selection
    //
    // `_update_app_btn_visibility`: the "SELECT APPS" row only exists on a
    // virtual, non-default input, which makes this strip 23 px shorter than its
    // sibling and gives the fader 23 px more.
    let apps_row = style.kind == StripKind::Input
        && strip.device_name.is_none()
        && !strip.is_default;
    if style.kind == StripKind::Input {
        let (check_rect, _) = child.allocate_exact_size(
            Vec2::new(avail_w, dims.checkbox_height),
            Sense::hover(),
        );
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

        // `_update_app_btn_visibility`: only a virtual, non-default input.
        if apps_row {
            if flat_button(
                &mut child,
                "SELECT APPS",
                Vec2::new(avail_w, dims.combo_height),
                theme,
            )
            .clicked()
            {
                actions.push(UiAction::SelectApps {
                    uid: strip.uid.clone(),
                });
            }
            child.add_space(row_gap(theme));
        }
    }

    // 5. FX row
    let effects: Vec<EffectKey> = EffectKey::for_strip(strip.kind);
    let fx_spacing = 2.0;
    let fx_count = effects.len().max(1) as f32;
    let fx_width = ((avail_w - fx_spacing * (fx_count - 1.0)) / fx_count)
        .clamp(14.0, dims.fx_button_width);
    let (fx_rect, _) = child.allocate_exact_size(
        Vec2::new(avail_w, dims.fx_button_height),
        Sense::hover(),
    );
    for (idx, effect) in effects.iter().enumerate() {
        let active = strip
            .effects
            .get(effect)
            .map(|state| state.active)
            .unwrap_or(false);
        let button_rect = Rect::from_min_size(
            egui::pos2(fx_rect.left() + idx as f32 * (fx_width + fx_spacing), fx_rect.top()),
            Vec2::new(fx_width, dims.fx_button_height),
        );
        if fx_button_in(&child, strip.uid.as_str(), button_rect, fx_label(*effect), active, theme)
            .clicked()
        {
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
            let (plate, _) =
                child.allocate_exact_size(Vec2::new(avail_w, plate_h), Sense::hover());
            painter.rect_filled(plate, paint::corner(3.0), theme.colors.routing_area.color32());
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
                if route_button_in(&child, strip.uid.as_str(), chip_rect, &route_label(label), active, theme)
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
        let modeled = crate::layout::fader_height_for_case(
            theme,
            strip.kind,
            apps_row,
            outputs.len(),
            rect.height(),
        );
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
    let (midi_rect, _) =
        child.allocate_exact_size(Vec2::new(avail_w, midi_h), Sense::hover());
    if midi_button_in(&child, strip.uid.as_str(), midi_rect, midi_learning, theme).clicked() {
        actions.push(UiAction::MidiLearn {
            uid: strip.uid.clone(),
            learning: !midi_learning,
        });
    }

    actions
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
                let primitives = crate::test_support::run_pass(
                    &ctx,
                    Vec2::new(120.0, height),
                    1.0,
                    |ui| {
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
                            false,
                        );
                        assert!(actions.is_empty(), "a paint-only pass must not act");
                    },
                );
                assert!(!primitives.is_empty(), "the strip painted nothing");
            }
        }
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
