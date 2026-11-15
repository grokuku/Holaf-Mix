//! MIDI control — the Rust twin of `src/backend/midi_engine.py` + the
//! `on_midi_message_received` slot of the Qt main window.
//!
//! # What the Qt UI does
//!
//! One `mido` input port is opened, `assigned_apps`-style per-strip mappings are
//! stored in `config.json` (`midi_volume` / `midi_mute` / `midi_mono`, each
//! `{type, channel, control|note}`), a lookup table is keyed by
//! `(type, channel, identifier)`, and:
//!
//! * a **control change** bound to `volume` sets `volume = value / 127`;
//! * a **note on** bound to `mute`/`mono` toggles on press (`velocity > 0`) and
//!   ignores the release — a launchpad/Akai button sends both;
//! * `send_feedback` writes the LED back (`note_on` velocity 127/0, or a CC
//!   value of 127/0 for controllers that use CC for LEDs);
//! * learning captures the **first** relevant message (`control_change` or
//!   `note_on`/`note_off`), then leaves learn mode and stores the mapping.
//!
//! # What is implemented here
//!
//! The **whole policy** is pure and always compiled: [`MidiMessage::parse`]
//! (raw bytes → message), [`MidiMap`] (the lookup table), [`decode`] (message →
//! [`UiAction`]), [`feedback`] (state → LED message), [`choose_port`] (which
//! controller) and [`MidiRuntime`] (reconnection state machine).  The device I/O
//! is behind the `midi` feature (the `port` submodule), **on by default** because the MIDI Mix
//! is the primary controller of this mixer: a machine without an ALSA sequencer
//! still starts, and says why in the status notice.  `--no-default-features` drops
//! it (the mixer then keeps working exactly as before, and the pure rules stay
//! unit tested: `cargo test -p holaf-ui` runs them everywhere).
//!
//! ## The port side (`--features midi`)
//!
//! `midir` (0.11) is the cross-platform crate the Python `mido`+`python-rtmidi`
//! pair was replaced with: one input port for listening, one output port for the
//! LED feedback, and one thread that forwards messages through a channel.  Two
//! limitations are documented there: a controller that only exposes a *combined*
//! port needs both directions on the same client, and `midir` reports ports by
//! name, so a controller re-plugged with a different name must be re-selected.
//!
//! ## Which port, and what happens when it goes away
//!
//! [`choose_port`] is the pure selection rule and [`MidiRuntime`] the pure
//! connection state machine (closed / open / failed, retried every
//! [`RETRY_INTERVAL`]).  Both are compiled and tested without ALSA, so the
//! "controller unplugged → retry every 2 s → reconnect" behaviour is asserted in
//! the unit tests instead of being hoped for on the user's desk.  The shell
//! (`main.rs`) is the only place that touches a device.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use holaf_core::model::Strip;

use crate::app::UiAction;

/// Controller values are 7-bit (`value / 127` is the Qt volume law).
pub const CC_MAX: u8 = 127;

/// How often a lost or absent controller is looked for again.
///
/// A MIDI controller is a USB device: unplugging it mid-set must not need a
/// restart of the mixer, and re-plugging it must bring the faders back within a
/// couple of seconds.  Two seconds is short enough to feel automatic and long
/// enough that a missing device is not probed 60 times per second.
pub const RETRY_INTERVAL: Duration = Duration::from_millis(2000);

/// Which controller property a mapping drives (`midi_volume` & co. in
/// `config.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MidiTarget {
    Volume,
    Mute,
    Mono,
}

impl MidiTarget {
    pub const ALL: [MidiTarget; 3] = [MidiTarget::Volume, MidiTarget::Mute, MidiTarget::Mono];

    pub fn as_str(self) -> &'static str {
        match self {
            MidiTarget::Volume => "volume",
            MidiTarget::Mute => "mute",
            MidiTarget::Mono => "mono",
        }
    }

    /// The `config.json` key the mapping is stored under (same as Python).
    pub fn config_key(self) -> &'static str {
        match self {
            MidiTarget::Volume => "midi_volume",
            MidiTarget::Mute => "midi_mute",
            MidiTarget::Mono => "midi_mono",
        }
    }

    pub fn from_config_key(key: &str) -> Option<Self> {
        match key {
            "midi_volume" => Some(MidiTarget::Volume),
            "midi_mute" => Some(MidiTarget::Mute),
            "midi_mono" => Some(MidiTarget::Mono),
            _ => None,
        }
    }
}

/// The two message shapes the mixer binds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKind {
    ControlChange,
    Note,
}

impl MessageKind {
    /// The `type` string `config.json` uses (`mido`'s names).
    pub fn as_config_str(self) -> &'static str {
        match self {
            MessageKind::ControlChange => "control_change",
            // `note_on` is what the Python learn stored for both note shapes.
            MessageKind::Note => "note_on",
        }
    }

    pub fn from_config_str(kind: &str) -> Option<Self> {
        match kind {
            "control_change" => Some(MessageKind::ControlChange),
            "note_on" | "note_off" => Some(MessageKind::Note),
            _ => None,
        }
    }
}

/// One channel-voice message the mixer understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MidiMessage {
    Control { channel: u8, control: u8, value: u8 },
    Note { channel: u8, note: u8, velocity: u8 },
}

impl MidiMessage {
    /// Decode a 3-byte channel-voice message.
    ///
    /// `0x90`–`0x9F` is a note on (a `note_on` with velocity 0 is a release,
    /// which is exactly what a controller sends when the button comes back up),
    /// `0x80`–`0x8F` is an explicit note off, `0xB0`–`0xBF` a control change.
    /// Anything else (clock, active sensing, program change, sysex) is `None`:
    /// the mixer has nothing to bind those to.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 3 {
            return None;
        }
        let status = bytes[0];
        if status & 0x80 == 0 {
            return None; // running status is not decoded: `midir` always sends the status byte
        }
        let channel = status & 0x0F;
        let data1 = bytes[1] & 0x7F;
        let data2 = bytes[2] & 0x7F;
        match status & 0xF0 {
            0x90 => Some(MidiMessage::Note {
                channel,
                note: data1,
                velocity: data2,
            }),
            0x80 => Some(MidiMessage::Note {
                channel,
                note: data1,
                velocity: 0,
            }),
            0xB0 => Some(MidiMessage::Control {
                channel,
                control: data1,
                value: data2,
            }),
            _ => None,
        }
    }

    /// The bytes [`Self::parse`] would read back (used for the LED feedback).
    pub fn to_bytes(self) -> Vec<u8> {
        match self {
            MidiMessage::Control {
                channel,
                control,
                value,
            } => vec![0xB0 | (channel & 0x0F), control & 0x7F, value & 0x7F],
            MidiMessage::Note {
                channel,
                note,
                velocity,
            } => vec![0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F],
        }
    }

    pub fn kind(self) -> MessageKind {
        match self {
            MidiMessage::Control { .. } => MessageKind::ControlChange,
            MidiMessage::Note { .. } => MessageKind::Note,
        }
    }

    pub fn channel(self) -> u8 {
        match self {
            MidiMessage::Control { channel, .. } | MidiMessage::Note { channel, .. } => channel,
        }
    }

    /// The CC number, or the note number — the half of the key that is not the
    /// message type (`(type, channel, identifier)` in Python).
    pub fn identifier(self) -> u8 {
        match self {
            MidiMessage::Control { control, .. } => control,
            MidiMessage::Note { note, .. } => note,
        }
    }

    /// CC value, or note velocity.
    pub fn value(self) -> u8 {
        match self {
            MidiMessage::Control { value, .. } => value,
            MidiMessage::Note { velocity, .. } => velocity,
        }
    }
}

