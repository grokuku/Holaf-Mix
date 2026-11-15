//! Custom widgets: the fader + VU pair and the button family.
//!
//! The PySide UI drives its look from Qt stylesheets, so "a button" there is a
//! `QPushButton` + 12 lines of CSS.  egui has no stylesheet layer, so each
//! widget is real drawing code here.  Every widget keeps the exact measured
//! geometry and colours of its Qt counterpart (see `docs/ui-inventory.md`) and
//! returns an [`egui::Response`] so the views decide what a click *means* —
//! widgets never mutate the model themselves.

pub mod buttons;
pub mod fader;

pub use buttons::{
    button_in, combo_display, flat_button, flat_button_in, fx_button, fx_button_in,
    fx_button_sized, fx_look, midi_button, midi_button_in, round_add_button, route_button,
    route_button_in, route_look, state_button, state_button_in, ButtonLook, MidiButtonState,
};
pub use fader::{fader_with_vu, FaderOutput, FaderStyle};