/// One stored mapping (`midi_volume` & co.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Binding {
    pub kind: MessageKind,
    pub channel: u8,
    pub control: Option<u8>,
    pub note: Option<u8>,
}

impl Binding {
    /// A `control_change` mapping.
    pub fn control(channel: u8, control: u8) -> Self {
        Self {
            kind: MessageKind::ControlChange,
            channel,
            control: Some(control),
            note: None,
        }
    }

    /// A `note_on` mapping (what a learn captures for both note shapes).
    pub fn note(channel: u8, note: u8) -> Self {
        Self {
            kind: MessageKind::Note,
            channel,
            control: None,
            note: Some(note),
        }
    }

    /// The configured identifier (control or note), if any.
    pub fn identifier(self) -> Option<u8> {
        self.control.or(self.note)
    }

    /// Short label for the UI (`CC 61`, `N 24`) — what the MIDI button shows
    /// for a moment after a successful learn.
    pub fn short_label(self) -> String {
        match self.kind {
            MessageKind::ControlChange => format!("CC {}", self.control.unwrap_or(0)),
            MessageKind::Note => format!("N {}", self.note.unwrap_or(0)),
        }
    }

    /// Long label for a tooltip / status line (`CC 61 ch.1`).
    pub fn describe(self) -> String {
        format!(
            "{} ch.{}",
            self.short_label(),
            // The file stores the 0-based channel; humans count from 1.
            self.channel + 1
        )
    }

    pub fn matches(self, message: MidiMessage) -> bool {
        self.kind == message.kind()
            && self.channel == message.channel()
            && self.identifier() == Some(message.identifier())
    }

    /// `{type, channel, control|note}` — the `config.json` shape the PySide UI
    /// writes (and the only one it reads).
    pub fn to_json(self) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        object.insert("type".to_string(), self.kind.as_config_str().into());
        object.insert("channel".to_string(), self.channel.into());
        if let Some(control) = self.control {
            object.insert("control".to_string(), control.into());
        }
        if let Some(note) = self.note {
            object.insert("note".to_string(), note.into());
        }
        serde_json::Value::Object(object)
    }

    /// Read one `config.json` mapping.  A malformed entry is `None` (a mapping
    /// nobody can honour is worse than no mapping: it would swallow messages).
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        let kind = MessageKind::from_config_str(object.get("type")?.as_str()?)?;
        let channel = object.get("channel")?.as_u64()?;
        if channel > 15 {
            return None;
        }
        let read = |key: &str| -> Option<u8> {
            object
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .filter(|value| *value <= 127)
                .map(|value| value as u8)
        };
        let control = read("control");
        let note = read("note");
        match kind {
            MessageKind::ControlChange => {
                control.map(|control| Self::control(channel as u8, control))
            }
            MessageKind::Note => note.map(|note| Self::note(channel as u8, note)),
        }
    }

    /// Read a **bare number** mapping (`"midi_volume": 61`).
    ///
    /// Neither the Python `Strip.to_dict` nor this crate ever writes that shape
    /// (both write `{type, channel, control|note}` — see [`Self::to_json`]), but a
    /// hand-edited config can contain one, and the Qt side crashes on it
    /// (`TypeError: 'int' object is not subscriptable`) instead of ignoring it.
    /// Accepting it costs three lines and turns "my Mix fader stopped working"
    /// into "the mapping was read".  Channel 0 and the target's natural message
    /// shape are assumed: a volume knob is a CC, a mute/mono button a note.
    pub fn from_legacy_number(target: MidiTarget, value: &serde_json::Value) -> Option<Self> {
        let number = u8::try_from(value.as_u64()?).ok().filter(|n| *n <= 127)?;
        Some(match target {
            MidiTarget::Volume => Self::control(0, number),
            MidiTarget::Mute | MidiTarget::Mono => Self::note(0, number),
        })
    }
}

/// One binding, with the strip it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripBinding {
    pub uid: &'static str,
    pub target: MidiTarget,
    pub binding: Binding,
}

/// The lookup table (`_rebuild_midi_lookup`).
///
/// Keyed by `(kind, channel, identifier)`; a mapping that two strips claim is
/// resolved by the **last** strip in the list, exactly like the Python dict
/// assignment overwriting the earlier entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MidiMap {
    entries: HashMap<(MessageKind, u8, u8), (String, MidiTarget)>,
}

impl MidiMap {
    /// Build the table from `(uid, target, binding)` triples.
    pub fn new<'a>(bindings: impl IntoIterator<Item = (&'a str, MidiTarget, Binding)>) -> Self {
        let mut map = Self::default();
        for (uid, target, binding) in bindings {
            if let Some(identifier) = binding.identifier() {
                map.entries.insert(
                    (binding.kind, binding.channel, identifier),
                    (uid.to_string(), target),
                );
            }
        }
        map
    }

    /// The Python `_rebuild_midi_lookup` over the strips + their config entries.
    pub fn from_strips(
        strips: &[Strip],
        bindings: &HashMap<String, Vec<(MidiTarget, Binding)>>,
    ) -> Self {
        let mut collect = Vec::new();
        for strip in strips {
            let Some(entries) = bindings.get(&strip.uid) else {
                continue;
            };
            for (target, binding) in entries {
                collect.push((strip.uid.as_str(), *target, *binding));
            }
        }
        Self::new(collect)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// What `message` drives, if anything.
    pub fn lookup(&self, message: MidiMessage) -> Option<(&str, MidiTarget)> {
        self.entries
            .get(&(message.kind(), message.channel(), message.identifier()))
            .map(|(uid, target)| (uid.as_str(), *target))
    }
}

/// What one incoming message must do.
#[derive(Debug, Clone, PartialEq)]
pub enum MidiOutcome {
    /// Perform this UI action (already carrying the clamped value / the toggled
    /// state read from the model).
    Action(UiAction),
    /// Learning captured the mapping (the caller stores it in `config.json`).
    Learned {
        uid: String,
        target: MidiTarget,
        binding: Binding,
    },
    /// A mapped message that needs no new action (a button **release**, for
    /// instance: the toggle happened on the press).
    Ignored,
}

/// Decode one message against the map.
///
/// * learn mode wins over the table: the first bindable message defines the
///   mapping (Python `_process_learn`);
/// * `volume` uses `value / 127` and is applied **as it arrives** — no throttle
///   here.  A knob sends a few hundred messages per second and every one of them
///   is the truth about where the knob is; the engine side already coalesces
///   (`AppState::volumes`, 25 Hz, last value wins), so dropping messages at this
///   level could only ever *lose* the final position of a gesture and make the
///   fader arrive short of where the knob stopped;
/// * `mute` / `mono` toggle on press only (`velocity > 0`), which is what makes
///   a two-event button behave like a button instead of a double toggle.
pub fn decode(
    map: &MidiMap,
    message: MidiMessage,
    learning: Option<(&str, MidiTarget)>,
    strips: &[Strip],
) -> MidiOutcome {
    if let Some((uid, target)) = learning {
        let binding = match message {
            MidiMessage::Control {
                channel, control, ..
            } => Binding::control(channel, control),
            MidiMessage::Note { channel, note, .. } => Binding::note(channel, note),
        };
        return MidiOutcome::Learned {
            uid: uid.to_string(),
            target,
            binding,
        };
    }
    let Some((uid, target)) = map.lookup(message) else {
        return MidiOutcome::Ignored;
    };
    let Some(strip) = strips.iter().find(|strip| strip.uid == uid) else {
        return MidiOutcome::Ignored;
    };
    match target {
        MidiTarget::Volume => MidiOutcome::Action(UiAction::Volume {
            uid: uid.to_string(),
            volume: cc_to_volume(message.value()),
        }),
        MidiTarget::Mute => {
            if message.value() == 0 {
                // Button release: the toggle already happened on the press.
                return MidiOutcome::Ignored;
            }
            MidiOutcome::Action(UiAction::Mute {
                uid: uid.to_string(),
                mute: !strip.mute,
            })
        }
        MidiTarget::Mono => {
            if message.value() == 0 {
                return MidiOutcome::Ignored;
            }
            MidiOutcome::Action(UiAction::Mono {
                uid: uid.to_string(),
                mono: !strip.is_mono,
            })
        }
    }
}

/// `value / 127` — the Qt volume law, clamped to `0..=1`.
pub fn cc_to_volume(value: u8) -> f32 {
    (value.min(CC_MAX) as f32) / (CC_MAX as f32)
}

/// The LED message for a button state (`send_feedback`): full velocity/value to
/// light it, zero to clear it — on the **same** message shape the button sends,
/// so the controller's LED follows its own binding.
pub fn feedback(binding: Binding, active: bool) -> MidiMessage {
    let level = if active { CC_MAX } else { 0 };
    match binding.kind {
        MessageKind::ControlChange => MidiMessage::Control {
            channel: binding.channel,
            control: binding.control.unwrap_or(0),
            value: level,
        },
        MessageKind::Note => MidiMessage::Note {
            channel: binding.channel,
            note: binding.note.unwrap_or(0),
            velocity: level,
        },
    }
}

/// Which button states a strip must mirror on its controller
/// (`_restore_button_states`): every `mute` / `mono` mapping, with its state.
pub fn button_feedback(
    strips: &[Strip],
    bindings: &HashMap<String, Vec<(MidiTarget, Binding)>>,
) -> Vec<MidiMessage> {
    let mut messages = Vec::new();
    for strip in strips {
        let Some(entries) = bindings.get(&strip.uid) else {
            continue;
        };
        for (target, binding) in entries {
            match target {
                MidiTarget::Mute => messages.push(feedback(*binding, strip.mute)),
                MidiTarget::Mono => messages.push(feedback(*binding, strip.is_mono)),
                MidiTarget::Volume => {}
            }
        }
    }
    messages
}

// ---------------------------------------------------------------------------
// Port selection and reconnection — pure, always compiled
// ---------------------------------------------------------------------------

/// Pick the port to listen to out of the ones the system offers.
///
/// * `configured` set → that name **if it is currently present**, else nothing
///   (a configured controller that is unplugged must not be silently replaced by
///   another device: the retry loop keeps its name and reconnects when it comes
///   back);
/// * `configured` unset → the AKAI MIDI Mix (or anything named `… mix …`),
///   then the only port if the system has exactly one, else nothing (grabbing an
///   arbitrary synth would be worse than letting the user pick).
///
/// The Qt UI auto-opened the first port whose name contained `mix`, else the
/// first port at all (`_init_midi`); the last rule is deliberately *not*
/// reproduced — with two ports that silently picks the wrong one.
pub fn choose_port(names: &[String], configured: Option<&str>) -> Option<String> {
    match configured {
        Some(name) => names.iter().find(|port| *port == name).cloned(),
        None => {
            let lower: Vec<String> = names.iter().map(|name| name.to_lowercase()).collect();
            names
                .iter()
                .zip(&lower)
                .find(|(_, lower)| lower.contains("midi mix"))
                .or_else(|| names.iter().zip(&lower).find(|(_, lower)| lower.contains("mix")))
                .map(|(name, _)| name.clone())
                .or_else(|| match names {
                    [only] => Some(only.clone()),
                    _ => None,
                })
        }
    }
}

/// Connection state of the controller.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PortState {
    /// Nothing open: no controller selected, or the port is being (re)opened.
    #[default]
    Closed,
    /// A controller answers.
    Open,
    /// The last attempt failed (the reason is what the status notice shows).
    Failed(String),
}

/// The pure reconnection policy: *which* port to open, and *when* to try again.
///
/// The device itself lives in the shell (it needs `midir`); this type owns the
/// decisions so they can be tested with an injected [`Instant`] — no hardware,
/// no sleeping:
///
/// ```ignore
/// let mut runtime = MidiRuntime::new(Some("MIDI Mix MIDI 1".into()));
/// assert_eq!(runtime.attempt(t0), Some("MIDI Mix MIDI 1".into())); // first try
/// runtime.on_failed("no such port");
/// assert_eq!(runtime.attempt(t0 + Duration::from_millis(500)), None); // too soon
/// assert!(runtime.attempt(t0 + RETRY_INTERVAL).is_some());           // retried
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MidiRuntime {
    desired: Option<String>,
    state: PortState,
    last_attempt: Option<Instant>,
}

impl MidiRuntime {
    pub fn new(desired: Option<String>) -> Self {
        Self {
            desired,
            state: PortState::Closed,
            last_attempt: None,
        }
    }

    /// The port the user (or the auto-selection) wants, `None` = MIDI off.
    pub fn desired(&self) -> Option<&str> {
        self.desired.as_deref()
    }

    /// Ask for another port (or none).  Returns `true` when that is a change, so
    /// the caller can drop the open connection and log it once.
    pub fn set_desired(&mut self, desired: Option<String>) -> bool {
        if self.desired == desired {
            return false;
        }
        self.desired = desired;
        self.state = PortState::Closed;
        self.last_attempt = None;
        true
    }

    pub fn state(&self) -> &PortState {
        &self.state
    }

    pub fn is_open(&self) -> bool {
        self.state == PortState::Open
    }

    /// When the port was last (re)tried — the caller can log a retry budget, and
    /// a test can assert that an open port is not hammered.
    pub fn last_attempt(&self) -> Option<Instant> {
        self.last_attempt
    }

    /// Why the last attempt failed / why the port was lost.
    pub fn last_error(&self) -> Option<&str> {
        match &self.state {
            PortState::Failed(reason) => Some(reason.as_str()),
            _ => None,
        }
    }

    /// The port to (re)try now, if one is due.  Records the attempt so the next
    /// call waits [`RETRY_INTERVAL`].
    pub fn attempt(&mut self, now: Instant) -> Option<String> {
        let name = self.desired.clone()?;
        if self.is_open() {
            return None;
        }
        if let Some(previous) = self.last_attempt {
            if now.saturating_duration_since(previous) < RETRY_INTERVAL {
                return None;
            }
        }
        self.last_attempt = Some(now);
        Some(name)
    }

    /// The port opened: the controller answers.
    pub fn on_opened(&mut self, name: &str) {
        if self.desired.as_deref() == Some(name) {
            self.state = PortState::Open;
        }
    }

    /// The attempt failed (the port vanished between the probe and the open).
    pub fn on_failed(&mut self, reason: impl Into<String>) {
        self.state = PortState::Failed(reason.into());
    }

    /// The open port went away (unplugged): back to the retry loop, without
    /// forgetting which controller we are waiting for.
    pub fn on_lost(&mut self, reason: impl Into<String>) {
        if self.desired.is_some() {
            self.state = PortState::Failed(reason.into());
        } else {
            self.state = PortState::Closed;
        }
    }

    /// What the status notice should say about the controller.
    pub fn status_line(&self) -> Option<String> {
        match (&self.state, self.desired.as_deref()) {
            (PortState::Open, Some(name)) => Some(format!("MIDI: {name}")),
            (PortState::Failed(reason), Some(name)) => Some(format!(
                "MIDI: {name} unavailable ({reason}) - retrying every {} s",
                RETRY_INTERVAL.as_secs()
            )),
            (PortState::Failed(reason), None) => Some(format!("MIDI: {reason}")),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Device I/O — feature `midi`
// ---------------------------------------------------------------------------

#[cfg(feature = "midi")]
pub use port::{MidiEvent, MidiPort, MidiVirtualSource, Ports};

#[cfg(feature = "midi")]
mod port {
    //! `midir` backend: one input port for the messages, one output port for the
    //! LEDs.
    //!
    //! # Known limits (documented, not hidden)
    //!
    //! * a controller that only exposes a combined port is opened twice (input
    //!   and output on the same name) — `midir` supports it, but two clients on
    //!   one ALSA raw port depend on the driver;
    //! * ports are matched **by name**: a controller re-plugged under another
    //!   name must be re-selected (the UI lists the names it sees);
    //! * no hot-plug watch: the port list is read when the UI asks (the app
    //!   re-opens on demand), because `midir` cannot notify a closed port.

    use super::{MidiMessage, MidiTarget};
    use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

    /// What the UI receives from the MIDI thread.
    #[derive(Debug, Clone, PartialEq)]
    pub enum MidiEvent {
        Message(MidiMessage),
        /// A port was opened (`port name`).
        Opened(String),
        /// The port disappeared or could not be opened.
        Error(String),
    }

    /// Handle to the MIDI thread: messages in, LED bytes out.
    pub struct MidiPort {
        events: Receiver<MidiEvent>,
        outbox: Sender<Vec<u8>>,
        name: String,
        connection: Option<midir::MidiInputConnection<()>>,
        output: Option<midir::MidiOutputConnection>,
        events_tx: Sender<MidiEvent>,
    }

    impl std::fmt::Debug for MidiPort {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MidiPort")
                .field("port", &self.name)
                .field("output", &self.output.is_some())
                .finish()
        }
    }

    /// Every port the system offers, both directions.
    ///
    /// A controller usually exposes the same name twice (an input and an output
    /// on the same ALSA client); listing both is what makes `--midi-list`
    /// answerable for the LED path too.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct Ports {
        pub inputs: Vec<String>,
        pub outputs: Vec<String>,
    }

    impl Ports {
        pub fn is_empty(&self) -> bool {
            self.inputs.is_empty() && self.outputs.is_empty()
        }
    }

    impl MidiPort {
        /// Names of every MIDI input the system offers.
        pub fn input_names() -> Result<Vec<String>, String> {
            Ok(Self::ports()?.inputs)
        }

        /// Names of every MIDI output the system offers.
        pub fn output_names() -> Result<Vec<String>, String> {
            Ok(Self::ports()?.outputs)
        }

        /// Both directions in one query (what `--midi-list` prints).
        pub fn ports() -> Result<Ports, String> {
            let input = midir::MidiInput::new("holaf-mix-query-in")
                .map_err(|err| describe_alsa_error(&err.to_string()))?;
            let inputs = input
                .ports()
                .iter()
                .filter_map(|port| input.port_name(port).ok())
                .collect();
            let output = midir::MidiOutput::new("holaf-mix-query-out")
                .map_err(|err| describe_alsa_error(&err.to_string()))?;
            let outputs = output
                .ports()
                .iter()
                .filter_map(|port| output.port_name(port).ok())
                .collect();
            Ok(Ports { inputs, outputs })
        }

        /// Open `name` for input and output and start forwarding.
        ///
        /// The callback runs on `midir`'s own thread; it only pushes into a
        /// channel, so no lock is taken on the audio/UI path.
        pub fn open(name: &str) -> Result<Self, String> {
            Self::open_with_wake(name, None)
        }

        /// [`Self::open`], plus a `wake` — usually the `egui::Context` of the
        /// window.
        ///
        /// The reader thread calls `wake.request_repaint()` as soon as a message
        /// arrives, so the UI paints **at the speed of the controller** instead of
        /// on the next vumeter tick (40 ms).  That is the whole difference between
        /// a MIDI fader that feels like the mouse and one that arrives in visible
        /// steps; without it a knob sweep is only *seen* 25 times a second.
        /// `Context` is `Send + Sync + Clone`, and `request_repaint` only sets a
        /// flag in winit, so this is safe from `midir`'s thread.
        pub fn open_with_wake(name: &str, wake: Option<egui::Context>) -> Result<Self, String> {
            let input = midir::MidiInput::new("holaf-mix-in")
                .map_err(|err| describe_alsa_error(&err.to_string()))?;
            let output = midir::MidiOutput::new("holaf-mix-out").map_err(|err| err.to_string())?;
            let input_port = input
                .ports()
                .into_iter()
                .find(|port| input.port_name(port).ok().as_deref() == Some(name))
                .ok_or_else(|| format!("no MIDI input named {name:?}"))?;
            let output_port = output
                .ports()
                .into_iter()
                .find(|port| output.port_name(port).ok().as_deref() == Some(name));

            let (events_tx, events) = mpsc::channel();
            let (outbox, outbox_rx) = mpsc::channel::<Vec<u8>>();
            let callback_tx = events_tx.clone();
            let connection = input
                .connect(
                    &input_port,
                    "holaf-mix-read",
                    move |_stamp, bytes, _| {
                        if let Some(message) = MidiMessage::parse(bytes) {
                            let _ = callback_tx.send(MidiEvent::Message(message));
                            // Wake the UI thread *now*: the message is applied on
                            // the next frame, so scheduling one immediately is
                            // what keeps a knob as responsive as the mouse.
                            if let Some(ctx) = &wake {
                                ctx.request_repaint();
                            }
                        }
                    },
                    (),
                )
                .map_err(|err| describe_alsa_error(&err.to_string()))?;

            let mut output = match output_port {
                Some(port) => Some(
                    output
                        .connect(&port, "holaf-mix-led")
                        .map_err(|err| err.to_string())?,
                ),
                None => None,
            };

            // LED writes happen on their own thread: a controller that stops
            // draining must never block the UI.
            std::thread::Builder::new()
                .name("holaf-ui-midi-out".to_string())
                .spawn(move || {
                    while let Ok(bytes) = outbox_rx.recv() {
                        if let Some(output) = output.as_mut() {
                            if let Err(err) = output.send(&bytes) {
                                log::warn!("midi: LED write failed: {err}");
                            }
                        }
                    }
                })
                .expect("the MIDI output thread must start");

            let _ = events_tx.send(MidiEvent::Opened(name.to_string()));
            Ok(Self {
                events,
                outbox,
                name: name.to_string(),
                connection: Some(connection),
                output: None,
                events_tx,
            })
        }

        pub fn name(&self) -> &str {
            &self.name
        }

        /// Drain what arrived since the last call (non-blocking).
        pub fn drain(&self) -> Vec<MidiEvent> {
            let mut events = Vec::new();
            loop {
                match self.events.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
            events
        }

        /// Queue one message for the controller (the LED feedback).
        pub fn send(&self, message: MidiMessage) {
            match self.outbox.send(message.to_bytes()) {
                Ok(()) => {}
                Err(err) => log::warn!("midi: cannot queue the LED message: {err}"),
            }
        }

        /// Which targets this port has a mapping for (informational).
        pub fn targets() -> [MidiTarget; 3] {
            MidiTarget::ALL
        }
    }

    /// A **virtual** controller: a MIDI source this process creates, which other
    /// clients (and [`MidiPort`]) see exactly like a hardware one.
    ///
    /// It is what `snd-virmidi` does in the kernel, done in userspace with
    /// `midir::os::unix::VirtualOutput`.  Two uses: the live test drives the whole
    /// device path without a MIDI Mix on the desk, and a user with no controller
    /// can try `--midi <name>` against it.
    pub struct MidiVirtualSource {
        connection: midir::MidiOutputConnection,
        /// The ALSA name other clients see (`client:port 128:0`), not the short
        /// name we asked for: this is what [`MidiPort::open`] needs.
        alsa_name: String,
    }

    impl std::fmt::Debug for MidiVirtualSource {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MidiVirtualSource")
                .field("alsa_name", &self.alsa_name)
                .finish()
        }
    }

    impl MidiVirtualSource {
        /// Create the source and resolve the ALSA name other clients will see.
        #[cfg(unix)]
        pub fn create(short_name: &str) -> Result<Self, String> {
            use midir::os::unix::VirtualOutput;

            let output = midir::MidiOutput::new("holaf-mix-virtual")
                .map_err(|err| describe_alsa_error(&err.to_string()))?;
            let connection = output
                .create_virtual(short_name)
                .map_err(|err| describe_alsa_error(&format!("create_virtual: {err:?}")))?;
            // The ALSA port is registered synchronously, but the *name* it is
            // published under is chosen by the sequencer: look for it.
            let mut alsa_name = None;
            for _ in 0..10 {
                if let Ok(names) = MidiPort::input_names() {
                    alsa_name = names
                        .into_iter()
                        .find(|name| name.contains(short_name));
                    if alsa_name.is_some() {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let alsa_name = alsa_name.ok_or_else(|| {
                format!("the virtual port {short_name:?} did not appear in the port list")
            })?;
            Ok(Self {
                connection,
                alsa_name,
            })
        }

        /// Not available on Windows: `midir` has no virtual ports there.
        #[cfg(not(unix))]
        pub fn create(short_name: &str) -> Result<Self, String> {
            Err(format!(
                "virtual MIDI ports are not supported on this platform ({short_name})"
            ))
        }

        /// The name [`MidiPort::open`] accepts.
        pub fn alsa_name(&self) -> &str {
            &self.alsa_name
        }

        /// Play one message, as a controller would.
        pub fn send(&mut self, message: MidiMessage) -> Result<(), String> {
            self.connection
                .send(&message.to_bytes())
                .map_err(|err| err.to_string())
        }

        /// Play raw bytes (clock, sysex: what the parser must ignore).
        pub fn send_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.connection.send(bytes).map_err(|err| err.to_string())
        }
    }

    /// Turn the raw ALSA failure into something a user can act on.
    ///
    /// The two real cases are a machine without an ALSA sequencer (a container,
    /// a kernel without `snd-seq`: `midir` then says `MidiInput::new() failed`,
    /// which explains nothing) and a missing permission on `/dev/snd/seq`.
    fn describe_alsa_error(raw: &str) -> String {
        let lower = raw.to_lowercase();
        if lower.contains("permission") || lower.contains("denied") {
            return format!(
                "{raw} (is the user in the `audio` group, or does /dev/snd/seq allow it?)"
            );
        }
        if lower.contains("seq")
            || lower.contains("no such file")
            || lower.contains("device or resource busy")
            || lower.contains("failed")
            // midir's own wording when ALSA cannot initialise (`/dev/snd/seq`
            // missing, or the kernel has no `snd-seq`).
            || lower.contains("could not be initialized")
            || lower.contains("midi support")
        {
            return format!(
                "{raw} (no ALSA sequencer: /dev/snd/seq is missing - load the `snd-seq` module, \
                 or run outside a container without /dev/snd)"
            );
        }
        raw.to_string()
    }

    impl Drop for MidiPort {
        fn drop(&mut self) {
            if let Some(connection) = self.connection.take() {
                connection.close();
            }
            self.output = None;
            let _ = self
                .events_tx
                .send(MidiEvent::Error("port closed".to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holaf_core::model::StripType;

    fn strip(uid: &str) -> Strip {
        Strip::with_uid(uid, uid.to_uppercase(), StripType::Input)
    }

    fn map() -> MidiMap {
        MidiMap::new([
            ("in-mic", MidiTarget::Volume, Binding::control(0, 61)),
            ("in-mic", MidiTarget::Mute, Binding::note(0, 24)),
            ("in-mic", MidiTarget::Mono, Binding::note(0, 22)),
            ("out-hp", MidiTarget::Volume, Binding::control(1, 62)),
        ])
    }

    #[test]
    fn messages_are_parsed_from_raw_bytes() {
        assert_eq!(
            MidiMessage::parse(&[0xB0, 61, 100]),
            Some(MidiMessage::Control {
                channel: 0,
                control: 61,
                value: 100
            })
        );
        assert_eq!(
            MidiMessage::parse(&[0x91, 24, 127]),
            Some(MidiMessage::Note {
                channel: 1,
                note: 24,
                velocity: 127
            })
        );
        // A `note_on` with velocity 0 is the release of a button.
        assert_eq!(
            MidiMessage::parse(&[0x90, 24, 0]),
            Some(MidiMessage::Note {
                channel: 0,
                note: 24,
                velocity: 0
            })
        );
        // …and an explicit note off is the same thing.
        assert_eq!(
            MidiMessage::parse(&[0x80, 24, 64]),
            Some(MidiMessage::Note {
                channel: 0,
                note: 24,
                velocity: 0
            })
        );
        // Clock/active-sensing/program-change/sysex/short buffers are ignored.
        for bytes in [
            vec![0xF8],
            vec![0xFE, 0, 0],
            vec![0xC0, 1, 0],
            vec![0xF0, 1, 2, 3],
            vec![0xB0, 61],
            vec![61, 1, 2],
        ] {
            assert_eq!(MidiMessage::parse(&bytes), None, "{bytes:?}");
        }
        // The bytes round-trip (that is the LED path).
        let message = MidiMessage::Note {
            channel: 3,
            note: 24,
            velocity: 127,
        };
        assert_eq!(MidiMessage::parse(&message.to_bytes()), Some(message));
    }

    #[test]
    fn a_control_change_drives_the_volume_with_the_qt_law() {
        let map = map();
        let strips = [strip("in-mic"), strip("out-hp")];
        let outcome = decode(
            &map,
            MidiMessage::Control {
                channel: 0,
                control: 61,
                value: 64,
            },
            None,
            &strips,
        );
        match outcome {
            MidiOutcome::Action(UiAction::Volume { uid, volume }) => {
                assert_eq!(uid, "in-mic");
                assert!((volume - 64.0 / 127.0).abs() < 1e-6, "{volume}");
            }
            other => panic!("{other:?}"),
        }
        // 0 and 127 are the endpoints.
        assert_eq!(cc_to_volume(0), 0.0);
        assert_eq!(cc_to_volume(127), 1.0);
        // A message on another channel or another CC is not ours.
        assert_eq!(
            map.lookup(MidiMessage::Control {
                channel: 1,
                control: 61,
                value: 10
            }),
            None
        );
        assert_eq!(
            map.lookup(MidiMessage::Control {
                channel: 1,
                control: 62,
                value: 10
            }),
            Some(("out-hp", MidiTarget::Volume))
        );
    }

    /// A knob burst must be **lossless**: the model follows every message and
    /// the last one always wins.  The engine side coalesces (25 Hz, last value
    /// wins); dropping messages here is what used to make a MIDI sweep arrive
    /// short of where the knob stopped, and in visible steps.
    #[test]
    fn a_knob_burst_is_applied_in_full_and_the_last_value_wins() {
        let map = map();
        let strips = [strip("in-mic")];
        let control = |value| MidiMessage::Control {
            channel: 0,
            control: 61,
            value,
        };
        let mut volumes = Vec::new();
        for value in (10..=100).step_by(5) {
            match decode(&map, control(value), None, &strips) {
                MidiOutcome::Action(UiAction::Volume { uid, volume }) => {
                    assert_eq!(uid, "in-mic");
                    volumes.push(volume);
                }
                other => panic!("a burst message was dropped: {other:?}"),
            }
        }
        assert_eq!(volumes.len(), 19, "every message is applied");
        assert_eq!(*volumes.last().unwrap(), cc_to_volume(100));
        // Monotonic here because the CC values are: the fader does not jump
        // backwards between two messages of one gesture.
        assert!(volumes.windows(2).all(|pair| pair[1] > pair[0]));
    }

    #[test]
    fn a_button_toggles_on_press_and_ignores_the_release() {
        let map = map();
        let strips = [strip("in-mic")];
        let press = MidiMessage::Note {
            channel: 0,
            note: 24,
            velocity: 127,
        };
        assert_eq!(
            decode(&map, press, None, &strips),
            MidiOutcome::Action(UiAction::Mute {
                uid: "in-mic".to_string(),
                mute: true
            })
        );
        // The release must not toggle back.
        let release = MidiMessage::Note {
            channel: 0,
            note: 24,
            velocity: 0,
        };
        assert_eq!(decode(&map, release, None, &strips), MidiOutcome::Ignored);
        // MONO likewise, and the toggle reads the current model state.
        let mut mono = strip("in-mic");
        mono.is_mono = true;
        assert_eq!(
            decode(
                &map,
                MidiMessage::Note {
                    channel: 0,
                    note: 22,
                    velocity: 1
                },
                None,
                &[mono]
            ),
            MidiOutcome::Action(UiAction::Mono {
                uid: "in-mic".to_string(),
                mono: false
            })
        );
    }

    #[test]
    fn learning_captures_the_first_bindable_message() {
        let map = map();
        let strips = [strip("in-mic")];
        // Nothing mapped on CC 5: ignored without learn mode.
        assert_eq!(
            decode(
                &map,
                MidiMessage::Control {
                    channel: 0,
                    control: 5,
                    value: 100
                },
                None,
                &strips,
            ),
            MidiOutcome::Ignored
        );
        // In learn mode, the first message defines the mapping — and it is
        // learned *instead of* being performed.
        assert_eq!(
            decode(
                &map,
                MidiMessage::Control {
                    channel: 2,
                    control: 5,
                    value: 100
                },
                Some(("in-mic", MidiTarget::Mute)),
                &strips,
            ),
            MidiOutcome::Learned {
                uid: "in-mic".to_string(),
                target: MidiTarget::Mute,
                binding: Binding::control(2, 5)
            }
        );
        // A note is learned as a `note_on` binding (both note shapes).
        assert_eq!(
            decode(
                &map,
                MidiMessage::Note {
                    channel: 0,
                    note: 99,
                    velocity: 0
                },
                Some(("in-mic", MidiTarget::Volume)),
                &strips,
            ),
            MidiOutcome::Learned {
                uid: "in-mic".to_string(),
                target: MidiTarget::Volume,
                binding: Binding::note(0, 99)
            }
        );
    }

    #[test]
    fn bindings_round_trip_through_the_python_json_shape() {
        let binding = Binding::control(0, 61);
        let json = binding.to_json();
        assert_eq!(json["type"], "control_change");
        assert_eq!(json["channel"], 0);
        assert_eq!(json["control"], 61);
        assert_eq!(Binding::from_json(&json), Some(binding));
        let note = Binding::note(1, 24);
        let json = note.to_json();
        assert_eq!(json["type"], "note_on");
        assert_eq!(json["note"], 24);
        assert_eq!(Binding::from_json(&json), Some(note));
        // A malformed mapping is dropped rather than swallowing messages.
        for bad in [
            serde_json::json!({"type": "control_change", "channel": 0}),
            serde_json::json!({"type": "note_on", "channel": 0, "control": 3}),
            serde_json::json!({"type": "pitchwheel", "channel": 0, "control": 3}),
            serde_json::json!({"type": "control_change", "channel": 20, "control": 3}),
            serde_json::json!(null),
        ] {
            assert_eq!(Binding::from_json(&bad), None, "{bad}");
        }
    }

    #[test]
    fn the_led_feedback_uses_the_binding_shape() {
        // A note button lights with velocity 127 and clears with 0.
        assert_eq!(
            feedback(Binding::note(0, 24), true),
            MidiMessage::Note {
                channel: 0,
                note: 24,
                velocity: 127
            }
        );
        assert_eq!(
            feedback(Binding::note(0, 24), false),
            MidiMessage::Note {
                channel: 0,
                note: 24,
                velocity: 0
            }
        );
        // A CC button uses the CC value.
        assert_eq!(
            feedback(Binding::control(1, 5), true),
            MidiMessage::Control {
                channel: 1,
                control: 5,
                value: 127
            }
        );
        // The button states of every strip are mirrored in one pass.
        let mut muted = strip("in-mic");
        muted.mute = true;
        let strips = [muted, strip("out-hp")];
        let bindings = HashMap::from([
            (
                "in-mic".to_string(),
                vec![
                    (MidiTarget::Mute, Binding::note(0, 24)),
                    (MidiTarget::Mono, Binding::note(0, 22)),
                    (MidiTarget::Volume, Binding::control(0, 61)),
                ],
            ),
            (
                "out-hp".to_string(),
                vec![(MidiTarget::Mute, Binding::note(1, 10))],
            ),
        ]);
        let messages = button_feedback(&strips, &bindings);
        // Mute (on) + mono (off) for in-mic, mute (off) for out-hp.  The volume
        // mapping is a knob: it has no LED.
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(messages.contains(&MidiMessage::Note {
            channel: 0,
            note: 24,
            velocity: 127
        }));
        assert!(messages.contains(&MidiMessage::Note {
            channel: 0,
            note: 22,
            velocity: 0
        }));
        assert!(messages.contains(&MidiMessage::Note {
            channel: 1,
            note: 10,
            velocity: 0
        }));
    }

    #[test]
    fn the_table_is_keyed_by_type_channel_and_identifier() {
        let map = map();
        assert_eq!(map.len(), 4);
        assert!(!map.is_empty());
        // Same CC on another channel is a different key.
        assert_eq!(
            map.lookup(MidiMessage::Control {
                channel: 0,
                control: 62,
                value: 0
            }),
            None
        );
        // A CC key and a note key never collide, even with the same number.
        let colliding = MidiMap::new([
            ("a", MidiTarget::Volume, Binding::control(0, 7)),
            ("b", MidiTarget::Mute, Binding::note(0, 7)),
        ]);
        assert_eq!(
            colliding.lookup(MidiMessage::Control {
                channel: 0,
                control: 7,
                value: 1
            }),
            Some(("a", MidiTarget::Volume))
        );
        assert_eq!(
            colliding.lookup(MidiMessage::Note {
                channel: 0,
                note: 7,
                velocity: 1
            }),
            Some(("b", MidiTarget::Mute))
        );
        // From strips + config entries (the UI's rebuild).
        let strips = [strip("in-mic"), strip("out-hp")];
        let bindings = HashMap::from([(
            "in-mic".to_string(),
            vec![(MidiTarget::Volume, Binding::control(0, 61))],
        )]);
        let map = MidiMap::from_strips(&strips, &bindings);
        assert_eq!(map.len(), 1);
        assert_eq!(MidiTarget::Volume.config_key(), "midi_volume");
        assert_eq!(
            MidiTarget::from_config_key("midi_mono"),
            Some(MidiTarget::Mono)
        );
    }

    #[test]
    fn the_config_keys_and_names_match_the_python_side() {
        assert_eq!(MidiTarget::Volume.as_str(), "volume");
        assert_eq!(MidiTarget::Mute.config_key(), "midi_mute");
        assert_eq!(MessageKind::Note.as_config_str(), "note_on");
        assert_eq!(
            MessageKind::from_config_str("note_off"),
            Some(MessageKind::Note)
        );
        assert_eq!(MessageKind::from_config_str("clock"), None);
    }

    #[test]
    fn a_binding_shows_itself_the_way_a_controller_labels_it() {
        // What the MIDI button flashes after a learn, and what the status notice
        // says (`ch.` is 1-based: the file stores 0).
        assert_eq!(Binding::control(0, 19).short_label(), "CC 19");
        assert_eq!(Binding::note(0, 24).short_label(), "N 24");
        assert_eq!(Binding::control(4, 61).describe(), "CC 61 ch.5");
        assert_eq!(Binding::note(1, 3).describe(), "N 3 ch.2");
    }

    #[test]
    fn a_bare_number_mapping_is_understood_and_never_written() {
        // Neither side writes this shape, but a hand-edited config can hold one
        // (and the Qt UI crashes on it).  Channel 0 + the target's natural shape.
        let value = serde_json::json!(61);
        assert_eq!(
            Binding::from_legacy_number(MidiTarget::Volume, &value),
            Some(Binding::control(0, 61))
        );
        assert_eq!(
            Binding::from_legacy_number(MidiTarget::Mute, &value),
            Some(Binding::note(0, 61))
        );
        assert_eq!(
            Binding::from_legacy_number(MidiTarget::Mono, &value),
            Some(Binding::note(0, 61))
        );
        // Out of range / not a number: nothing to honour.
        for bad in [
            serde_json::json!(128),
            serde_json::json!(-1),
            serde_json::json!(61.5),
            serde_json::json!("61"),
            serde_json::json!(null),
            serde_json::json!({"control": 61}),
        ] {
            assert_eq!(
                Binding::from_legacy_number(MidiTarget::Volume, &bad),
                None,
                "{bad}"
            );
        }
        // The legacy *read* never changes the shape we write.
        assert_eq!(Binding::control(0, 61).to_json()["type"], "control_change");
    }

    #[test]
    fn the_port_is_chosen_like_a_controller_owner_expects() {
        let midimix = vec![
            "Midi Through:Midi Through Port-0 14:0".to_string(),
            "MIDI Mix:MIDI Mix MIDI 1 24:0".to_string(),
            "Some Synth:Synth MIDI 1 28:0".to_string(),
        ];
        // A configured port wins, and is matched exactly (no fuzzy matching: the
        // stored name is what the ALSA client announces).
        assert_eq!(
            choose_port(&midimix, Some("Some Synth:Synth MIDI 1 28:0")),
            Some("Some Synth:Synth MIDI 1 28:0".to_string())
        );
        // A configured port that is *absent* yields nothing: an unplugged
        // controller must not be replaced by another device behind the user's
        // back (the runtime keeps retrying this name).
        assert_eq!(choose_port(&midimix, Some("MIDI Mix:MIDI Mix MIDI 1 24:0")), Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string()));
        assert_eq!(choose_port(&midimix, Some("Gone:Whatever 99:0")), None);
        // Nothing configured: the MIDI Mix is picked, like the Qt `"mix" in name`
        // rule.
        assert_eq!(
            choose_port(&midimix, None),
            Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string())
        );
        // A single anonymous controller is used (there is no ambiguity), two are
        // not (the user picks).
        assert_eq!(
            choose_port(&["Keystation 49 MIDI 1".to_string()], None),
            Some("Keystation 49 MIDI 1".to_string())
        );
        assert_eq!(
            choose_port(
                &[
                    "Keystation 49 MIDI 1".to_string(),
                    "Nanokontrol MIDI 1".to_string()
                ],
                None
            ),
            None
        );
        assert_eq!(choose_port(&[], None), None);
        // The case-insensitive match (`midi mix` wins over a bare `mix`).
        let names = vec![
            "Remix Controller:Port 1".to_string(),
            "akai midi mix:port 2".to_string(),
        ];
        assert_eq!(choose_port(&names, None), Some("akai midi mix:port 2".to_string()));
    }

    #[test]
    fn an_absent_controller_is_retried_every_two_seconds() {
        let start = Instant::now();
        let mut runtime = MidiRuntime::new(Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string()));
        assert_eq!(runtime.state(), &PortState::Closed);
        // First attempt: immediately due.
        assert_eq!(
            runtime.attempt(start),
            Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string())
        );
        runtime.on_failed("no such port");
        assert_eq!(
            runtime.last_error(),
            Some("no such port"),
            "the reason is what the status notice shows"
        );
        // Not due again before the interval…
        assert_eq!(runtime.attempt(start + Duration::from_millis(1999)), None);
        // …and due exactly at the interval (2 s, the documented retry).
        assert_eq!(
            runtime.attempt(start + RETRY_INTERVAL),
            Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string())
        );
        // Reconnecting: the port answers again.
        runtime.on_opened("MIDI Mix:MIDI Mix MIDI 1 24:0");
        assert!(runtime.is_open());
        assert_eq!(runtime.state(), &PortState::Open);
        // An open port is not re-opened every frame.
        assert_eq!(runtime.attempt(start + RETRY_INTERVAL * 10), None);
        // Unplugged: back in the retry loop, same controller.
        runtime.on_lost("port closed");
        assert!(!runtime.is_open());
        assert_eq!(runtime.desired(), Some("MIDI Mix:MIDI Mix MIDI 1 24:0"));
        assert!(runtime.attempt(start + RETRY_INTERVAL * 11).is_some());
    }

    #[test]
    fn switching_the_port_restarts_the_retry_clock() {
        let start = Instant::now();
        let mut runtime = MidiRuntime::new(Some("a".to_string()));
        assert!(runtime.attempt(start).is_some());
        runtime.on_opened("a");
        // Same port: no change, the connection is kept.
        assert!(!runtime.set_desired(Some("a".to_string())));
        assert!(runtime.is_open());
        // Another port: closed at once, and the new one is due immediately.
        assert!(runtime.set_desired(Some("b".to_string())));
        assert!(!runtime.is_open());
        assert_eq!(runtime.attempt(start), Some("b".to_string()));
        // `None` = MIDI off: nothing is ever attempted again.
        assert!(runtime.set_desired(None));
        assert_eq!(runtime.attempt(start + RETRY_INTERVAL * 5), None);
        // A stray "opened" for a port we no longer want is not a success.
        runtime.set_desired(Some("c".to_string()));
        runtime.on_opened("b");
        assert!(!runtime.is_open());
    }

    #[test]
    fn the_status_line_explains_a_missing_controller() {
        let start = Instant::now();
        let mut runtime = MidiRuntime::new(Some("MIDI Mix".to_string()));
        assert_eq!(runtime.status_line(), None, "nothing to say yet");
        let _ = runtime.attempt(start);
        runtime.on_opened("MIDI Mix");
        assert_eq!(runtime.status_line().as_deref(), Some("MIDI: MIDI Mix"));
        runtime.on_lost("port closed");
        let line = runtime.status_line().expect("a lost port is explained");
        assert!(line.contains("MIDI Mix"), "{line}");
        assert!(line.contains("retrying"), "{line}");
        // No controller selected: the status notice says nothing.
        runtime.set_desired(None);
        assert_eq!(runtime.status_line(), None);
    }
}
