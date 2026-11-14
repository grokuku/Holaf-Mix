//! [`AppState`] — everything the UI draws from — plus the action model.
//!
//! Three ideas keep this crate testable:
//!
//! 1. **`render` is the only entry point.** It takes `&mut AppState` and a
//!    `&mut Ui`; the eframe binary, the headless PNG snapshot and the tests all
//!    call the very same function, so what the snapshot shows is what the app
//!    draws.  There is no second code path for "preview" or "screenshot mode".
//! 2. **Views return [`UiAction`]s instead of mutating.** A view may only
//!    *describe* what the user asked for.  [`AppState::apply`] performs it on
//!    the model, queues the matching [`EngineCommand`] (throttled/debounced) and
//!    schedules the config write — the same contract the PySide signals
//!    (`volume_changed`, `effect_toggled`, …) had, except it is plain data we
//!    can assert on in a unit test.
//! 3. **The engine is optional and never blocks.** With no [`EngineBridge`]
//!    (snapshot, unit tests, `--no-default-features`) the VUs are animated by a
//!    deterministic function of time; with one, the levels come from the engine
//!    and a dead/missing daemon is a banner, not a crash.
//!
//! # Connection states
//!
//! [`EngineState`] is what the banner in [`crate::views::main_window`] shows:
//!
//! | state | meaning | what the UI does |
//! |---|---|---|
//! | `None` | no backend compiled in (`--no-default-features`, snapshot) | demo meters, actions logged only |
//! | `Connecting` | the pipewire backend is connecting | actions are queued |
//! | `Connected` | `apply_snapshot` accepted | the real mixer |
//! | `Disconnected(reason)` | no daemon / the worker died | **disconnected mode**: the strip list stays editable, every audio action is logged + refused, the reason is shown |
//!
//! # Persistence
//!
//! [`AppState::from_config`] loads the real `config.json` through
//! [`crate::config::UiConfig`] (migrations included) and every persistent action
//! schedules a debounced atomic write that **preserves the keys the Rust model
//! does not know** (`midi_*`, `assigned_apps`, `window_geometry`, …).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use holaf_core::model::{EffectKey, Strip, StripType};

use crate::apps::{self, AppList, AppSelection};
use crate::bridge::{EngineBridge, EngineCommand, EngineEvent};
use crate::coalesce::Throttled;
use crate::config::{SaveDebounce, UiConfig, WindowGeometry};
use crate::devices::{self, DevicePresence};
use crate::diagnostics::{DiagnosticsState, Inputs, Report};
use crate::dialogs::{AddStripDialog, DeleteConfirm, DevicePicker, FxSettings};
use crate::engine_json::DeviceInfo;
use crate::meters::{Envelope, MeterBoard, METER_INTERVAL};
use crate::midi::{Binding, MidiMap, MidiMessage, MidiOutcome, MidiTarget};
use crate::theme::images::ImageCache;
use crate::theme::resolve::{resolve, ResolvedStripStyle, StripDescriptor};
use crate::theme::ThemeFile;

/// Debounce for the device-change restart, mirroring Qt's
/// `DEBOUNCE_ENGINE_RESTART_MS` (rapid changes coalesce into one graph rebuild).
pub const ENGINE_RESTART_DEBOUNCE: Duration = Duration::from_millis(200);

/// Debounce for FX parameter pushes (the Qt dialog emits once on close; the Rust
/// one is live, so the drag is coalesced instead).
pub const FX_PARAMS_DEBOUNCE: Duration = Duration::from_millis(200);

/// How many undo records are kept for rollback.  A failure is reported within
/// one engine round-trip, so a handful would do; 64 is cheap insurance.
const UNDO_DEPTH: usize = 64;

/// How many frames the window size is observed before a change counts as a
/// resize gesture ([`AppState::note_window_size`]).
///
/// The first frames are the WM's business: eframe creates the window, the WM
/// applies `MinInnerSize` / the previous position and may hand back a size that
/// is not the one we asked for.  That settle is not a user gesture, so it must
/// not be persisted.
pub const RESIZE_ARM_FRAMES: u32 = 5;

/// Height difference (px) below which an observed change is jitter, not a drag.
/// The same 2 px the Qt UI used to decide the size had really changed.
pub const RESIZE_JITTER: f32 = 2.0;

/// How long the MIDI button shows the number of the CC it just learned.
///
/// The Qt UI turned the button back to plain `MIDI` with no confirmation at all,
/// so the user had to *guess* whether the learn had taken.  A short flash
/// (`CC 19`) answers that without a dialog.
pub const MIDI_FLASH_INTERVAL: Duration = Duration::from_millis(1500);

/// Everything the user can ask the UI to do.
///
/// One variant per Qt signal of `StripWidget` + `MainWindow`, plus the dialog
/// openers/confirmations.  Keeping them as data (instead of direct mutation) is
/// what lets `AppState::apply` stay a pure, testable function.
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    /// Fader moved (`volume_changed`).  Coalesced to 25 Hz before it reaches the
    /// engine ([`crate::coalesce::Throttled`]).
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
    /// One control port changed in the FX settings dialog (live, debounced).
    FxParam {
        uid: String,
        effect: EffectKey,
        param: String,
        value: f32,
    },
    /// The FX settings dialog was opened (double click / right click on a FX
    /// button).  `None` for the engine: it only opens a window.
    OpenFxSettings { uid: String, effect: EffectKey },
    /// Routing chip clicked (`route_changed`).
    Route {
        uid: String,
        target: String,
        active: bool,
    },
    /// "Default Sink" checkbox (`default_changed`).
    DefaultSink { uid: String, is_default: bool },
    /// A device was picked in the combo (`device_changed`): a structural change,
    /// so it schedules the debounced `apply_snapshot`.
    DeviceChanged { uid: String, device: Option<String> },
    /// MIDI button clicked (`midi_learn_requested` / learn mode toggled).
    MidiLearn { uid: String, learning: bool },
    /// The strip's MIDI menu asked to learn **one** property (`Learn Volume` /
    /// `Learn Mute` / `Learn Mono`): the next bindable message is stored there.
    MidiLearnTarget { uid: String, target: MidiTarget },
    /// `Cancel learn` — back to playing, nothing stored.
    MidiLearnCancel,
    /// `Clear Mappings` of the strip's MIDI menu.
    ClearMidiMappings { uid: String },
    /// `MIDI port...` in the window menu: open the port picker.
    OpenMidiPortPicker,
    /// A port was picked in that dialog (`None` = disconnect).
    MidiPortSelected { name: Option<String> },
    /// "×" clicked (`delete_requested`): asks for confirmation first.
    RequestDelete { uid: String },
    /// Delete confirmed (`QMessageBox` "Yes") — this one removes the strip.
    Delete { uid: String },
    /// Label double-clicked (`label_changed` dialog opened).
    BeginRename { uid: String },
    /// Rename dialog confirmed.
    Rename { uid: String, label: String },
    /// "+" in a section header (`on_add_clicked`): opens the add dialog.
    BeginAddStrip { kind: StripType },
    /// Add-strip dialog confirmed.  `label: None` = the automatic name
    /// (`Input 5`), which is what the tests and the harness use.
    AddStrip {
        kind: StripType,
        label: Option<String>,
    },
    /// "SELECT APPS" clicked (`app_selection_requested`): opens the check-box
    /// dialog over the running applications.
    SelectApps { uid: String },
    /// The app-selection dialog was confirmed: the strip's `assigned_apps`.
    SelectAppsDone { uid: String, names: Vec<String> },
    /// Context menu → "Restart audio engine": tear the graph down, reconnect
    /// and re-apply the snapshot (engine thread, anti-reentrance).
    RestartEngine,
    /// Context menu → "Debug window": open the diagnostics dialog.
    OpenDiagnostics,
    /// Context menu → "Open project page": hand the URL to the browser.
    OpenProjectPage,
    /// A right click asked for a context menu (see [`crate::menus`]).
    OpenContextMenu {
        kind: crate::menus::ContextMenuKind,
        pos: egui::Pos2,
    },
    /// The always-on-top flag changed (`ui_rust.always_on_top`).
    SetAlwaysOnTop { value: bool },
    /// Device combo clicked: asks the engine for its device list and opens the
    /// pop-up under the button (`dialogs::draw_device_picker`).
    OpenDevicePicker { uid: String },
}

/// What the engine side of the app currently is, as the banner shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineState {
    /// No backend compiled in (`--no-default-features`) or intentionally absent
    /// (snapshot binary): the VUs follow [`AppState::animate`].
    None,
    /// A bridge exists and is connecting.
    Connecting,
    /// `apply_snapshot` was accepted by the backend.
    Connected,
    /// The backend could not be reached (or the worker died).  The reason is
    /// shown to the user; audio actions are logged and ignored.
    Disconnected(String),
}

impl EngineState {
    pub fn is_disconnected(&self) -> bool {
        matches!(self, EngineState::Disconnected(_))
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            EngineState::Disconnected(reason) => Some(reason),
            _ => None,
        }
    }
}

/// One reversible model change, kept until the engine confirms (or rejects) it.
///
/// This is what makes optimistic UI safe: a command the worker refuses is
/// reverted to the value the widget had *before* the click, instead of leaving
/// the strip showing a volume the daemon never applied.
#[derive(Debug, Clone)]
enum Undo {
    Volume {
        uid: String,
        previous: f32,
    },
    Mute {
        uid: String,
        previous: bool,
    },
    Mono {
        uid: String,
        previous: bool,
    },
    FxActive {
        uid: String,
        effect: EffectKey,
        previous: bool,
    },
    FxParams {
        uid: String,
        effect: EffectKey,
        previous: IndexMap<String, f32>,
    },
    Route {
        uid: String,
        previous: Vec<String>,
    },
    DefaultSink {
        previous: Option<String>,
    },
    Device {
        uid: String,
        previous: Option<String>,
    },
}

impl Undo {
    /// Does this record belong to the command that just failed?
    fn matches(&self, command: &EngineCommand) -> bool {
        match (self, command) {
            (Undo::Volume { uid, .. }, EngineCommand::SetVolume { uid: other, .. }) => uid == other,
            (Undo::Mute { uid, .. }, EngineCommand::SetMute { uid: other, .. }) => uid == other,
            (Undo::Mono { uid, .. }, EngineCommand::SetMono { uid: other, .. }) => uid == other,
            (
                Undo::FxActive { uid, effect, .. },
                EngineCommand::SetEffect {
                    uid: other,
                    effect: other_effect,
                    ..
                },
            ) => uid == other && effect == other_effect,
            (
                Undo::FxParams { uid, effect, .. },
                EngineCommand::SetFxParams {
                    uid: other,
                    effect: other_effect,
                    ..
                },
            ) => uid == other && effect == other_effect,
            (Undo::Route { uid, .. }, EngineCommand::SetRoute { source, .. }) => uid == source,
            (Undo::DefaultSink { .. }, EngineCommand::SetDefaultSink { .. }) => true,
            (Undo::Device { uid, .. }, EngineCommand::SetDevice { uid: other, .. }) => uid == other,
            _ => false,
        }
    }
}

/// The brief confirmation painted on a strip's MIDI button after a learn
/// (`CC 19` / `N 24`), until `until`.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiFlash {
    pub uid: String,
    /// What the button shows (`Binding::short_label`).
    pub text: String,
    pub until: Instant,
}

/// The UI state: theme, strips, meter levels, caches, dialogs and the engine.
pub struct AppState {
    /// Active theme file (tokens + per-strip overrides).
    pub theme: ThemeFile,
    /// Strips, in `config.json` order (inputs and outputs interleaved like the
    /// model: the view partitions them into the two columns).
    pub strips: Vec<Strip>,
    /// Decoded theme images.
    pub image_cache: ImageCache,
    /// Decay envelope per strip uid — the levels the VU bars draw.
    levels: HashMap<String, Envelope>,
    /// When the meters were last polled (decay is wall-clock based).
    last_meter_tick: Instant,
    /// Strips the board has no source for (warned once).
    meter_missing: HashSet<String>,
    /// Actions the views produced this frame, waiting for the engine.
    pending_actions: Vec<UiAction>,
    /// Last status message (status bar).
    pub status: Option<String>,
    /// `Some((uid, target))` while a MIDI learn is armed.
    pub midi_learning: Option<(String, MidiTarget)>,
    /// `Some(uid)` while the rename dialog is open.
    pub renaming: Option<String>,
    /// Rename dialog buffer.
    pub rename_buffer: String,
    /// Optional engine bridge (absent in the demo/snapshot builds).
    pub bridge: Option<EngineBridge>,
    /// Connection state (drives the banner).
    pub engine: EngineState,
    /// Seconds since start, driven by `ui.input(|i| i.time)` — time-based
    /// animation so a snapshot at a fixed time is reproducible.
    pub time: f32,
    /// Config store; `None` = no persistence (demo, snapshot, `--no-config`).
    pub config: Option<UiConfig>,
    /// The window height **to persist** — the one from `config.json`, replaced
    /// only by a real resize gesture (see [`Self::note_window_size`]).
    window: Option<(f32, f32)>,
    /// The window size observed on the last frame (what the WM actually gave us,
    /// which is *not* necessarily what we asked for).
    window_observed: Option<(f32, f32)>,
    /// How many frames the window size has been observed for; used to ignore the
    /// startup settle (first frames) — see [`RESIZE_ARM_FRAMES`].
    window_frames: u32,
    /// The clock the throttles/debounces use (`Instant::now()` in the app, set
    /// explicitly by the tests).
    now: Instant,
    /// Fader volume coalescing (25 Hz, last value wins).
    volumes: Throttled<f32>,
    /// Debounce of the FX parameter pushes.
    fx_debounce: SaveDebounce,
    /// Values waiting for their debounce window.
    fx_pending: HashMap<(String, EffectKey), IndexMap<String, f32>>,
    /// Debounce of the device-change graph rebuild (`apply_snapshot`).
    device_restart: SaveDebounce,
    /// Reversible changes whose engine command is still in flight.
    undo: Vec<Undo>,
    /// Devices the engine last reported (device picker).
    pub engine_devices: Vec<DeviceInfo>,
    /// Strips whose `device_name` is not in [`Self::engine_devices`] (the view
    /// paints `(Not Found)`, like the Qt combo did).
    pub missing_devices: HashSet<String>,
    /// Why the device list is empty/partial, when the engine said so.
    pub devices_error: Option<String>,
    /// FX settings dialog state.
    pub fx_dialog: Option<FxSettings>,
    /// Add-strip dialog state.
    pub add_dialog: Option<AddStripDialog>,
    /// Delete confirmation state (`Some(uid)` while the dialog is open).
    pub delete_confirm: Option<DeleteConfirm>,
    /// Device pop-up state (non-modal, anchored under the strip's combo button).
    pub device_picker: Option<DevicePicker>,
    /// `uid → assigned application names` (the `assigned_apps` key of
    /// `config.json`, which the `Strip` model does not carry).
    pub app_assignments: HashMap<String, Vec<String>>,
    /// The applications the engine last reported, stub flag included.
    pub app_list: AppList,
    /// Why the app list could not be read, when the engine said so.
    pub apps_error: Option<String>,
    /// "SELECT APPS" dialog state.
    pub apps_dialog: Option<AppSelection>,
    /// `uid → bus node name` from the engine's `status` (`Holaf_Bus_<Label>`),
    /// the sink an assigned application must be moved to.
    pub node_names: HashMap<String, String>,
    /// The last `status()` payload (the diagnostics report is built from it).
    pub backend_status: Option<serde_json::Value>,
    /// Why `status()` failed, when it did.
    pub status_error: Option<String>,
    /// The 5 s application-routing enforcement timer (Qt
    /// `ENFORCE_ROUTING_INTERVAL_MS`).
    enforce: SaveDebounce,
    /// "Restart audio engine" in progress (anti-reentrance, see
    /// [`Self::restart_engine`]).
    pub restarting: bool,
    /// Diagnostics window state (`None` = closed).
    pub diagnostics: Option<DiagnosticsState>,
    /// `ui_rust.always_on_top` — the Rust-only window flag (Qt used
    /// `WindowStaysOnTopHint` unconditionally).
    pub always_on_top: bool,
    /// The open context menu, if any ([`crate::menus`]).
    pub context_menu: Option<crate::menus::ContextMenu>,
    /// The `WindowLevel` last sent to the viewport (`None` = never sent).
    pub applied_window_level: Option<bool>,
    /// Per-strip MIDI bindings (`midi_volume` / `midi_mute` / `midi_mono` of
    /// `config.json`, which the `Strip` model does not carry).
    pub midi_bindings: HashMap<String, Vec<(MidiTarget, Binding)>>,
    /// The keyed lookup table, rebuilt whenever a binding changes.
    pub midi_map: MidiMap,
    /// LED messages waiting to be written to the controller.
    pub midi_feedback: Vec<MidiMessage>,
    /// Per-strip volume throttle for a knob that sends faster than the model
    /// needs to change.
    midi_last_volume: HashMap<(String, MidiTarget), Instant>,
    /// The MIDI input the user selected (`None` = no controller).
    pub midi_port_name: Option<String>,
    /// The MIDI input the user **wants** (`ui_rust.midi_port`, or the automatic
    /// pick at startup).  The shell opens it and re-opens it when it disappears;
    /// it is what the port picker edits.
    pub midi_port_request: Option<String>,
    /// May the shell pick a controller by itself?  `true` only while
    /// `ui_rust.midi_port` is **absent** from the config: once the user picked a
    /// port (or explicitly picked "No controller"), that decision is kept.
    pub midi_port_auto: bool,
    /// Every MIDI input the shell last saw.  Empty when the build has no `midi`
    /// feature (`midi_ports_error` then says so) — the picker needs a list, and
    /// only the shell can ask ALSA.
    pub midi_ports: Vec<String>,
    /// Why the port list is empty, when the shell could not read it.
    pub midi_ports_error: Option<String>,
    /// Is a controller answering right now?  Maintained by the shell (the
    /// library never touches a device): the picker and the status line need it.
    pub midi_connected: bool,
    /// The port picker dialog, when open.
    pub midi_port_picker: Option<crate::dialogs::MidiPortPicker>,
    /// Confirmation shown on the MIDI button after a learn (`CC 19`).
    pub midi_flash: Option<MidiFlash>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::demo()
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl AppState {
    /// The Python app's default configuration, as `holaf_core` model strips.
    ///
    /// Used by the snapshot binary and the unit tests; the app itself always
    /// goes through [`AppState::from_config`].
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
        default.effects.insert(
            EffectKey::Eq,
            holaf_core::model::EffectState::inactive_with_defaults(EffectKey::Eq),
        );
        default.routes = vec!["out-casque".to_string()];
        strips.push(default);

        let mut web = Strip::with_uid("in-web", "Web", StripType::Input);
        web.volume = 0.7;
        web.effects.insert(
            EffectKey::Eq,
            holaf_core::model::EffectState::inactive_with_defaults(EffectKey::Eq),
        );
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

    /// Build a state from an existing strip list and theme (no persistence).
    pub fn from_strips(strips: Vec<Strip>, theme: ThemeFile) -> Self {
        let now = Instant::now();
        Self {
            theme,
            strips,
            image_cache: ImageCache::new(),
            levels: HashMap::new(),
            last_meter_tick: now,
            meter_missing: HashSet::new(),
            pending_actions: Vec::new(),
            status: None,
            midi_learning: None,
            renaming: None,
            rename_buffer: String::new(),
            bridge: None,
            engine: EngineState::None,
            time: 0.0,
            config: None,
            window: None,
            window_observed: None,
            window_frames: 0,
            now,
            volumes: Throttled::new(METER_INTERVAL),
            fx_debounce: SaveDebounce::new(FX_PARAMS_DEBOUNCE),
            fx_pending: HashMap::new(),
            device_restart: SaveDebounce::new(ENGINE_RESTART_DEBOUNCE),
            undo: Vec::new(),
            engine_devices: Vec::new(),
            missing_devices: HashSet::new(),
            devices_error: None,
            fx_dialog: None,
            add_dialog: None,
            delete_confirm: None,
            device_picker: None,
            app_assignments: HashMap::new(),
            app_list: AppList::default(),
            apps_error: None,
            apps_dialog: None,
            node_names: HashMap::new(),
            backend_status: None,
            status_error: None,
            enforce: SaveDebounce::new(apps::ENFORCE_INTERVAL),
            restarting: false,
            diagnostics: None,
            always_on_top: true,
            context_menu: None,
            applied_window_level: None,
            midi_bindings: HashMap::new(),
            midi_map: MidiMap::default(),
            midi_feedback: Vec::new(),
            midi_last_volume: HashMap::new(),
            midi_port_name: None,
            midi_port_request: None,
            midi_port_auto: true,
            midi_ports: Vec::new(),
            midi_ports_error: None,
            midi_connected: false,
            midi_port_picker: None,
            midi_flash: None,
        }
    }

    /// **The real startup path**: load `config.json`, its window geometry and the
    /// active theme.
    ///
    /// A config is never allowed to prevent the UI from starting (that was an
    /// explicit Python-side bug report):
    ///
    /// | situation | result |
    /// |---|---|
    /// | file present and valid | strips + geometry + theme, persisted on change |
    /// | file absent | **no strips**, no write (the window is then the 160+160 minimum), warning in the status bar |
    /// | file corrupt | same, with the parse error in the status bar and a `log::warn!` |
    ///
    /// No panic, and never an empty `apply_snapshot` pushed to a running daemon
    /// when there was no config to load (see [`Self::attach_engine`]).
    pub fn from_config(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let mut state = Self::from_strips(Vec::new(), ThemeFile::holaf_dark());
        match UiConfig::open(path) {
            Ok(config) => {
                let strips = config.strips();
                let theme = load_theme(config.theme(), path);
                let geometry = config.geometry();
                state.theme = theme;
                state.strips = strips;
                if let Some(geometry) = geometry {
                    state.window = Some((
                        geometry
                            .width
                            .unwrap_or(state.theme.theme.dimensions.window_width),
                        geometry.height,
                    ));
                }
                if config.loaded() {
                    log::info!(
                        "loaded {} strips from {}",
                        state.strips.len(),
                        config.path().display()
                    );
                } else {
                    let message = format!(
                        "no config at {} - starting with an empty mixer",
                        config.path().display()
                    );
                    log::warn!("{message}");
                    state.status = Some(message);
                }
                state.app_assignments = config.assigned_apps();
                state.midi_bindings = config.midi_bindings();
                state.midi_map = MidiMap::from_strips(&state.strips, &state.midi_bindings);
                state.always_on_top = config.ui_rust_bool("always_on_top").unwrap_or(true);
                // The controller the user picked last time (`ui_rust.midi_port`).
                // Python has no such key — it auto-opened the first port whose
                // name contained "mix" at every start — so this is a Rust-only
                // preference, and its absence is what triggers the automatic
                // pick in the shell.
                state.midi_port_request = config.ui_rust_string("midi_port");
                // Only an *absent* key leaves the automatic pick enabled: an
                // explicit "No controller" (`null`) is a decision, not a default.
                state.midi_port_auto = !config.ui_rust_has("midi_port");
                if let Some(name) = &state.midi_port_request {
                    log::info!("midi: configured controller {name:?}");
                }
                state.config = Some(config);
            }
            Err(err) => {
                let message = format!("cannot load {}: {err}", path.display());
                log::warn!("{message} - starting with an empty mixer (the file is left untouched)");
                state.status = Some(message);
            }
        }
        state.animate(1.0);
        state
    }

    /// Attach the engine and push the loaded strips to it.
    ///
    /// The initial `apply_snapshot` is only sent when we actually have a config
    /// to apply (`UiConfig::loaded`) or the model is non-empty: a first run with
    /// no config must not tear down a graph the daemon is serving for the Qt UI.
    pub fn attach_engine(&mut self, bridge: EngineBridge) {
        let should_push =
            self.config.as_ref().is_some_and(|config| config.loaded()) || !self.strips.is_empty();
        self.engine = EngineState::Connecting;
        self.bridge = Some(bridge);
        if should_push {
            self.send_command(EngineCommand::SyncSnapshot(self.strips.clone()));
        }
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
            .map(Envelope::level)
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
        self.levels
            .entry(uid.into())
            .or_default()
            .set([levels[0], levels[1]]);
    }

    /// Feed one engine reading through the decay envelope.
    fn feed_levels(&mut self, uid: &str, levels: [f32; 2], dt: Duration) {
        self.levels
            .entry(uid.to_string())
            .or_default()
            .update(levels, dt);
    }

    /// Advance the time-based animation.
    ///
    /// With no engine attached the meters follow a deterministic function of
    /// `time` (two out-of-phase sines per strip, scaled by its volume), so the
    /// headless snapshot always shows the same, plausible picture and a test
    /// can assert on it.  With a bridge attached, real levels win and this only
    /// records `time`.
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

    /// The clock the throttles and debounces use.  The app calls
    /// [`Self::tick`] every frame with `Instant::now()`; tests set it explicitly
    /// so a 25 Hz rule is asserted without sleeping.
    pub fn set_clock(&mut self, now: Instant) {
        self.now = now;
    }

    pub fn clock(&self) -> Instant {
        self.now
    }

    /// Apply one action: mutate the model (recording what to undo), queue it for
    /// the engine, and schedule the config write.
    ///
    /// Unknown uids are ignored (a strip deleted between two frames must not
    /// panic the next click), which is what `self.widgets.get(uid)` did in
    /// Python.
    pub fn apply(&mut self, action: UiAction) {
        let now = self.now;
        if let Some(undo) = self.undo_for(&action) {
            self.push_undo(undo);
        }

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
                // The controller's lamp follows the strip, whatever moved it (a
                // MIDI button, the mouse, or an undo).  Qt did this in
                // `_on_mute_toggled`; centralising it here covers every path.
                self.queue_midi_led(uid, MidiTarget::Mute, *mute);
            }
            UiAction::Mono { uid, mono } => {
                if let Some(index) = self.strip_index(uid) {
                    self.strips[index].is_mono = *mono;
                }
                self.queue_midi_led(uid, MidiTarget::Mono, *mono);
            }
            UiAction::ToggleFx {
                uid,
                effect,
                active,
            } => {
                if let Some(index) = self.strip_index(uid) {
                    let entry = self.strips[index]
                        .effects
                        .entry(*effect)
                        .or_insert_with(|| {
                            holaf_core::model::EffectState::inactive_with_defaults(*effect)
                        });
                    entry.active = *active;
                }
            }
            UiAction::FxParam {
                uid,
                effect,
                param,
                value,
            } => {
                if let Some(index) = self.strip_index(uid) {
                    let entry = self.strips[index]
                        .effects
                        .entry(*effect)
                        .or_insert_with(|| {
                            holaf_core::model::EffectState::inactive_with_defaults(*effect)
                        });
                    entry.params.insert(param.clone(), *value);
                }
                if let Some(dialog) = &mut self.fx_dialog {
                    if &dialog.uid == uid && dialog.effect == *effect {
                        dialog.params.insert(param.clone(), *value);
                    }
                }
            }
            UiAction::OpenFxSettings { uid, effect } => {
                if let Some(index) = self.strip_index(uid) {
                    let params = self.strips[index]
                        .effects
                        .get(effect)
                        .map(|state| state.params.clone())
                        .unwrap_or_else(|| holaf_core::fx::default_params(*effect));
                    let label = self.strips[index].label.clone();
                    self.fx_dialog = Some(FxSettings {
                        uid: uid.clone(),
                        effect: *effect,
                        label,
                        params,
                    });
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
                    // `_refresh_device_ui_state`: the mode follows the device.
                    self.strips[index].device_name = device.clone();
                }
                // Structural: one debounced `apply_snapshot` (Qt restarts the
                // engine through `_schedule_engine_restart`).
                self.device_restart.request(now);
            }
            UiAction::MidiLearn { uid, learning } => {
                if *learning {
                    // The strip's MIDI button now opens the per-property menu
                    // (`Learn Volume` / `Learn Mute` / `Learn Mono`), which is
                    // what the Qt `_show_midi_menu` offered; this action is kept
                    // for the "arm volume" shortcut and for tests.
                    self.midi_learn(uid, MidiTarget::Volume);
                } else {
                    self.midi_learning = None;
                    self.status = Some("MIDI learn cancelled".to_string());
                }
            }
            UiAction::MidiLearnTarget { uid, target } => self.midi_learn(uid, *target),
            UiAction::MidiLearnCancel => {
                if self.midi_learning.take().is_some() {
                    self.status = Some("MIDI learn cancelled".to_string());
                }
            }
            UiAction::ClearMidiMappings { uid } => self.clear_midi_bindings(uid, now),
            UiAction::OpenMidiPortPicker => {
                // The list is the shell's (`state.midi_ports`, refreshed every
                // retry interval); opening asks for nothing and never blocks.
                self.midi_port_picker = Some(crate::dialogs::MidiPortPicker {
                    selected: self.midi_port_request.clone(),
                });
            }
            UiAction::MidiPortSelected { name } => {
                self.midi_port_picker = None;
                let name = name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string);
                if name == self.midi_port_request {
                    // Re-picking the same port is not a change: no write, no
                    // reconnect (the connection must not be dropped for nothing).
                    log::debug!("midi: port {name:?} already selected");
                    self.midi_port_auto = false;
                    self.finish_action(action, now);
                    return;
                }
                if let Some(config) = &mut self.config {
                    config.set_ui_rust_string("midi_port", name.as_deref());
                }
                self.midi_port_request = name.clone();
                // An explicit choice (including "No controller") ends the
                // automatic selection for good.
                self.midi_port_auto = false;
                // A new controller invalidates the "connected" flag until the
                // shell says otherwise; the next frame opens it.
                self.midi_connected = false;
                self.status = Some(match &name {
                    Some(name) => format!("MIDI: connecting to {name}"),
                    None => "MIDI: no controller".to_string(),
                });
                log::info!("midi: controller selected: {name:?}");
                self.request_save(now);
            }
            UiAction::RequestDelete { uid } => {
                if let Some(index) = self.strip_index(uid) {
                    self.delete_confirm = Some(DeleteConfirm {
                        uid: uid.clone(),
                        label: self.strips[index].label.clone(),
                    });
                }
            }
            UiAction::Delete { uid } => {
                self.delete_confirm = None;
                if let Some(index) = self.strip_index(uid) {
                    let removed = self.strips.remove(index);
                    self.levels.remove(&removed.uid);
                    self.volumes.forget(&removed.uid);
                    self.fx_pending
                        .retain(|(pending, _), _| pending != &removed.uid);
                    // Drop the routes pointing at the removed strip.
                    for strip in &mut self.strips {
                        strip.routes.retain(|r| r != &removed.uid);
                    }
                    // Its application assignment and MIDI mappings die with it
                    // (the config entries disappear too: `UiConfig::document`
                    // only writes the strips that still exist).
                    self.app_assignments.remove(&removed.uid);
                    self.midi_bindings.remove(&removed.uid);
                    self.midi_map = MidiMap::from_strips(&self.strips, &self.midi_bindings);
                    self.midi_last_volume
                        .retain(|(uid, _), _| uid != &removed.uid);
                    // A learn armed on the strip that just disappeared would
                    // otherwise store a mapping nobody can ever use.
                    if self.is_midi_learning(&removed.uid) {
                        self.midi_learning = None;
                    }
                    if self.midi_flash.as_ref().is_some_and(|flash| flash.uid == removed.uid) {
                        self.midi_flash = None;
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
            UiAction::BeginAddStrip { kind } => {
                self.add_dialog = Some(AddStripDialog {
                    kind: *kind,
                    label: String::new(),
                });
            }
            UiAction::AddStrip { kind, label } => {
                self.add_dialog = None;
                let label = label
                    .as_deref()
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| match kind {
                        StripType::Input => format!("Input {}", self.count_of(*kind) + 1),
                        StripType::Output => format!("Output {}", self.count_of(*kind) + 1),
                    });
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
                if let Some(index) = self.strip_index(uid) {
                    let label = self.strips[index].label.clone();
                    let assigned = self.app_assignments.get(uid).cloned().unwrap_or_default();
                    // The dialog opens immediately over the cached list — a
                    // question to the engine must never block a click — and the
                    // fresh answer fills it in when it arrives.
                    self.apps_error = None;
                    self.apps_dialog =
                        Some(AppSelection::new(uid, &label, &assigned, &self.app_list));
                    self.send_command(EngineCommand::AppList);
                }
            }
            UiAction::SelectAppsDone { uid, names } => {
                self.apps_dialog = None;
                if let Some(index) = self.strip_index(uid) {
                    let label = self.strips[index].label.clone();
                    let names: Vec<String> = names
                        .iter()
                        .map(|name| name.trim().to_string())
                        .filter(|name| !name.is_empty())
                        .collect();
                    if names.is_empty() {
                        self.app_assignments.remove(uid);
                    } else {
                        self.app_assignments.insert(uid.clone(), names.clone());
                    }
                    // `assigned_apps` is a UI-only key: it goes straight into
                    // the config store, which preserves everything else.
                    if let Some(config) = &mut self.config {
                        config.set_assigned_apps(uid, &names);
                    }
                    self.status = Some(if names.is_empty() {
                        format!("{label}: no application assigned")
                    } else {
                        format!("{label}: {}", names.join(", "))
                    });
                    // Route them now instead of waiting for the next tick.
                    self.send_command(EngineCommand::AppList);
                }
            }
            UiAction::RestartEngine => self.restart_engine(now),
            UiAction::OpenDiagnostics => self.open_diagnostics(),
            UiAction::OpenProjectPage => {
                match crate::menus::open_url(crate::menus::PROJECT_PAGE_URL) {
                    Ok(how) => {
                        log::info!("project page opened with {how}");
                        self.status = Some(format!("opened {}", crate::menus::PROJECT_PAGE_URL));
                    }
                    Err(err) => {
                        // A browser failure must never be silent (it is the one
                        // symptom the user cannot debug from the UI).
                        log::warn!("cannot open the project page: {err}");
                        self.status = Some(format!(
                            "cannot open {} - no browser ({err})",
                            crate::menus::PROJECT_PAGE_URL
                        ));
                    }
                }
            }
            UiAction::OpenContextMenu { kind, pos } => {
                log::debug!("context menu: {kind:?}");
                // The frame time is what keeps the opening click from closing
                // the menu in the same frame (see `menus::draw`).
                self.context_menu = Some(
                    crate::menus::ContextMenu::new(kind.clone(), *pos).opened_at(self.time as f64),
                );
            }
            UiAction::SetAlwaysOnTop { value } => {
                self.always_on_top = *value;
                if let Some(config) = &mut self.config {
                    config.set_ui_rust_bool("always_on_top", *value);
                }
                self.status = Some(format!(
                    "always on top {}",
                    if *value { "enabled" } else { "disabled" }
                ));
            }
            UiAction::OpenDevicePicker { uid } => {
                if let Some(index) = self.strip_index(uid) {
                    let kind = self.strips[index].kind;
                    self.devices_error = None;
                    self.device_picker = Some(DevicePicker {
                        uid: uid.clone(),
                        kind,
                        current: self.strips[index].device_name.clone(),
                        // The pop-up opens under the combo button and is
                        // dismissed by a click outside it: the opening click *is*
                        // outside, so its frame must be exempted (same trick as
                        // `menus::ContextMenu`).
                        opened_at: self.time as f64,
                    });
                    // Ask the engine for a fresh list; the pop-up shows the cached
                    // one immediately and fills up when the answer arrives.
                    self.send_command(EngineCommand::DeviceList);
                }
            }
        }

        self.finish_action(action, now);
    }

    /// The tail of [`Self::apply`] — engine dispatch, persistence, action log.
    ///
    /// A separate function because one arm (`MidiPortSelected`) stops early for an
    /// idempotent choice, and must still go through the bookkeeping: an action
    /// that never reaches `pending_actions` would silently disappear for whoever
    /// drains them (the tests, and the engine diagnostics).
    fn finish_action(&mut self, action: UiAction, now: Instant) {
        // Queue for the engine, then hand it to the bridge if there is one.
        // Structural changes (add / delete / rename) rebuild the graph plan, so
        // they send the whole strip list: `Backend::apply_snapshot`.
        for command in engine_commands(&action, &self.strips) {
            self.dispatch(command, now);
        }

        if is_persistent(&action) {
            self.request_save(now);
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

    // -----------------------------------------------------------------
    // Undo / rollback
    // -----------------------------------------------------------------

    fn undo_for(&self, action: &UiAction) -> Option<Undo> {
        Some(match action {
            UiAction::Volume { uid, .. } => Undo::Volume {
                uid: uid.clone(),
                previous: self.strips.get(self.strip_index(uid)?)?.volume,
            },
            UiAction::Mute { uid, .. } => Undo::Mute {
                uid: uid.clone(),
                previous: self.strips.get(self.strip_index(uid)?)?.mute,
            },
            UiAction::Mono { uid, .. } => Undo::Mono {
                uid: uid.clone(),
                previous: self.strips.get(self.strip_index(uid)?)?.is_mono,
            },
            UiAction::ToggleFx { uid, effect, .. } => Undo::FxActive {
                uid: uid.clone(),
                effect: *effect,
                previous: self
                    .strips
                    .get(self.strip_index(uid)?)?
                    .effects
                    .get(effect)
                    .map(|state| state.active)
                    .unwrap_or(false),
            },
            UiAction::FxParam { uid, effect, .. } => Undo::FxParams {
                uid: uid.clone(),
                effect: *effect,
                previous: self
                    .strips
                    .get(self.strip_index(uid)?)?
                    .effects
                    .get(effect)
                    .map(|state| state.params.clone())
                    .unwrap_or_default(),
            },
            UiAction::Route { uid, .. } => Undo::Route {
                uid: uid.clone(),
                previous: self.strips.get(self.strip_index(uid)?)?.routes.clone(),
            },
            UiAction::DefaultSink { .. } => Undo::DefaultSink {
                previous: self
                    .strips
                    .iter()
                    .find(|strip| strip.is_default)
                    .map(|strip| strip.uid.clone()),
            },
            UiAction::DeviceChanged { uid, .. } => Undo::Device {
                uid: uid.clone(),
                previous: self.strips.get(self.strip_index(uid)?)?.device_name.clone(),
            },
            _ => return None,
        })
    }

    fn push_undo(&mut self, undo: Undo) {
        if self.undo.len() >= UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.undo.push(undo);
    }

    /// Revert the model change a failed command was carrying, and tell the user.
    fn rollback(&mut self, command: &EngineCommand, reason: &str) {
        let Some(position) = self.undo.iter().rposition(|undo| undo.matches(command)) else {
            return;
        };
        match self.undo.remove(position) {
            Undo::Volume {
                uid,
                previous: value,
            } => {
                if let Some(index) = self.strip_index(&uid) {
                    self.strips[index].volume = value;
                }
            }
            Undo::Mute { uid, previous } => {
                if let Some(index) = self.strip_index(&uid) {
                    self.strips[index].mute = previous;
                }
            }
            Undo::Mono { uid, previous } => {
                if let Some(index) = self.strip_index(&uid) {
                    self.strips[index].is_mono = previous;
                }
            }
            Undo::FxActive {
                uid,
                effect,
                previous,
            } => {
                if let Some(index) = self.strip_index(&uid) {
                    if let Some(state) = self.strips[index].effects.get_mut(&effect) {
                        state.active = previous;
                    }
                }
            }
            Undo::FxParams {
                uid,
                effect,
                previous,
            } => {
                if let Some(index) = self.strip_index(&uid) {
                    if let Some(state) = self.strips[index].effects.get_mut(&effect) {
                        state.params = previous;
                    }
                }
                let restored = self
                    .strip_index(&uid)
                    .and_then(|index| self.strips[index].effects.get(&effect))
                    .map(|state| state.params.clone())
                    .unwrap_or_default();
                if let Some(dialog) = &mut self.fx_dialog {
                    if dialog.uid == uid && dialog.effect == effect {
                        dialog.params = restored;
                    }
                }
            }
            Undo::Route { uid, previous } => {
                if let Some(index) = self.strip_index(&uid) {
                    self.strips[index].routes = previous;
                }
            }
            Undo::DefaultSink { previous } => {
                for strip in &mut self.strips {
                    strip.is_default = previous.as_deref() == Some(strip.uid.as_str());
                }
            }
            Undo::Device { uid, previous } => {
                if let Some(index) = self.strip_index(&uid) {
                    self.strips[index].device_name = previous;
                }
            }
        }
        self.volumes.clear();
        self.status = Some(format!("engine error: {reason} - change reverted"));
    }

    // -----------------------------------------------------------------
    // Engine plumbing
    // -----------------------------------------------------------------

    /// Queue a command, applying the throttles/debounces that belong to it.
    fn dispatch(&mut self, command: EngineCommand, now: Instant) {
        match &command {
            EngineCommand::SetVolume { uid, volume } => {
                // At most one `set_volume` per strip per 40 ms, always the last
                // value: a drag cannot flood the engine thread.
                if let Some(value) = self.volumes.record(now, uid, *volume) {
                    self.send_command(EngineCommand::SetVolume {
                        uid: uid.clone(),
                        volume: value,
                    });
                }
            }
            EngineCommand::SetFxParams {
                uid,
                effect,
                params,
            } => {
                // Live sliders: coalesce into one push per 200 ms.
                self.fx_pending
                    .insert((uid.clone(), *effect), params.clone());
                self.fx_debounce.request(now);
            }
            _ => self.send_command(command),
        }
    }

    fn send_command(&mut self, command: EngineCommand) {
        if self.engine.is_disconnected() {
            let reason = self
                .engine
                .reason()
                .unwrap_or("engine unavailable")
                .to_string();
            log::warn!("engine disconnected ({reason}): {command:?} ignored");
            self.status = Some(format!("engine disconnected: {reason}"));
            return;
        }
        let Some(bridge) = &self.bridge else {
            log::debug!("engine: no backend attached, dropping {command:?}");
            return;
        };
        if let Err(err) = bridge.send(command) {
            log::warn!("engine: {err}");
            self.engine = EngineState::Disconnected(err.to_string());
            self.status = Some(format!("engine: {err}"));
        }
    }

    /// Send everything the throttles/debounces owe, if their window elapsed.
    pub fn flush_engine(&mut self, now: Instant) {
        for (uid, volume) in self.volumes.take_due(now) {
            self.send_command(EngineCommand::SetVolume { uid, volume });
        }
        if self.device_restart.take_due(now) {
            // One graph rebuild for the whole burst of device changes.
            self.send_command(EngineCommand::SyncSnapshot(self.strips.clone()));
        }
        if self.fx_debounce.take_due(now) {
            let pending: Vec<((String, EffectKey), IndexMap<String, f32>)> =
                self.fx_pending.drain().collect();
            for ((uid, effect), params) in pending {
                self.send_command(EngineCommand::SetFxParams {
                    uid,
                    effect,
                    params,
                });
            }
        }
    }

    /// Drain the bridge's events into the state: levels, status, rollbacks.
    pub fn poll_engine(&mut self) {
        let now = Instant::now();
        self.poll_engine_at(now);
    }

    /// [`Self::poll_engine`] with an explicit clock (tests).
    pub fn poll_engine_at(&mut self, now: Instant) {
        let Some(bridge) = &self.bridge else {
            self.decay_meters(now, None);
            return;
        };
        let events = bridge.drain_events();
        let board: Option<MeterBoard> = bridge.meter_board().cloned();
        let dt = now.saturating_duration_since(self.last_meter_tick);

        let mut event_levels: HashMap<String, [f32; 2]> = HashMap::new();
        for event in events {
            match event {
                EngineEvent::Ready => {
                    self.engine = EngineState::Connected;
                    self.status = Some("engine ready".to_string());
                }
                EngineEvent::Levels(levels) => event_levels.extend(levels),
                EngineEvent::Applied(what) => {
                    log::trace!("engine applied {what}");
                    if self.engine != EngineState::Connected {
                        self.engine = EngineState::Connected;
                    }
                }
                EngineEvent::Devices(devices) => {
                    log::info!("engine: {} audio device(s)", devices.len());
                    self.engine_devices = devices;
                    self.devices_error = None;
                    self.reconcile_devices();
                }
                EngineEvent::Apps(list) => {
                    log::debug!(
                        "engine: {} application(s){}",
                        list.apps.len(),
                        if list.stub { " (stub engine)" } else { "" }
                    );
                    self.app_list = list;
                    self.apps_error = None;
                    // A fresh answer is exactly when enforcement can run: move
                    // whatever is not on its strip's bus yet.
                    self.enforce_applications();
                }
                EngineEvent::Status(value) => {
                    self.node_names = status_node_names(&value);
                    self.backend_status = Some(value);
                    self.status_error = None;
                    // The dialog is open: rebuild its report from the new
                    // payload (that is what "Refresh" does, asynchronously).
                    if self.diagnostics.is_some() {
                        self.refresh_diagnostics();
                    }
                }
                EngineEvent::Restarted(outcome) => {
                    self.restarting = false;
                    match outcome {
                        Ok(message) => {
                            log::info!("engine: {message}");
                            self.engine = EngineState::Connected;
                            // The graph was rebuilt from the last snapshot: the
                            // meter sources are new, so are the assigned apps.
                            self.status = Some(message);
                            self.send_command(EngineCommand::AppList);
                        }
                        Err(reason) => {
                            log::warn!("engine restart failed: {reason}");
                            self.engine = EngineState::Disconnected(reason.clone());
                            self.status = Some(format!("audio engine restart failed: {reason}"));
                        }
                    }
                }
                EngineEvent::Error(reason) => {
                    // A connection-level failure (a refused command arrives as
                    // `Failed`): the UI stays up in "disconnected mode".
                    log::warn!("engine: {reason}");
                    self.engine = EngineState::Disconnected(reason.clone());
                    self.status = Some(format!("engine error: {reason}"));
                    self.devices_error = Some(reason.clone());
                    // The app list and the status payload are equally
                    // unavailable: the dialogs say why instead of showing an
                    // empty list (which reads as "nothing is playing").
                    self.apps_error = Some(reason.clone());
                    self.status_error = Some(reason);
                }
                EngineEvent::Failed { command, reason } => {
                    log::warn!("engine refused {command:?}: {reason}");
                    self.rollback(&command, &reason);
                }
            }
        }

        match board {
            Some(board) => self.feed_from_board(&board, now, dt),
            None => {
                // Mock/event engine: apply the envelope to the reported levels.
                self.decay_meters(now, Some(&event_levels));
            }
        }
    }

    fn feed_from_board(&mut self, board: &MeterBoard, now: Instant, dt: Duration) {
        self.last_meter_tick = now;
        for index in 0..self.strips.len() {
            let uid = self.strips[index].uid.clone();
            match board.read(&uid) {
                Some(levels) => {
                    self.meter_missing.remove(&uid);
                    // The envelope takes `dt` into account, so a 60 fps repaint
                    // decays exactly like a 25 Hz one.
                    self.feed_levels(&uid, levels, dt);
                }
                None => {
                    if self.meter_missing.insert(uid.clone()) {
                        log::warn!("metering: no source for {uid} (VU stays dark)");
                    }
                    self.feed_levels(&uid, [0.0, 0.0], dt);
                }
            }
        }
    }

    /// Apply the decay envelope to a level map (mock engine / disconnect).
    fn decay_meters(&mut self, now: Instant, levels: Option<&HashMap<String, [f32; 2]>>) {
        let dt = now.saturating_duration_since(self.last_meter_tick);
        self.last_meter_tick = now;
        let uids: Vec<String> = self.strips.iter().map(|s| s.uid.clone()).collect();
        for uid in uids {
            let raw = levels
                .and_then(|map| map.get(&uid).copied())
                .unwrap_or([0.0, 0.0]);
            self.feed_levels(&uid, raw, dt);
        }
    }

    // -----------------------------------------------------------------
    // Persistence
    // -----------------------------------------------------------------

    /// Schedule a debounced config write (no-op when nothing can be persisted
    /// yet: a first run with no strips must not create a config file).
    pub fn request_save(&mut self, now: Instant) {
        let Some(config) = &mut self.config else {
            return;
        };
        if config.loaded() || !self.strips.is_empty() {
            config.request_save(now);
        }
    }

    /// Write the config when its debounce elapsed.
    pub fn save_pending(&mut self, now: Instant) {
        let geometry = self.geometry();
        let strips = self.strips.clone();
        let Some(config) = &mut self.config else {
            return;
        };
        if let Some(result) = config.save_if_due(now, &strips, geometry) {
            match result {
                Ok(()) => log::debug!("config written to {}", config.path().display()),
                Err(err) => {
                    log::warn!("cannot write {}: {err}", config.path().display());
                    self.status = Some(format!("config save failed: {err}"));
                }
            }
        }
    }

    /// Write immediately (explicit save, clean shutdown).
    pub fn save_now(&mut self) {
        let geometry = self.geometry();
        let strips = self.strips.clone();
        let Some(config) = &mut self.config else {
            return;
        };
        if !config.save_pending() {
            return;
        }
        match config.save_now(&strips, geometry) {
            Ok(()) => log::debug!("config written to {}", config.path().display()),
            Err(err) => {
                log::warn!("cannot write {}: {err}", config.path().display());
                self.status = Some(format!("config save failed: {err}"));
            }
        }
    }

    /// The geometry to persist (width informational, height authoritative).
    ///
    /// The width is only included when the loaded document already had a
    /// `window.width`: the width is locked to the strip count, so writing it into
    /// a config that never had one would add a key for no reason (F5).
    pub fn geometry(&self) -> Option<WindowGeometry> {
        let (width, height) = self.window?;
        let keep_width = self.config.as_ref().is_some_and(UiConfig::has_stored_width);
        Some(WindowGeometry {
            width: keep_width.then_some(width),
            height,
        })
    }

    /// Record the window size seen on this frame.
    ///
    /// **A save is only scheduled for a resize the user performed**, never for the
    /// size the window happens to have at startup:
    ///
    /// * `self.window` (what gets persisted) keeps the value loaded from
    ///   `config.json` and is only replaced by an armed, observed change.
    /// * The startup observation is therefore dropped on the floor — a window
    ///   manager that hands us 575 px of height while the file says 423 is not a
    ///   user decision (and with `MinInnerSize` the constraint may force it), so
    ///   writing it back would rewrite the user's file on every launch (F4).
    /// * The comparison is *observed vs observed*, not observed vs persisted: a
    ///   constant startup size can never schedule a write, however far it is from
    ///   the stored one.
    /// * The first [`RESIZE_ARM_FRAMES`] frames are ignored so an eframe/WM
    ///   settle (the window is created, then resized once by the WM) is not
    ///   mistaken for a gesture either.
    pub fn note_window_size(&mut self, width: f32, height: f32, now: Instant) {
        if !width.is_finite() || !height.is_finite() || height < 1.0 {
            return;
        }
        let width = width.max(1.0);
        self.window_frames = self.window_frames.saturating_add(1);
        let previous = self.window_observed.replace((width, height));
        // First run (no config): adopt what the WM gave us, nothing to defend.
        if self.window.is_none() {
            self.window = Some((width, height));
        }
        if self.window_frames <= RESIZE_ARM_FRAMES {
            return;
        }
        let Some((_, previous_height)) = previous else {
            return;
        };
        if (previous_height - height).abs() > RESIZE_JITTER {
            self.window = Some((width, height));
            self.request_save(now);
        }
    }

    /// How the strip's device combo must label its binding.
    ///
    /// `Unknown` when the engine never listed devices (snapshot, disconnected
    /// mode): a strip must not be labelled broken just because nobody answered.
    pub fn device_presence(&self, uid: &str) -> DevicePresence {
        match self.strip_index(uid) {
            Some(index) => devices::presence(&self.strips[index], &self.engine_devices),
            None => DevicePresence::Unknown,
        }
    }

    /// Re-bind the strips whose device survived under a new `node.name`.
    ///
    /// Called whenever the engine reports its device list.  A repair rewrites
    /// `device_name` (the user's config is then saved, like any other edit) and
    /// schedules the debounced `apply_snapshot`, because the device a strip is
    /// bound to *is* part of the graph plan.  A strip whose device is gone keeps
    /// its `device_name` (never silently unbound: the device may come back) and
    /// is listed in [`Self::missing_devices`] so the view shows `(Not Found)`.
    pub fn reconcile_devices(&mut self) {
        let now = self.now;
        let (repairs, missing) = devices::reconcile(&self.strips, &self.engine_devices);
        self.missing_devices = missing.iter().map(|(uid, _, _)| uid.clone()).collect();
        if repairs.is_empty() {
            for (uid, label, declared) in &missing {
                log::debug!("device of {label:?} ({uid}) is gone: {declared}");
            }
            return;
        }
        for repair in &repairs {
            // An approximate match is a *guess*: it is logged loudly, because it
            // rewrites the user's configuration.
            log::warn!(
                "device {:?} of {:?} not found - re-bound to {:?} ({:?})",
                repair.declared,
                repair.label,
                repair.resolved,
                repair.kind
            );
            if let Some(index) = self.strip_index(&repair.uid) {
                self.strips[index].device_name = Some(repair.resolved.clone());
            }
        }
        let summary = repairs
            .iter()
            .map(|repair| format!("{} → {}", repair.label, repair.resolved))
            .collect::<Vec<_>>()
            .join(", ");
        self.status = Some(format!("device re-bound: {summary}"));
        self.device_restart.request(now);
        self.request_save(now);
    }

    /// The device list a strip may bind to, in picker order.
    pub fn devices_for(&self, kind: StripType) -> Vec<DeviceInfo> {
        self.engine_devices
            .iter()
            .filter(|device| match kind {
                StripType::Input => device.is_source(),
                StripType::Output => device.is_sink(),
            })
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------
    // Applications (SELECT APPS + 5 s enforcement)
    // -----------------------------------------------------------------

    /// The assigned application names of one strip.
    pub fn assigned_apps(&self, uid: &str) -> &[String] {
        self.app_assignments
            .get(uid)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Does any strip take applications over?  (Nothing else needs the 5 s
    /// `app_list` polling.)
    pub fn any_strip_enforces_apps(&self) -> bool {
        self.strips
            .iter()
            .any(|strip| apps::enforces(strip, self.assigned_apps(&strip.uid)))
    }

    /// Ask the engine to move every assigned application that is not already on
    /// its strip's bus (the Qt `_move_apps_to_strip`).
    ///
    /// Called with the answer to every `AppList` — i.e. every
    /// [`apps::ENFORCE_INTERVAL`] and right after the dialog is confirmed — so
    /// the plan is always computed from a list that was just read.  Each move is
    /// one [`EngineCommand::AppMove`]; a refused move is reported by the bridge
    /// (`Failed`) and shows up in the status line, never as a silent no-op.
    pub fn enforce_applications(&mut self) {
        if self.app_list.stub {
            log::debug!("apps: the engine cannot move applications (stub payload)");
            return;
        }
        let moves = apps::plan(
            &self.strips,
            &self.app_assignments,
            &self.node_names,
            &self.app_list,
        );
        for movement in moves {
            log::info!(
                "apps: moving {:?} to {} (strip {})",
                movement.app_name,
                movement.sink,
                movement.uid
            );
            self.send_command(EngineCommand::AppMove {
                app_id: movement.app_id,
                sink: movement.sink,
            });
        }
    }

    /// The 5 s enforcement timer: ask for the application list when one is due.
    ///
    /// Idempotent and cheap when nothing is assigned: no strip enforces ⇒ the
    /// engine is never polled.
    pub fn tick_app_enforcement(&mut self, now: Instant) {
        if !self.any_strip_enforces_apps() {
            self.enforce.cancel();
            return;
        }
        // The first tick after an assignment asks *now* (the Qt timer starts
        // immediately), then one poll per interval.
        if !self.enforce.is_pending() || self.enforce.take_due(now) {
            self.send_command(EngineCommand::AppList);
            self.enforce.request(now);
        }
    }

    // -----------------------------------------------------------------
    // Full engine restart (context menu)
    // -----------------------------------------------------------------

    /// "Restart audio engine": stop the graph, reconnect, re-apply the snapshot.
    ///
    /// The work happens on the engine thread (it blocks for as long as PipeWire
    /// needs), and it is **not** re-entrant: a second click while one restart
    /// runs is refused with a status line, exactly like Qt's
    /// `_full_restart_running`.  Two concurrent restarts would race on the same
    /// audio graph.
    pub fn restart_engine(&mut self, now: Instant) {
        let _ = now;
        if self.restarting {
            self.status = Some("an audio engine restart is already running".to_string());
            return;
        }
        match &self.bridge {
            None => {
                self.status = Some("no audio engine attached - nothing to restart".to_string());
            }
            Some(_) => {
                // A restart is the way *out* of disconnected mode, so it goes
                // around `send_command` (which refuses while disconnected).
                let command = EngineCommand::RestartEngine;
                let Some(bridge) = &self.bridge else { return };
                match bridge.send(command) {
                    Ok(()) => {
                        self.restarting = true;
                        self.engine = EngineState::Connecting;
                        self.status = Some("restarting the audio engine…".to_string());
                        log::info!("engine: restart requested");
                    }
                    Err(err) => {
                        log::warn!("engine: {err}");
                        self.engine = EngineState::Disconnected(err.to_string());
                        self.status = Some(format!("engine: {err}"));
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Diagnostics window
    // -----------------------------------------------------------------

    /// "Debug window": build the report from what the UI already knows, open the
    /// dialog, and ask the engine for a fresh `status` (the answer refreshes the
    /// report a frame later — the dialog never blocks on the daemon).
    pub fn open_diagnostics(&mut self) {
        self.refresh_diagnostics();
        if let Some(state) = &mut self.diagnostics {
            state.tab = crate::diagnostics::DiagTab::Summary;
        }
        self.send_command(EngineCommand::Status);
    }

    /// Rebuild the open report in place (the "Refresh" button).
    pub fn refresh_diagnostics(&mut self) {
        let report = self.collect_report();
        match &mut self.diagnostics {
            Some(diagnostics) => diagnostics.report = report,
            None => self.diagnostics = Some(DiagnosticsState::new(report)),
        }
        self.send_command(EngineCommand::Status);
    }

    /// Close the diagnostics window.
    pub fn close_diagnostics(&mut self) {
        self.diagnostics = None;
    }

    /// `(uid, label, declared device)` of every strip whose device is gone —
    /// the diagnostics input (and the same list the view paints `(Not Found)`
    /// from).
    pub fn missing_device_list(&self) -> Vec<(String, String, String)> {
        let mut uids: Vec<&String> = self.missing_devices.iter().collect();
        uids.sort();
        uids.into_iter()
            .filter_map(|uid| {
                let strip = self.strips.get(self.strip_index(uid)?)?;
                Some((
                    strip.uid.clone(),
                    strip.label.clone(),
                    strip.device_name.clone().unwrap_or_default(),
                ))
            })
            .collect()
    }

    /// Build a diagnostic report from the current state.
    pub fn collect_report(&self) -> Report {
        let missing_devices = self.missing_device_list();
        let mut meter_missing: Vec<String> = self.meter_missing.iter().cloned().collect();
        meter_missing.sort();
        let sources: HashMap<String, String> = HashMap::new();
        let config_path = self
            .config
            .as_ref()
            .map(|config| config.path().to_path_buf());
        let status = self.backend_status.clone();
        Report::collect(&Inputs {
            status: status.as_ref(),
            status_error: self.status_error.as_deref(),
            devices: &self.engine_devices,
            meter_sources: &sources,
            strips: &self.strips,
            missing_devices: &missing_devices,
            meter_missing: &meter_missing,
            config_path: config_path.as_deref(),
            now_unix: crate::diagnostics::now_unix(),
            daemon_pids: None,
        })
    }

    // -----------------------------------------------------------------
    // MIDI
    // -----------------------------------------------------------------

    /// Handle one incoming MIDI message (`on_midi_message_received`).
    ///
    /// The policy lives in [`crate::midi::decode`]; this method only applies the
    /// outcome: a learned mapping is stored in `config.json` and in the lookup
    /// table, an action goes through [`Self::apply`] (so it is persisted, sent to
    /// the engine and rolled back on failure like any mouse action), and a
    /// mute/mono toggle queues the LED feedback.
    pub fn handle_midi_message(&mut self, message: MidiMessage, now: Instant) {
        let learning = self
            .midi_learning
            .as_ref()
            .map(|(uid, target)| (uid.as_str(), *target));
        let outcome = crate::midi::decode(
            &self.midi_map,
            message,
            learning,
            &self.strips,
            &mut self.midi_last_volume,
            now,
        );
        match outcome {
            MidiOutcome::Action(action) => {
                let target = match &action {
                    UiAction::Volume { .. } => MidiTarget::Volume,
                    UiAction::Mute { .. } => MidiTarget::Mute,
                    UiAction::Mono { .. } => MidiTarget::Mono,
                    _ => return,
                };
                log::debug!("midi: {action:?} ({})", target.as_str());
                // `apply` queues the LED feedback for a mute/mono toggle, so the
                // controller's lamp follows the strip whatever moved it.
                self.apply(action);
            }
            MidiOutcome::Learned {
                uid,
                target,
                binding,
            } => {
                self.midi_learning = None;
                if let Some(config) = &mut self.config {
                    config.set_midi_binding(&uid, target, Some(binding));
                }
                let entry = self.midi_bindings.entry(uid.clone()).or_default();
                entry.retain(|(bound, _)| *bound != target);
                entry.push((target, binding));
                entry.sort_by_key(|(target, _)| *target as u8);
                self.midi_map = MidiMap::from_strips(&self.strips, &self.midi_bindings);
                // Mirror the current state on the button right away.
                if let Some(strip) = self
                    .strip_index(&uid)
                    .and_then(|index| self.strips.get(index))
                {
                    let active = match target {
                        MidiTarget::Mute => strip.mute,
                        MidiTarget::Mono => strip.is_mono,
                        MidiTarget::Volume => false,
                    };
                    if target != MidiTarget::Volume {
                        self.midi_feedback
                            .push(crate::midi::feedback(binding, active));
                    }
                }
                log::info!(
                    "midi: learned {} for {uid} on channel {} id {}",
                    target.as_str(),
                    binding.channel,
                    binding.identifier().unwrap_or(0)
                );
                self.status = Some(format!(
                    "MIDI: {} = {} ({} bound)",
                    target.as_str(),
                    binding.describe(),
                    strip_label(self, &uid)
                ));
                // Confirmation on the strip's MIDI button (`CC 19`) — the Qt UI
                // silently reverted to `MIDI`, which left the user unsure.
                self.midi_flash = Some(MidiFlash {
                    uid,
                    text: binding.short_label(),
                    until: now + MIDI_FLASH_INTERVAL,
                });
                self.request_save(now);
            }
            MidiOutcome::Ignored => {}
        }
    }

    /// Adopt a controller found by the shell's automatic selection.
    ///
    /// Persisted at once (`ui_rust.midi_port`) so the next start connects to the
    /// same port even if the user never touches the menu — which is what "it
    /// picked up my MIDI Mix by itself" has to mean.
    pub fn adopt_midi_port(&mut self, name: &str, now: Instant) {
        self.midi_port_request = Some(name.to_string());
        self.midi_port_auto = false;
        if let Some(config) = &mut self.config {
            config.set_ui_rust_string("midi_port", Some(name));
        }
        self.status = Some(format!("MIDI: {name}"));
        self.request_save(now);
    }

    /// Take the LED messages that must be written to the controller.
    pub fn take_midi_feedback(&mut self) -> Vec<MidiMessage> {
        std::mem::take(&mut self.midi_feedback)
    }

    /// Re-send the button states of every strip (Qt `_restore_button_states`,
    /// called when the port opens so the controller's lamps match the mixer).
    pub fn restore_midi_button_states(&mut self) {
        let messages = crate::midi::button_feedback(&self.strips, &self.midi_bindings);
        self.midi_feedback.extend(messages);
    }

    /// Queue the LED message mirroring `active` on the strip's `target` button
    /// (a no-op when that property has no binding, or for a volume knob).
    pub fn queue_midi_led(&mut self, uid: &str, target: MidiTarget, active: bool) {
        if target == MidiTarget::Volume {
            return;
        }
        let Some(bindings) = self.midi_bindings.get(uid) else {
            return;
        };
        if let Some((_, binding)) = bindings.iter().find(|(bound, _)| *bound == target) {
            self.midi_feedback
                .push(crate::midi::feedback(*binding, active));
        }
    }

    /// The MIDI bindings of one strip (`volume` / `mute` / `mono`), empty when it
    /// has none.  What the MIDI button's tooltip lists.
    pub fn midi_bindings_for(&self, uid: &str) -> &[(MidiTarget, Binding)] {
        self.midi_bindings
            .get(uid)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Is a learn armed on this strip (the MIDI button then reads `LEARNING...`)?
    pub fn is_midi_learning(&self, uid: &str) -> bool {
        self.midi_learning
            .as_ref()
            .is_some_and(|(learning, _)| learning == uid)
    }

    /// Arm MIDI learn for a specific property (`Learn Volume` / `Learn Mute` /
    /// `Learn Mono` in the strip's MIDI menu — the Qt menu's three entries).
    ///
    /// The **next** bindable message (a CC or a note) defines the mapping and
    /// ends the learn, exactly like `MidiEngine._process_learn`: one property per
    /// gesture, no "still learning" mode the user can forget about.
    pub fn midi_learn(&mut self, uid: &str, target: MidiTarget) {
        if self.strip_index(uid).is_none() {
            return;
        }
        let label = strip_label(self, uid);
        log::info!("midi: learn armed on {label:?} for {}", target.as_str());
        self.midi_learning = Some((uid.to_string(), target));
        self.status = Some(if self.midi_connected {
            format!("MIDI: play the control for {label} {}", target.as_str())
        } else {
            format!(
                "MIDI: learning {} on {label} - no controller connected, pick one in the MIDI port menu",
                target.as_str()
            )
        });
    }

    /// Clear one strip's MIDI mappings (Qt `_clear_midi`: all three at once).
    pub fn clear_midi_bindings(&mut self, uid: &str, now: Instant) {
        if let Some(config) = &mut self.config {
            for target in MidiTarget::ALL {
                config.set_midi_binding(uid, target, None);
            }
        }
        let had = self.midi_bindings.remove(uid).is_some();
        self.midi_map = MidiMap::from_strips(&self.strips, &self.midi_bindings);
        // A learn armed on that strip has nothing left to fill in.
        if self.is_midi_learning(uid) {
            self.midi_learning = None;
        }
        let label = strip_label(self, uid);
        self.status = Some(if had {
            String::from("MIDI: mappings cleared on ") + &label
        } else {
            String::from("MIDI: no mapping to clear on ") + &label
        });
        self.midi_flash = None;
        self.request_save(now);
    }

    // -----------------------------------------------------------------
    // Frame tick
    // -----------------------------------------------------------------

    /// Everything that must happen once per frame besides painting: poll the
    /// engine, flush the throttles, write the config if due.
    pub fn tick(&mut self, now: Instant) {
        self.now = now;
        self.poll_engine_at(now);
        self.flush_engine(now);
        self.tick_app_enforcement(now);
        self.tick_midi_flash(now);
        self.save_pending(now);
    }

    /// Expire the learn confirmation painted on a strip's MIDI button.
    fn tick_midi_flash(&mut self, now: Instant) {
        if let Some(flash) = &self.midi_flash {
            if now >= flash.until {
                self.midi_flash = None;
            }
        }
    }

    /// What the strip's MIDI button explains on hover: every mapping it has and
    /// what a click does.  The Qt UI showed nothing (a mapping was invisible once
    /// learned, short of reading `config.json`), which is the other half of
    /// "voit les mappings".
    pub fn midi_summary(&self, uid: &str) -> String {
        let mut lines: Vec<String> = self
            .midi_bindings_for(uid)
            .iter()
            .map(|(target, binding)| format!("{} -> {}", binding.describe(), target.as_str()))
            .collect();
        if lines.is_empty() {
            lines.push("no MIDI mapping".to_string());
        }
        lines.push(String::new());
        lines.push("click: learn / clear the mappings".to_string());
        match (&self.midi_port_name, self.midi_connected) {
            (Some(name), true) => lines.push(format!("controller: {name}")),
            _ => lines.push("no controller connected (window menu > MIDI port...)".to_string()),
        }
        lines.join("\n")
    }

    /// What the strip's MIDI button should show, if not the plain `MIDI`:
    /// `LEARNING...` while a learn is armed, or the CC just learned.
    pub fn midi_button_text(&self, uid: &str) -> Option<String> {
        if self.is_midi_learning(uid) {
            return Some("LEARNING...".to_string());
        }
        self.midi_flash
            .as_ref()
            .filter(|flash| flash.uid == uid)
            .map(|flash| flash.text.clone())
    }

    /// Stop the engine and flush what must not be lost.
    ///
    /// Mirrors `make_shutdown_all` on the Python side: the worker gets one
    /// `Shutdown`, is joined, and no PipeWire object/child survives (the
    /// bridge's own `Drop` is the second net).
    pub fn shutdown(&mut self) {
        self.save_now();
        self.volumes.clear();
        self.fx_pending.clear();
        self.fx_debounce.cancel();
        self.device_restart.cancel();
        self.enforce.cancel();
        self.restarting = false;
        if let Some(bridge) = self.bridge.take() {
            log::info!("engine: shutting down");
            bridge.shutdown();
        }
        self.engine = EngineState::None;
    }

    /// Does the UI run without a live engine (banner + ignored audio actions)?
    pub fn is_disconnected(&self) -> bool {
        self.engine.is_disconnected()
    }

    /// The reason shown in the banner, if any.
    pub fn engine_error(&self) -> Option<String> {
        match &self.engine {
            EngineState::Disconnected(reason) => Some(reason.clone()),
            _ => None,
        }
    }
}

/// `uid → node_name` from a `status()` payload (the sink an assigned
/// application must be moved to).
pub fn status_node_names(status: &serde_json::Value) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Some(strips) = status.get("strips").and_then(serde_json::Value::as_array) else {
        return names;
    };
    for entry in strips {
        let (Some(uid), Some(node_name)) = (
            entry.get("uid").and_then(serde_json::Value::as_str),
            entry.get("node_name").and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        if !uid.is_empty() && !node_name.is_empty() {
            names.insert(uid.to_string(), node_name.to_string());
        }
    }
    names
}

/// Resolve the theme of a loaded config.
///
/// Order: the config's `"theme"` key (a path, or a built-in name), then
/// `$HOLAF_THEME`, then `theme.json` next to the config.  Anything unreadable
/// falls back to the built-in `holaf-dark` with a warning — a theme is
/// decoration, never a reason to refuse to start.
pub fn load_theme(config_theme: Option<&str>, config_path: &Path) -> ThemeFile {
    let candidate: Option<std::path::PathBuf> = config_theme
        .and_then(|name| {
            let as_path = Path::new(name);
            if as_path.is_file() {
                Some(as_path.to_path_buf())
            } else {
                let sibling = config_path.with_file_name(name);
                sibling.is_file().then_some(sibling)
            }
        })
        .or_else(|| std::env::var_os("HOLAF_THEME").map(std::path::PathBuf::from))
        .or_else(|| {
            let sibling = config_path.with_file_name("theme.json");
            sibling.is_file().then_some(sibling)
        });

    match candidate {
        Some(path) => match ThemeFile::load(&path) {
            Ok(theme) => {
                log::info!("theme: {} ({} path)", theme.name, path.display());
                theme
            }
            Err(err) => {
                log::warn!(
                    "cannot load theme {}: {err} - using holaf-dark",
                    path.display()
                );
                ThemeFile::holaf_dark()
            }
        },
        None => {
            if let Some(name) = config_theme {
                log::warn!("unknown theme {name:?} in config - using holaf-dark");
            }
            ThemeFile::holaf_dark()
        }
    }
}

/// Map a UI action onto the engine commands it implies (`vec![]` for pure-UI
/// actions like opening a dialog).
///
/// The list is computed **after** the model was mutated, because structural
/// actions carry the whole strip list (`apply_snapshot`).
pub fn engine_commands(action: &UiAction, strips: &[Strip]) -> Vec<EngineCommand> {
    match action {
        UiAction::Volume { uid, volume } => vec![EngineCommand::SetVolume {
            uid: uid.clone(),
            volume: *volume,
        }],
        UiAction::Mute { uid, mute } => vec![EngineCommand::SetMute {
            uid: uid.clone(),
            mute: *mute,
        }],
        UiAction::Mono { uid, mono } => vec![EngineCommand::SetMono {
            uid: uid.clone(),
            mono: *mono,
        }],
        UiAction::ToggleFx {
            uid,
            effect,
            active,
        } => vec![EngineCommand::SetEffect {
            uid: uid.clone(),
            effect: *effect,
            active: *active,
        }],
        UiAction::FxParam {
            uid,
            effect,
            param: _,
            value: _,
        } => strips
            .iter()
            .find(|strip| &strip.uid == uid)
            .and_then(|strip| strip.effects.get(effect).map(|state| state.params.clone()))
            .map(|params| {
                vec![EngineCommand::SetFxParams {
                    uid: uid.clone(),
                    effect: *effect,
                    params,
                }]
            })
            .unwrap_or_default(),
        UiAction::Route {
            uid,
            target,
            active,
        } => vec![EngineCommand::SetRoute {
            source: uid.clone(),
            target: target.clone(),
            active: *active,
        }],
        // Unchecking has no engine counterpart: the daemon only accepts "make
        // this the default".
        UiAction::DefaultSink { uid, is_default } => {
            if *is_default {
                vec![EngineCommand::SetDefaultSink { uid: uid.clone() }]
            } else {
                Vec::new()
            }
        }
        // A device change rebuilds the graph plan: one debounced snapshot
        // (`AppState::apply` schedules it, `flush_engine` sends it).
        UiAction::DeviceChanged { .. } => Vec::new(),
        // Structural changes: the whole list, because the graph plan depends on
        // every strip (names, routes, ordinals).
        UiAction::AddStrip { .. } | UiAction::Delete { .. } | UiAction::Rename { .. } => {
            vec![EngineCommand::SyncSnapshot(strips.to_vec())]
        }
        // Assigning applications changes nothing in the graph plan: the
        // applications are moved (`app_move`), not the strips.
        UiAction::SelectAppsDone { .. } => Vec::new(),
        UiAction::MidiLearn { .. }
        | UiAction::MidiLearnTarget { .. }
        | UiAction::MidiLearnCancel
        | UiAction::ClearMidiMappings { .. }
        | UiAction::OpenMidiPortPicker
        | UiAction::MidiPortSelected { .. }
        | UiAction::BeginRename { .. }
        | UiAction::RequestDelete { .. }
        | UiAction::BeginAddStrip { .. }
        | UiAction::OpenFxSettings { .. }
        | UiAction::SelectApps { .. }
        | UiAction::OpenDevicePicker { .. }
        | UiAction::RestartEngine
        | UiAction::OpenDiagnostics
        | UiAction::OpenProjectPage
        | UiAction::OpenContextMenu { .. }
        | UiAction::SetAlwaysOnTop { .. } => Vec::new(),
    }
}

/// Does this action change something the config must remember?
pub fn is_persistent(action: &UiAction) -> bool {
    matches!(
        action,
        UiAction::Volume { .. }
            | UiAction::Mute { .. }
            | UiAction::Mono { .. }
            | UiAction::ToggleFx { .. }
            | UiAction::FxParam { .. }
            | UiAction::Route { .. }
            | UiAction::DefaultSink { .. }
            | UiAction::DeviceChanged { .. }
            | UiAction::Rename { .. }
            | UiAction::AddStrip { .. }
            | UiAction::Delete { .. }
            | UiAction::SelectAppsDone { .. }
            | UiAction::SetAlwaysOnTop { .. }
            | UiAction::MidiPortSelected { .. }
    )
}

/// The label of a strip, for a status line (its uid when it no longer exists).
fn strip_label(state: &AppState, uid: &str) -> String {
    state
        .strips
        .iter()
        .find(|strip| strip.uid == uid)
        .map(|strip| strip.label.clone())
        .unwrap_or_else(|| uid.to_string())
}

/// The single entry point: lock the width, tick, animate, draw, draw dialogs.
pub fn render(state: &mut AppState, ui: &mut egui::Ui) {
    let time = ui.input(|i| i.time).max(0.0);
    let now = Instant::now();
    // Horizontal rule: the width is derived from the strip count, not chosen by
    // the user.  Done here (and not in the eframe shell) so the app, the
    // headless snapshot and the tests all go through the same geometry code.
    crate::layout::enforce_window_width(ui.ctx(), &state.strips);
    let screen = ui.ctx().content_rect();
    state.note_window_size(screen.width(), screen.height(), now);
    state.tick(now);
    state.animate(time as f32);
    // The always-on-top flag is a viewport property (Qt's
    // `WindowStaysOnTopHint`): sent once per change, so a WM that ignores it can
    // re-apply it by toggling the setting.
    apply_window_level(state, ui.ctx());
    crate::views::main_window::render(state, ui);
    draw_rename_dialog(state, ui);
    crate::dialogs::draw(state, ui);
    // The context menu is drawn last: it floats above every dialog (a menu
    // opened over a dialog must stay reachable).
    crate::menus::draw(state, ui);
}

/// Send `WindowLevel::AlwaysOnTop` when the flag changed (Qt's
/// `setWindowFlags(WindowStaysOnTopHint)`, which the Python app applied
/// unconditionally; here it is a stored preference).
fn apply_window_level(state: &mut AppState, ctx: &egui::Context) {
    if state.applied_window_level == Some(state.always_on_top) {
        return;
    }
    let level = if state.always_on_top {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    };
    ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
    state.applied_window_level = Some(state.always_on_top);
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

    /// A bridge whose every command fails — the rollback path.
    fn failing_bridge() -> EngineBridge {
        EngineBridge::spawn_with(|command: &EngineCommand, _levels| {
            Err(format!("backend refused {command:?}"))
        })
    }

    /// Drain the engine events (the worker is a thread: poll with a bound).
    fn wait_for_engine(state: &mut AppState, mut done: impl FnMut(&AppState) -> bool) -> bool {
        for _ in 0..400 {
            state.poll_engine_at(state.clock());
            if done(state) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
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
            s.strips
                .iter()
                .filter(|s| s.kind == StripType::Input)
                .count(),
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

    // --- step 1: loading ----------------------------------------------------

    /// A copy of the repository's `config.json` in a scratch dir, so a test that
    /// mutates the model can never write to the real fixture.
    fn scratch_config(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("holaf-ui-app-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config.json"),
            &path,
        )
        .unwrap();
        path
    }

    #[test]
    fn from_config_loads_the_real_strips_and_geometry() {
        let s = AppState::from_config(scratch_config("load"));
        assert_eq!(s.strips.len(), 7);
        assert_eq!(
            s.strips
                .iter()
                .filter(|st| st.kind == StripType::Input)
                .count(),
            4
        );
        // The geometry of the `window` object is picked up (height only).
        assert!(s.geometry().is_some_and(|g| g.height >= 300.0));
        // Persistence is attached: the store knows its path.
        assert!(s.config.is_some());
        // Nothing is disconnected: no bridge was attached.
        assert_eq!(s.engine, EngineState::None);
        // The status bar has no warning for a valid config.
        assert!(s.status.is_none(), "{:?}", s.status);
        // Dropping the state must NOT rewrite the fixture (nothing is dirty).
        let path = s.config.as_ref().unwrap().path().to_path_buf();
        let before = std::fs::read_to_string(&path).unwrap();
        drop(s);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_missing_config_starts_empty_with_a_visible_warning() {
        let dir = std::env::temp_dir().join(format!(
            "holaf-ui-app-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let s = AppState::from_config(&path);
        assert!(s.strips.is_empty(), "no demo strips on a first run");
        // The window is then the minimum width (two empty sections).
        assert_eq!(crate::layout::required_width_for(&s.strips), 320);
        let status = s.status.clone().expect("a visible warning");
        assert!(status.contains("no config"), "{status}");
        // Nothing was created on disk.
        assert!(!path.exists());
    }

    #[test]
    fn a_corrupt_config_is_reported_and_left_alone() {
        let dir = std::env::temp_dir().join(format!(
            "holaf-ui-app-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{ broken").unwrap();
        let s = AppState::from_config(&path);
        assert!(s.strips.is_empty());
        assert!(s
            .status
            .as_deref()
            .is_some_and(|status| status.contains("cannot load")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");
    }

    #[test]
    fn a_config_theme_path_is_honoured_and_a_bad_one_falls_back() {
        let dir = std::env::temp_dir().join(format!(
            "holaf-ui-app-theme-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let theme_path = dir.join("my.json");
        let mut theme = ThemeFile::holaf_dark();
        theme.name = "custom".to_string();
        theme.save(&theme_path).unwrap();

        let config_path = dir.join("config.json");
        std::fs::write(
            &config_path,
            "{\"theme\": \"my.json\", \"strips\": [{\"uid\":\"a\",\"label\":\"A\",\"kind\":\"input\"}]}",
        )
        .unwrap();
        let s = AppState::from_config(&config_path);
        assert_eq!(s.theme.name, "custom");
        assert_eq!(s.strips.len(), 1);

        // An unknown theme name warns and keeps the built-in theme.
        std::fs::write(&config_path, "{\"theme\": \"nope.json\", \"strips\": []}").unwrap();
        let s = AppState::from_config(&config_path);
        assert_eq!(s.theme, ThemeFile::holaf_dark());
    }

    // --- interactions -------------------------------------------------------

    #[test]
    fn volume_action_updates_the_model_and_sends_a_engine_command() {
        let mut s = state();
        s.apply(UiAction::Volume {
            uid: "in-mic".to_string(),
            volume: 0.42,
        });
        assert!((s.strips[0].volume - 0.42).abs() < 1e-6);
        assert_eq!(s.pending_action_count(), 1);
        let actions = s.take_actions();
        assert_eq!(
            engine_commands(&actions[0], &s.strips),
            vec![EngineCommand::SetVolume {
                uid: "in-mic".to_string(),
                volume: 0.42,
            }]
        );
        assert_eq!(s.pending_action_count(), 0);
    }

    #[test]
    fn a_fader_drag_is_coalesced_to_25_hz_with_the_last_value() {
        let mut s = state();
        let bridge =
            EngineBridge::spawn_with(|command: &EngineCommand, _| Ok(format!("{command:?}")));
        s.attach_engine(bridge);
        let start = Instant::now();
        s.set_clock(start);
        // 10 moves in 100 ms (a 100 Hz drag): the first goes out, the rest are
        // coalesced into one trailing send.
        for step in 0..10 {
            s.set_clock(start + Duration::from_millis(step * 11));
            s.apply(UiAction::Volume {
                uid: "in-mic".to_string(),
                volume: 0.10 + step as f32 * 0.01,
            });
        }
        let actions: Vec<UiAction> = s.take_actions();
        assert_eq!(actions.len(), 10, "the model follows the drag 1:1");
        assert!((s.strips[0].volume - 0.19).abs() < 1e-6);
        // Nothing due yet (the last record was 11 ms ago).
        s.flush_engine(start + Duration::from_millis(120));
        // After the window, the last value is sent exactly once.
        s.flush_engine(start + Duration::from_millis(160));
        // The event stream proves how many `set_volume` reached the engine.
        let mut seen = Vec::new();
        for _ in 0..100 {
            for event in s.bridge.as_ref().unwrap().drain_events() {
                if let EngineEvent::Applied(what) = event {
                    if what.contains("SetVolume") {
                        seen.push(what);
                    }
                }
            }
            if seen.len() >= 4 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // 10 moves in 110 ms reach the engine at most once per 40 ms window
        // (the first of each window is immediate, the rest is one trailing
        // value): 4 calls instead of 10.
        assert!(seen.len() <= 4, "{seen:?}");
        assert!(seen.len() >= 2, "{seen:?}");
        assert!(seen[0].contains("0.1"), "{seen:?}");
        assert!(seen.last().unwrap().contains("0.19"), "{seen:?}");
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
        let actions = s.take_actions();
        assert_eq!(
            engine_commands(&actions[0], &s.strips),
            vec![EngineCommand::SetMute {
                uid: "in-web".to_string(),
                mute: true
            }]
        );
        assert_eq!(
            engine_commands(&actions[1], &s.strips),
            vec![EngineCommand::SetMono {
                uid: "in-web".to_string(),
                mono: true
            }]
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
    fn the_fx_dialog_opens_edits_and_pushes_debounced_params() {
        let mut s = state();
        s.apply(UiAction::OpenFxSettings {
            uid: "in-mic".to_string(),
            effect: EffectKey::Eq,
        });
        let dialog = s.fx_dialog.as_ref().expect("dialog opened");
        assert_eq!(dialog.uid, "in-mic");
        assert_eq!(dialog.effect, EffectKey::Eq);
        assert_eq!(dialog.label, "Mic");
        assert_eq!(dialog.params.len(), 15);
        // Opening a dialog sends nothing.
        assert!(engine_commands(
            &UiAction::OpenFxSettings {
                uid: "in-mic".into(),
                effect: EffectKey::Eq
            },
            &s.strips
        )
        .is_empty());

        // Two rapid slider moves: the model follows immediately, the engine gets
        // the last value once.
        let start = Instant::now();
        s.set_clock(start);
        for (i, value) in [3.0_f32, 6.0].iter().enumerate() {
            s.set_clock(start + Duration::from_millis(i as u64 * 20));
            s.apply(UiAction::FxParam {
                uid: "in-mic".to_string(),
                effect: EffectKey::Eq,
                param: "440Hz gain".to_string(),
                value: *value,
            });
        }
        let mic = s.strips.iter().find(|st| st.uid == "in-mic").unwrap();
        assert_eq!(mic.effects[&EffectKey::Eq].params["440Hz gain"], 6.0);
        assert_eq!(
            s.fx_dialog.as_ref().unwrap().params["440Hz gain"],
            6.0,
            "the dialog working copy follows too"
        );
        // Not due before the debounce.
        s.flush_engine(start + Duration::from_millis(100));
        s.flush_engine(start + Duration::from_millis(FX_PARAMS_DEBOUNCE.as_millis() as u64 + 100));
        assert!(s.fx_pending.is_empty(), "the pending push was sent");
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
    fn default_sink_is_exclusive_and_only_the_check_sinks() {
        let mut s = state();
        assert!(
            s.strips
                .iter()
                .find(|s| s.uid == "in-default")
                .unwrap()
                .is_default
        );
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
        let actions = s.take_actions();
        assert_eq!(
            engine_commands(&actions[0], &s.strips),
            vec![EngineCommand::SetDefaultSink {
                uid: "in-web".to_string()
            }]
        );
        // Unchecking is allowed and leaves nobody default.
        s.apply(UiAction::DefaultSink {
            uid: "in-web".to_string(),
            is_default: false,
        });
        assert!(s.strips.iter().all(|s| !s.is_default));
        let actions = s.take_actions();
        assert!(engine_commands(&actions[0], &s.strips).is_empty());
    }

    #[test]
    fn a_device_change_rebuilds_the_graph_once_after_the_debounce() {
        let mut s = state();
        let bridge =
            EngineBridge::spawn_with(|command: &EngineCommand, _| Ok(format!("{command:?}")));
        s.attach_engine(bridge);
        let start = Instant::now();
        s.set_clock(start);
        // Three quick changes (a user scrolling the combo) → one snapshot.
        for (i, device) in ["alsa_output.a", "alsa_output.b", "alsa_output.c"]
            .iter()
            .enumerate()
        {
            s.set_clock(start + Duration::from_millis(i as u64 * 50));
            s.apply(UiAction::DeviceChanged {
                uid: "out-mix".to_string(),
                device: Some((*device).to_string()),
            });
        }
        assert_eq!(
            s.strips[s.strip_index("out-mix").unwrap()].kind,
            StripType::Output,
            "a device makes the bus a physical output"
        );
        // Nothing sent yet, one snapshot after the debounce.
        s.flush_engine(start + Duration::from_millis(300));
        let mut snapshots = Vec::new();
        for _ in 0..400 {
            for event in s.bridge.as_ref().unwrap().drain_events() {
                if let EngineEvent::Applied(what) = event {
                    if what.contains("SyncSnapshot") {
                        snapshots.push(what);
                    }
                }
            }
            if snapshots.iter().any(|what| what.contains("alsa_output.c")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // One snapshot carries the final device (the startup snapshot of
        // `attach_engine` is another, unrelated one).
        let with_device: Vec<&String> = snapshots
            .iter()
            .filter(|what| what.contains("alsa_output.c"))
            .collect();
        assert_eq!(with_device.len(), 1, "{snapshots:?}");
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
    fn the_device_picker_asks_the_engine_for_its_list() {
        let mut s = state();
        s.apply(UiAction::OpenDevicePicker {
            uid: "out-mix".to_string(),
        });
        let picker = s.device_picker.as_ref().expect("picker opened");
        assert_eq!(picker.uid, "out-mix");
        assert_eq!(picker.kind, StripType::Output);
        assert_eq!(picker.current, None, "the bus has no device");
        // The command itself is the DeviceList *request*: no strip mutation.
        assert!(engine_commands(
            &UiAction::OpenDevicePicker {
                uid: "out-mix".into()
            },
            &s.strips
        )
        .is_empty());
    }

    #[test]
    fn deleting_needs_a_confirmation_before_anything_happens() {
        let mut s = state();
        s.set_levels("out-hp", [0.5, 0.5]);
        s.apply(UiAction::RequestDelete {
            uid: "out-hp".to_string(),
        });
        // Nothing removed yet, an engine command? None.
        assert_eq!(s.strips.len(), 7);
        assert_eq!(s.delete_confirm.as_ref().unwrap().label, "HP");
        let actions = s.take_actions();
        assert!(engine_commands(&actions[0], &s.strips).is_empty());

        s.apply(UiAction::Delete {
            uid: "out-hp".to_string(),
        });
        assert!(s.delete_confirm.is_none());
        assert_eq!(s.strips.len(), 6);
        assert!(s.strips.iter().all(|st| st.uid != "out-hp"));
        // `in-web` used to route to `out-hp`.
        let web = s.strips.iter().find(|s| s.uid == "in-web").unwrap();
        assert!(web.routes.is_empty(), "{:?}", web.routes);
        assert_ne!(s.levels_for(s.strip_index("in-web").unwrap()), [0.0, 0.0]);
        assert_eq!(s.status.as_deref(), Some("Removed HP"));
        // The delete is one snapshot of the new list.
        let actions = s.take_actions();
        let commands = engine_commands(&actions[0], &s.strips);
        assert!(matches!(commands.as_slice(), [EngineCommand::SyncSnapshot(s)] if s.len() == 6));
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
        s.apply(UiAction::OpenFxSettings {
            uid: "ghost".to_string(),
            effect: EffectKey::Eq,
        });
        s.apply(UiAction::RequestDelete {
            uid: "ghost".to_string(),
        });
        assert_eq!(s.strips.len(), before);
        assert!(s.fx_dialog.is_none());
        assert!(s.delete_confirm.is_none());
        assert_eq!(s.levels_for(999), [0.0, 0.0]);
    }

    #[test]
    fn adding_a_strip_opens_a_dialog_and_then_inserts_it() {
        let mut s = state();
        s.apply(UiAction::BeginAddStrip {
            kind: StripType::Input,
        });
        assert_eq!(s.add_dialog.as_ref().unwrap().kind, StripType::Input);
        // Confirming with a label uses it.
        s.apply(UiAction::AddStrip {
            kind: StripType::Input,
            label: Some("  Line in  ".to_string()),
        });
        assert!(s.add_dialog.is_none());
        assert_eq!(s.strips.len(), 8);
        assert_eq!(s.strips[4].label, "Line in");
        assert_eq!(s.strips[4].kind, StripType::Input);
        assert_eq!(s.strips[5].label, "Casque");
        assert_eq!(s.strips[4].uid, "in-1");
        // A blank label falls back to the automatic name.
        s.apply(UiAction::AddStrip {
            kind: StripType::Output,
            label: Some("   ".to_string()),
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
        // A rename rebuilds the graph (the bus name carries the label).
        let actions = s.take_actions();
        assert!(matches!(
            engine_commands(&actions[1], &s.strips).as_slice(),
            [EngineCommand::SyncSnapshot(_)]
        ));

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
        assert!(s.status.as_deref().unwrap().contains("MIDI"));
        s.apply(UiAction::MidiLearn {
            uid: "in-chat".to_string(),
            learning: false,
        });
        assert!(s.midi_learning.is_none());
        assert!(s.status.as_deref().unwrap().contains("cancelled"));
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
            UiAction::RequestDelete {
                uid: "in-mic".to_string(),
            },
            UiAction::BeginAddStrip {
                kind: StripType::Input,
            },
            UiAction::OpenFxSettings {
                uid: "in-mic".to_string(),
                effect: EffectKey::Eq,
            },
            UiAction::MidiLearn {
                uid: "in-mic".to_string(),
                learning: true,
            },
        ] {
            assert!(
                engine_commands(&action, &[]).is_empty(),
                "{action:?} must not reach the engine"
            );
        }
    }

    // --- meters ------------------------------------------------------------

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
    fn engine_levels_decay_when_the_engine_goes_quiet() {
        let mut s = state();
        let bridge = EngineBridge::spawn_with(|command: &EngineCommand, levels| {
            if let EngineCommand::SetVolume { uid, volume } = command {
                levels.insert(uid.clone(), [*volume, *volume]);
            }
            Ok("ok".to_string())
        });
        s.attach_engine(bridge);
        let start = Instant::now();
        s.set_clock(start);
        s.apply(UiAction::Volume {
            uid: "in-mic".to_string(),
            volume: 0.6,
        });
        // The mock reports levels through `Levels` events.
        assert!(wait_for_engine(&mut s, |state| state.levels_for(0)[0] > 0.5));
        // Then silence: the envelope pulls the bar down by 15 % per tick.
        let high = s.levels_for(0)[0];
        s.poll_engine_at(start + Duration::from_millis(40));
        let after = s.levels_for(0)[0];
        assert!(after < high, "{after} must decay from {high}");
        assert!((after - high * 0.85).abs() < 0.1, "{after} vs {high}");
    }

    #[test]
    fn a_meter_board_feeds_the_bars_and_decays_a_missing_source() {
        let mut s = state();
        let board = MeterBoard::new();
        // Only one strip has a meter source (the others are silently unmatched).
        let slots = board.assign(&["in-mic".to_string()]);
        // `in-mic` is mono in the demo, so both bars take the peak.
        board.publish(slots["in-mic"], [0.4, 0.8]);
        let now = Instant::now();
        s.feed_from_board(&board, now, METER_INTERVAL);
        assert_eq!(s.levels_for(0), [0.8, 0.8]);
        // A strip the engine does not meter is reported once…
        assert_eq!(s.meter_missing.len(), s.strips.len() - 1);
        // …and decays to silence (the demo state started with animated levels,
        // so it is not dark on the very first tick).
        for tick in 1..80 {
            s.feed_from_board(&board, now + METER_INTERVAL * tick, METER_INTERVAL);
        }
        assert!(s.levels_for(2)[0] < 1e-4, "{:?}", s.levels_for(2));
        // Silence on the board decays the bar instead of freezing it.
        board.publish(slots["in-mic"], [0.0, 0.0]);
        s.feed_from_board(&board, now + 2 * METER_INTERVAL, METER_INTERVAL);
        let after = s.levels_for(0)[0];
        assert!(after > 0.0 && after < 0.8, "{after}");
    }

    #[test]
    fn the_mock_bridge_has_no_meter_board_but_the_engine_does_not_matter() {
        // Guard the contract `poll_engine` relies on: a bridge without a board
        // (mock, tests) still drains its events without touching the meters.
        let mut s = state();
        s.attach_engine(EngineBridge::spawn_mock());
        assert!(s.bridge.as_ref().unwrap().meter_board().is_none());
        assert!(wait_for_engine(&mut s, |state| state.engine == EngineState::Connected));
        s.shutdown();
        assert_eq!(s.engine, EngineState::None);
    }

    // --- engine plumbing ---------------------------------------------------

    #[test]
    fn engine_commands_cover_every_audio_action() {
        let strips = state().strips.clone();
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
        ];
        for case in cases {
            assert!(
                !engine_commands(&case, &strips).is_empty(),
                "{case:?} must reach the engine"
            );
        }
        // Unchecking "Default Sink" has no engine counterpart.
        assert!(engine_commands(
            &UiAction::DefaultSink {
                uid: "u".into(),
                is_default: false
            },
            &strips
        )
        .is_empty());
        // A device change is a *debounced* snapshot, not an immediate command.
        assert!(engine_commands(
            &UiAction::DeviceChanged {
                uid: "u".into(),
                device: None
            },
            &strips
        )
        .is_empty());
        // Structural changes carry the whole list.
        assert!(matches!(
            engine_commands(
                &UiAction::AddStrip {
                    kind: StripType::Input,
                    label: None
                },
                &strips
            )
            .as_slice(),
            [EngineCommand::SyncSnapshot(list)] if list.len() == strips.len()
        ));
    }

    #[test]
    fn persistent_actions_mark_the_config_dirty() {
        let mut s = AppState::from_config(scratch_config("dirty"));
        let path = s.config.as_ref().unwrap().path().to_path_buf();
        let mic = s
            .strips
            .iter()
            .position(|strip| strip.label == "Mic")
            .expect("the fixture has a Mic strip");
        let mic_uid = s.strips[mic].uid.clone();
        let before = std::fs::read_to_string(&path).unwrap();
        let start = Instant::now();
        s.set_clock(start);
        s.apply(UiAction::Volume {
            uid: mic_uid.clone(),
            volume: 0.33,
        });
        assert!(
            s.config.as_ref().unwrap().save_pending(),
            "a volume is persisted"
        );
        // Opening a dialog is not persistent.
        s.apply(UiAction::OpenFxSettings {
            uid: mic_uid.clone(),
            effect: EffectKey::Gate,
        });
        // Writing happens on the debounce, one atomic write for the burst.
        s.save_pending(start);
        assert!(
            std::fs::read_to_string(&path).unwrap() == before,
            "still debouncing"
        );
        let strips_before = s.strips.clone();
        s.save_pending(start + Duration::from_millis(500));
        assert!(!s.config.as_ref().unwrap().save_pending());
        let saved = holaf_core::config::load_config(&path).unwrap();
        let saved_mic = saved["strips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["uid"].as_str() == Some(mic_uid.as_str()))
            .expect("the Mic strip is still in the document");
        assert!((saved_mic["volume"].as_f64().unwrap() - 0.33).abs() < 1e-6);
        // The unknown per-strip keys are still there.
        assert_eq!(saved["strips"][0]["midi_volume"]["control"], 61);
        assert_eq!(mic, 2, "the fixture order is unchanged");
        assert_eq!(s.strips, strips_before);
        // Leave the fixture untouched from here on.
        s.strips = Vec::new();
        s.config.as_mut().unwrap().cancel_save();
    }

    #[test]
    fn a_failed_command_rolls_the_model_back() {
        let mut s = state();
        s.attach_engine(failing_bridge());
        s.apply(UiAction::Mute {
            uid: "in-web".to_string(),
            mute: true,
        });
        assert!(s.strips[2].mute, "optimistic UI");
        assert!(
            wait_for_engine(&mut s, |state| !state.strips[2].mute),
            "the refusal must revert the toggle: {:?}",
            s.status
        );
        assert!(s
            .status
            .as_deref()
            .is_some_and(|status| status.contains("reverted")));
    }

    #[test]
    fn a_failed_fader_move_restores_the_previous_volume() {
        let mut s = state();
        s.attach_engine(failing_bridge());
        let before = s.strips[0].volume;
        s.apply(UiAction::Volume {
            uid: "in-mic".to_string(),
            volume: 0.01,
        });
        assert_eq!(s.strips[0].volume, 0.01);
        assert!(wait_for_engine(&mut s, |state| {
            (state.strips[0].volume - before).abs() < 1e-6
        }));
    }

    #[test]
    fn a_failed_effect_toggle_restores_the_previous_state() {
        let mut s = state();
        s.attach_engine(failing_bridge());
        // The demo `out-hp` has no tube active.
        s.apply(UiAction::ToggleFx {
            uid: "out-hp".to_string(),
            effect: EffectKey::Tube,
            active: true,
        });
        assert!(s.strips[5].effects[&EffectKey::Tube].active);
        assert!(wait_for_engine(&mut s, |state| {
            !state.strips[5].effects[&EffectKey::Tube].active
        }));
    }

    #[test]
    fn a_disconnected_engine_is_a_banner_not_a_crash() {
        let mut s = state();
        s.engine = EngineState::Disconnected("cannot connect to PipeWire".to_string());
        assert!(s.is_disconnected());
        assert_eq!(
            s.engine_error().as_deref(),
            Some("cannot connect to PipeWire")
        );
        // Actions keep working on the model, are logged and cannot reach a
        // bridge (there is none here).
        s.apply(UiAction::Mute {
            uid: "in-web".to_string(),
            mute: true,
        });
        assert!(s.strips[2].mute);
        assert!(s
            .status
            .as_deref()
            .is_some_and(|status| status.contains("engine disconnected")));
    }

    #[test]
    fn structural_actions_push_a_full_snapshot_to_the_bridge() {
        let mut s = state();
        s.bridge = Some(EngineBridge::spawn_mock());
        s.apply(UiAction::Rename {
            uid: "in-mic".to_string(),
            label: "Micro".to_string(),
        });
        let mut seen = Vec::new();
        for _ in 0..200 {
            for event in s.bridge.as_ref().unwrap().drain_events() {
                seen.push(format!("{event:?}"));
            }
            if seen.iter().any(|what| what.contains("snapshot 7 strips")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            seen.iter().any(|what| what.contains("snapshot 7 strips")),
            "{seen:?}"
        );
        s.shutdown();
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
    fn shutdown_stops_the_engine_thread_and_persists() {
        let mut s = AppState::from_config(scratch_config("shutdown"));
        s.attach_engine(EngineBridge::spawn_mock());
        let start = Instant::now();
        s.set_clock(start);
        let mic_uid = s.strips[2].uid.clone();
        assert_eq!(s.strips[2].label, "Mic");
        s.apply(UiAction::Volume {
            uid: mic_uid.clone(),
            volume: 0.5,
        });
        s.shutdown();
        assert!(s.bridge.is_none());
        assert_eq!(s.engine, EngineState::None);
        // The pending write was flushed on shutdown.
        let path = s.config.as_ref().unwrap().path().to_path_buf();
        let saved = holaf_core::config::load_config(&path).unwrap();
        let saved_mic = saved["strips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["uid"].as_str() == Some(mic_uid.as_str()))
            .expect("the Mic strip is still in the document");
        assert!((saved_mic["volume"].as_f64().unwrap() - 0.5).abs() < 1e-6);
        s.config.as_mut().unwrap().cancel_save();
    }

    #[test]
    fn window_geometry_changes_schedule_a_debounced_save() {
        let mut s = state();
        s.config = Some(
            UiConfig::open(
                std::env::temp_dir().join(format!("holaf-ui-geom-{}.json", std::process::id())),
            )
            .unwrap(),
        );
        let now = Instant::now();
        s.set_clock(now);
        // Startup: the same size frame after frame is not a gesture, so nothing
        // is scheduled no matter how long it lasts.
        for _ in 0..RESIZE_ARM_FRAMES + 5 {
            s.note_window_size(794.0, 600.0, now);
        }
        assert!(s.geometry().is_some());
        assert!(!s.config.as_ref().unwrap().save_pending());
        // A sub-pixel jitter is not a resize either…
        s.note_window_size(794.0, 600.5, now);
        assert!(!s.config.as_ref().unwrap().save_pending());
        // …a real drag is.
        s.note_window_size(794.0, 700.0, now);
        assert!(s.config.as_ref().unwrap().save_pending());
        assert_eq!(s.geometry().unwrap().height, 700.0);
        s.config.as_mut().unwrap().cancel_save();
    }

    /// F4: a launch where the window manager (or `MinInnerSize`) gives the window
    /// a height other than the stored one must not rewrite `config.json`.  Ten
    /// frames without any interaction leave the file — and the value we would
    /// persist — untouched.
    #[test]
    fn rendering_frames_without_a_resize_never_writes_the_config() {
        let path = scratch_config("no-write-on-start");
        let stored = AppState::from_config(&path)
            .geometry()
            .expect("the fixture has a window height")
            .height;
        let mut s = AppState::from_config(&path);
        let now = Instant::now();
        s.set_clock(now);
        // The WM hands us 152 px more than the file says (the real symptom: 423
        // in the file, 575 on screen).
        for _ in 0..10 {
            s.note_window_size(794.0, stored + 152.0, now);
        }
        assert!(
            !s.config.as_ref().unwrap().save_pending(),
            "a non-interactive launch must not schedule a write"
        );
        assert_eq!(
            s.geometry().unwrap().height,
            stored,
            "the stored height is kept"
        );
        s.save_pending(now + crate::config::SAVE_DEBOUNCE * 2);
        assert_eq!(
            holaf_core::config::load_config(&path).unwrap()["window"]["height"].as_f64(),
            Some(stored as f64),
            "the file is byte-for-byte the height the user left"
        );
    }

    /// Step 6: the height survives a run (round-trip through the real file),
    /// the width stays informational and the Qt geometry blob is untouched.
    #[test]
    fn the_window_height_round_trips_through_config_json() {
        let path = scratch_config("geometry-roundtrip");
        let height_before = {
            let document = holaf_core::config::load_config(&path).unwrap();
            document["window"]["height"].as_f64().unwrap()
        };
        let qt_blob_before = {
            let document = holaf_core::config::load_config(&path).unwrap();
            document["window_geometry"].clone()
        };
        let mut s = AppState::from_config(&path);
        assert!((s.geometry().unwrap().height as f64 - height_before).abs() < 0.01);
        let now = Instant::now();
        s.set_clock(now);
        // Startup settle (the size the file holds), then the user drags taller.
        for _ in 0..RESIZE_ARM_FRAMES + 2 {
            s.note_window_size(794.0, height_before as f32, now);
        }
        s.note_window_size(794.0, 812.0, now);
        // Below the 400 ms debounce nothing is written…
        s.save_pending(now);
        assert_eq!(
            holaf_core::config::load_config(&path).unwrap()["window"]["height"].as_f64(),
            Some(height_before),
            "still debouncing"
        );
        // …after it, the height is on disk.
        s.save_pending(now + crate::config::SAVE_DEBOUNCE);
        let document = holaf_core::config::load_config(&path).unwrap();
        assert_eq!(document["window"]["height"].as_f64(), Some(812.0));
        // F5: the fixture has no `window.width` and the UI must not add one (the
        // width is locked to the strip count, not a user choice).
        assert!(
            document["window"].get("width").is_none(),
            "no width key added: {}",
            document["window"]
        );
        // The Qt blob (`window_geometry`, a base64 `saveGeometry()`) survives
        // verbatim: the Rust UI must never destroy the Python UI's geometry.
        assert_eq!(document["window_geometry"], qt_blob_before);
        drop(s);
        // A fresh start reuses the saved height (this is what `main.rs` sends
        // as `ViewportCommand::InnerSize`), and the strips are untouched.
        let reopened = AppState::from_config(&path);
        let geometry = reopened.geometry().expect("geometry restored");
        assert!((geometry.height - 812.0).abs() < 0.01);
        assert_eq!(reopened.strips.len(), 7);
    }

    // --- step 4: context menu actions ---------------------------------------

    /// A bridge that records every command it is given (as `Dedup`-proof
    /// strings) so a test can count what reached the "engine".
    fn recording_bridge() -> (EngineBridge, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let thread_log = std::sync::Arc::clone(&log);
        let bridge = EngineBridge::spawn_with(move |command: &EngineCommand, _levels| {
            if let Ok(mut guard) = thread_log.lock() {
                guard.push(format!("{command:?}"));
            }
            Ok("ok".to_string())
        });
        (bridge, log)
    }

    fn commands(bridge: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
        bridge.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    /// Wait (bounded) until `predicate` holds on the recorded commands: the
    /// engine thread is a thread, so a recorded command arrives asynchronously.
    fn wait_commands(
        log: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        mut predicate: impl FnMut(&[String]) -> bool,
    ) -> Vec<String> {
        for _ in 0..400 {
            let seen = commands(log);
            if predicate(&seen) {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        commands(log)
    }

    #[test]
    fn the_restart_action_is_anti_reentrant_and_reports_its_outcome() {
        let mut s = state();
        s.attach_engine(EngineBridge::spawn_mock());
        let now = Instant::now();
        s.set_clock(now);
        s.apply(UiAction::RestartEngine);
        assert!(s.restarting);
        assert_eq!(s.engine, EngineState::Connecting);
        assert!(s
            .status
            .as_deref()
            .is_some_and(|status| status.contains("restarting")));
        // A second click while it runs is refused, not queued.
        s.apply(UiAction::RestartEngine);
        assert!(s
            .status
            .as_deref()
            .is_some_and(|status| status.contains("already running")));
        // The engine thread answers with `Restarted`, which clears the guard.
        assert!(wait_for_engine(&mut s, |state| !state.restarting));
        assert_eq!(s.engine, EngineState::Connected);
        s.shutdown();
    }

    #[test]
    fn the_restart_action_is_refused_without_an_engine() {
        let mut s = state();
        s.apply(UiAction::RestartEngine);
        assert!(!s.restarting);
        assert!(s
            .status
            .as_deref()
            .is_some_and(|status| status.contains("no audio engine")));
    }

    #[test]
    fn the_debug_window_collects_a_report_from_the_engine_status() {
        let mut s = state();
        s.attach_engine(EngineBridge::spawn_mock());
        s.apply(UiAction::OpenDiagnostics);
        // Opened immediately (before the answer) with an "unreachable" report.
        let diagnostics = s.diagnostics.as_ref().expect("dialog opened");
        assert!(!diagnostics.report.engine.available);
        assert_eq!(diagnostics.tab, crate::diagnostics::DiagTab::Summary);
        // The `status` answer refreshes it in place.
        assert!(wait_for_engine(&mut s, |state| {
            state
                .diagnostics
                .as_ref()
                .is_some_and(|diagnostics| diagnostics.report.engine.available)
        }));
        let report = &s.diagnostics.as_ref().unwrap().report;
        assert!(report.engine.running);
        assert_eq!(report.engine.fx_host.mode, "external");
        assert!(report.text().contains("ENGINE"));
        // The report is what "Copy report" puts on the clipboard.
        assert!(report.text().contains("FINDINGS"));
        s.shutdown();
    }

    #[test]
    fn the_always_on_top_flag_is_stored_in_the_rust_only_key() {
        let mut s = AppState::from_config(scratch_config("aot"));
        let path = s.config.as_ref().unwrap().path().to_path_buf();
        // Qt pinned the window on top unconditionally; the Rust UI defaults to
        // the same behaviour.
        assert!(s.always_on_top);
        let now = Instant::now();
        s.set_clock(now);
        s.apply(UiAction::SetAlwaysOnTop { value: false });
        assert!(!s.always_on_top);
        s.save_pending(now + crate::config::SAVE_DEBOUNCE);
        let document = holaf_core::config::load_config(&path).unwrap();
        assert_eq!(
            document["ui_rust"]["always_on_top"],
            serde_json::json!(false)
        );
        let reopened = AppState::from_config(&path);
        assert!(!reopened.always_on_top);
        drop(reopened);
        s.strips = Vec::new();
        s.config.as_mut().unwrap().cancel_save();
    }

    /// The flag reaches the viewport exactly once per change (a
    /// `WindowLevel(AlwaysOnTop)` command), which is what the WM acts on.
    #[test]
    fn the_window_level_is_sent_once_per_change() {
        let ctx = crate::test_support::headless_context();
        let mut s = AppState::demo();
        s.applied_window_level = None;
        let run = |s: &mut AppState| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                time: Some(1.0),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| render(s, ui));
            output.textures_delta.clear();
            output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .map(|viewport| viewport.commands.clone())
                .unwrap_or_default()
        };
        let commands = run(&mut s);
        assert!(
            commands.iter().any(|command| matches!(
                command,
                egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop)
            )),
            "{commands:?}"
        );
        // Nothing to say on the next frame: same flag.
        let commands = run(&mut s);
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::WindowLevel(_))),
            "{commands:?}"
        );
        // Turning it off sends `Normal` once.
        s.apply(UiAction::SetAlwaysOnTop { value: false });
        let commands = run(&mut s);
        assert!(
            commands.iter().any(|command| matches!(
                command,
                egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal)
            )),
            "{commands:?}"
        );
    }

    // --- step 1: application routing ---------------------------------------

    #[test]
    fn select_apps_opens_the_dialog_over_the_cached_list() {
        let mut s = state();
        s.app_list = crate::apps::AppList::parse(&serde_json::json!({
            "apps": [{"id": 1, "name": "firefox", "pid": 10, "sink": "alsa_output.x"}]
        }));
        s.app_assignments
            .insert("in-mic".to_string(), vec!["zoom".to_string()]);
        s.apply(UiAction::SelectApps {
            uid: "in-mic".to_string(),
        });
        let dialog = s.apps_dialog.as_ref().expect("dialog opened");
        assert_eq!(dialog.uid, "in-mic");
        assert_eq!(dialog.label, "Mic");
        // Union of running apps and already-assigned ones, sorted.
        assert_eq!(dialog.candidates, ["firefox", "zoom"]);
        assert!(dialog.is_selected("zoom") && !dialog.is_selected("firefox"));
        // Opening the dialog sends no engine command beyond the app list…
        assert!(engine_commands(
            &UiAction::SelectApps {
                uid: "in-mic".into()
            },
            &s.strips
        )
        .is_empty());
    }

    #[test]
    fn confirming_the_app_dialog_persists_the_assignment_and_moves_the_apps() {
        let path = scratch_config("apps-confirm");
        let mut s = AppState::from_config(&path);
        let (bridge, log) = recording_bridge();
        s.attach_engine(bridge);
        // Only a **virtual** input takes applications over, so the fixture's
        // `Web` strip (no device) is the one to use — `Mic` is bound to a real
        // source and must never steal an application.
        let uid = s
            .strips
            .iter()
            .find(|strip| strip.label == "Web")
            .map(|strip| strip.uid.clone())
            .expect("the fixture has a Web strip");
        s.node_names
            .insert(uid.clone(), "Holaf_Bus_Web".to_string());
        let now = Instant::now();
        s.set_clock(now);
        s.apply(UiAction::SelectAppsDone {
            uid: uid.clone(),
            names: vec!["firefox".to_string(), "  discord ".to_string()],
        });
        // The names are trimmed, stored and persisted under the Python key.
        assert_eq!(s.assigned_apps(&uid), ["firefox", "discord"]);
        s.save_pending(now + crate::config::SAVE_DEBOUNCE);
        let document = holaf_core::config::load_config(&path).unwrap();
        let entry = document["strips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["uid"].as_str() == Some(uid.as_str()))
            .unwrap();
        assert_eq!(
            entry["assigned_apps"],
            serde_json::json!(["firefox", "discord"])
        );
        // A fresh load reads them back.
        let reopened = AppState::from_config(&path);
        assert_eq!(reopened.assigned_apps(&uid), ["firefox", "discord"]);
        drop(reopened);

        // The engine is asked for the running applications, and the answer moves
        // exactly the assigned ones onto the strip's bus.
        s.app_list = crate::apps::AppList::parse(&serde_json::json!({
            "apps": [
                {"id": 42, "name": "firefox", "sink": "alsa_output.other"},
                {"id": 43, "name": "discord", "sink": "alsa_output.other"},
                {"id": 44, "name": "mpv", "sink": "alsa_output.other"}
            ]
        }));
        s.enforce_applications();
        let seen = wait_commands(&log, |seen| {
            seen.iter()
                .filter(|command| command.contains("AppMove"))
                .count()
                >= 2
        });
        assert!(
            seen.iter()
                .any(|command| command.contains("AppMove") && command.contains("42")),
            "{seen:?}"
        );
        assert!(
            seen.iter()
                .any(|command| command.contains("AppMove") && command.contains("43")),
            "{seen:?}"
        );
        // `mpv` is not assigned: it must never be moved.
        assert!(
            !seen
                .iter()
                .any(|command| command.contains("AppMove") && command.contains("44")),
            "{seen:?}"
        );
        // Idempotence: replaying the *same* list must not move anything twice.
        let moves = |log: &std::sync::Arc<std::sync::Mutex<Vec<String>>>| {
            commands(log)
                .iter()
                .filter(|command| command.contains("AppMove"))
                .count()
        };
        assert_eq!(moves(&log), 2, "{:?}", commands(&log));
        // Once the applications really are on the bus, the answer to the next
        // `app_list` says so and the plan is empty (that is what makes a 5 s
        // timer harmless: it is a no-op on a settled system).
        s.app_list = crate::apps::AppList::parse(&serde_json::json!({
            "apps": [
                {"id": 42, "name": "firefox", "sink": "Holaf_Bus_Web"},
                {"id": 43, "name": "discord", "sink": "Holaf_Bus_Web"}
            ]
        }));
        s.enforce_applications();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(moves(&log), 2, "a settled app is not moved again");
        s.shutdown();
        s.strips = Vec::new();
        let mut s = AppState::from_config(&path);
        s.strips.clear();
        s.config.as_mut().unwrap().cancel_save();
    }

    #[test]
    fn the_enforcement_timer_only_polls_when_a_strip_takes_apps() {
        let mut s = state();
        let (bridge, log) = recording_bridge();
        s.attach_engine(bridge);
        let start = Instant::now();
        s.set_clock(start);
        // Nothing assigned: the engine is never polled.
        for tick in 0..5 {
            s.tick_app_enforcement(start + Duration::from_secs(tick));
        }
        assert!(
            !commands(&log)
                .iter()
                .any(|command| command.contains("AppList")),
            "{:?}",
            commands(&log)
        );
        // Assign one application to the virtual Mic strip: the first tick asks,
        // then every 5 s.
        s.app_assignments
            .insert("in-mic".to_string(), vec!["firefox".to_string()]);
        s.tick_app_enforcement(start);
        let asked = |log: &std::sync::Arc<std::sync::Mutex<Vec<String>>>| {
            commands(log)
                .iter()
                .filter(|command| command.contains("AppList"))
                .count()
        };
        wait_commands(&log, |seen| {
            seen.iter()
                .filter(|command| command.contains("AppList"))
                .count()
                >= 1
        });
        assert_eq!(asked(&log), 1, "{:?}", commands(&log));
        s.tick_app_enforcement(start + Duration::from_secs(1));
        assert_eq!(asked(&log), 1, "no polling before the interval");
        s.tick_app_enforcement(start + Duration::from_secs(6));
        wait_commands(&log, |seen| {
            seen.iter()
                .filter(|command| command.contains("AppList"))
                .count()
                >= 2
        });
        assert_eq!(asked(&log), 2, "one poll per 5 s");
        s.shutdown();
    }

    #[test]
    fn a_stub_app_list_is_never_turned_into_moves() {
        let mut s = state();
        let (bridge, log) = recording_bridge();
        s.attach_engine(bridge);
        s.app_assignments
            .insert("in-mic".to_string(), vec!["firefox".to_string()]);
        s.app_list = crate::apps::AppList::parse(&serde_json::json!({
            "apps": [], "stub": true, "note": "not implemented"
        }));
        s.enforce_applications();
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            !commands(&log)
                .iter()
                .any(|command| command.contains("AppMove")),
            "{:?}",
            commands(&log)
        );
        s.shutdown();
    }

    // --- step 2: MIDI -------------------------------------------------------

    /// The strip's MIDI menu arms one property at a time (`Learn Volume` /
    /// `Learn Mute` / `Learn Mono`), the next control defines it, and the button
    /// confirms with the CC number — the Rust counterpart of the Qt
    /// `midi_learn_requested(uid, prop)` → `LEARNING...` → `mapping_detected`
    /// cycle.
    #[test]
    fn each_learn_entry_binds_only_its_own_property_and_flashes_the_cc() {
        let path = scratch_config("midi-learn-targets");
        let mut s = AppState::from_config(&path);
        let uid = s.strips[0].uid.clone();
        let now = Instant::now();
        s.set_clock(now);
        // The fixture binds channel 0 CC 61 to the volume: it is replaced.
        s.apply(UiAction::MidiLearnTarget {
            uid: uid.clone(),
            target: MidiTarget::Volume,
        });
        assert!(s.is_midi_learning(&uid));
        assert_eq!(s.midi_button_text(&uid).as_deref(), Some("LEARNING..."));
        s.handle_midi_message(
            crate::midi::MidiMessage::Control {
                channel: 0,
                control: 19,
                value: 100,
            },
            now,
        );
        // One gesture, one property: the learn ended, the other two mappings of
        // the strip are untouched.
        assert!(!s.is_midi_learning(&uid));
        let bindings = s.midi_bindings_for(&uid);
        assert!(bindings.contains(&(MidiTarget::Volume, Binding::control(0, 19))));
        assert!(bindings.contains(&(MidiTarget::Mute, Binding::note(0, 24))));
        assert!(bindings.contains(&(MidiTarget::Mono, Binding::note(0, 22))));
        assert_eq!(bindings.len(), 3);
        // Confirmation on the button, for a moment only.
        assert_eq!(s.midi_button_text(&uid).as_deref(), Some("CC 19"));
        s.tick_midi_flash(now + crate::app::MIDI_FLASH_INTERVAL);
        assert_eq!(s.midi_button_text(&uid), None);
        // …and it is in the config, in the Python shape.
        s.save_pending(now + crate::config::SAVE_DEBOUNCE);
        let document = holaf_core::config::load_config(&path).unwrap();
        assert_eq!(document["strips"][0]["midi_volume"]["control"], 19);
        assert_eq!(
            document["strips"][0]["midi_volume"]["type"],
            "control_change"
        );

        // A *note* learned for MUTE (the MIDI Mix sends notes for its buttons).
        s.apply(UiAction::MidiLearnTarget {
            uid: uid.clone(),
            target: MidiTarget::Mute,
        });
        s.handle_midi_message(
            crate::midi::MidiMessage::Note {
                channel: 0,
                note: 5,
                velocity: 0, // a release is still a learn: the shape matters
            },
            now + Duration::from_millis(10),
        );
        assert!(s
            .midi_bindings_for(&uid)
            .contains(&(MidiTarget::Mute, Binding::note(0, 5))));
        assert_eq!(s.midi_button_text(&uid).as_deref(), Some("N 5"));
        // "Cancel learn" only ends the learn: nothing is stored.
        s.apply(UiAction::MidiLearnTarget {
            uid: uid.clone(),
            target: MidiTarget::Mono,
        });
        s.apply(UiAction::MidiLearnCancel);
        assert!(s.midi_learning.is_none());
        assert!(s
            .midi_bindings_for(&uid)
            .contains(&(MidiTarget::Mono, Binding::note(0, 22))));
        s.strips = Vec::new();
        s.config.as_mut().unwrap().cancel_save();
    }

    /// The controller's lamps follow the mixer from **any** path, not only from a
    /// MIDI event: a click on MUTE (or an undo) must light the button too.
    #[test]
    fn a_mouse_toggle_lights_the_controller_led() {
        let path = scratch_config("midi-led-mouse");
        let mut s = AppState::from_config(&path);
        let uid = s.strips[0].uid.clone();
        s.take_midi_feedback();
        s.apply(UiAction::Mute {
            uid: uid.clone(),
            mute: true,
        });
        assert_eq!(
            s.take_midi_feedback(),
            vec![crate::midi::MidiMessage::Note {
                channel: 0,
                note: 24,
                velocity: 127
            }]
        );
        s.apply(UiAction::Mono {
            uid: uid.clone(),
            mono: true,
        });
        assert_eq!(
            s.take_midi_feedback(),
            vec![crate::midi::MidiMessage::Note {
                channel: 0,
                note: 22,
                velocity: 127
            }]
        );
        // A strip without bindings queues nothing, and a CC mapping (a knob) has
        // no lamp at all.
        let mix = s
            .strips
            .iter()
            .find(|strip| strip.label == "Mix Out")
            .map(|strip| strip.uid.clone())
            .expect("Mix Out");
        assert!(s.midi_bindings_for(&mix).is_empty());
        s.apply(UiAction::Mute {
            uid: mix,
            mute: true,
        });
        assert!(s.take_midi_feedback().is_empty());
        s.strips = Vec::new();
        s.config.as_mut().unwrap().cancel_save();
    }

    /// Selecting a controller stores it under `ui_rust.midi_port` (a Rust-only
    /// key: Python ignores it), and the choice survives a restart — including the
    /// explicit "no controller" (`null`), which must not be overridden by the
    /// automatic pick.
    #[test]
    fn picking_a_port_persists_it_and_is_reloaded() {
        let path = scratch_config("midi-port");
        let mut s = AppState::from_config(&path);
        let now = Instant::now();
        s.set_clock(now);
        // A fresh config does not name a port: the automatic pick is allowed.
        assert_eq!(s.midi_port_request, None);
        assert!(s.midi_port_auto);
        s.apply(UiAction::MidiPortSelected {
            name: Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string()),
        });
        assert_eq!(
            s.midi_port_request.as_deref(),
            Some("MIDI Mix:MIDI Mix MIDI 1 24:0")
        );
        assert!(!s.midi_port_auto);
        assert!(s.midi_port_picker.is_none(), "the dialog closes");
        s.save_pending(now + crate::config::SAVE_DEBOUNCE);
        let document = holaf_core::config::load_config(&path).unwrap();
        assert_eq!(
            document["ui_rust"]["midi_port"],
            "MIDI Mix:MIDI Mix MIDI 1 24:0"
        );
        // …and the MIDI mappings of the user's config are still there.
        assert_eq!(document["strips"][0]["midi_volume"]["control"], 61);
        assert!(document.get("window_geometry").is_some());

        let mut reloaded = AppState::from_config(&path);
        assert_eq!(
            reloaded.midi_port_request.as_deref(),
            Some("MIDI Mix:MIDI Mix MIDI 1 24:0")
        );
        assert!(!reloaded.midi_port_auto, "a stored port disables auto-pick");
        // "No controller" is a decision, not a default.
        reloaded.apply(UiAction::MidiPortSelected { name: None });
        let reloaded_now = reloaded.clock();
        reloaded.save_pending(reloaded_now + crate::config::SAVE_DEBOUNCE);
        let document = holaf_core::config::load_config(&path).unwrap();
        assert_eq!(document["ui_rust"]["midi_port"], serde_json::Value::Null);
        let mut reloaded = AppState::from_config(&path);
        assert_eq!(reloaded.midi_port_request, None);
        assert!(!reloaded.midi_port_auto, "`null` means never auto-pick again");
        reloaded.strips.clear();
        reloaded.config.as_mut().unwrap().cancel_save();
        s.strips = Vec::new();
        s.config.as_mut().unwrap().cancel_save();
    }

    /// Re-picking the port that is already selected is not a change: no config
    /// write, no reconnect (a needless `close`+`open` would drop the controller
    /// every time the user confirms the dialog) — but the action is still
    /// recorded, so whoever drains `pending_actions` sees it.
    #[test]
    fn reselecting_the_same_port_changes_nothing() {
        let mut s = state();
        s.midi_port_request = Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string());
        s.midi_connected = true;
        s.midi_port_name = Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string());
        let before = s.pending_action_count();
        s.apply(UiAction::MidiPortSelected {
            name: Some(" MIDI Mix:MIDI Mix MIDI 1 24:0 ".to_string()),
        });
        assert_eq!(s.midi_port_request.as_deref(), Some("MIDI Mix:MIDI Mix MIDI 1 24:0"));
        assert!(s.midi_connected, "the connection is not dropped");
        assert_eq!(s.midi_port_name.as_deref(), Some("MIDI Mix:MIDI Mix MIDI 1 24:0"));
        assert!(s.midi_port_picker.is_none(), "the dialog still closes");
        assert_eq!(
            s.pending_action_count(),
            before + 1,
            "the action is recorded even when it is a no-op"
        );
    }

    /// Deleting a strip ends a learn armed on it: the mapping would have nowhere
    /// to go (the config entry disappears with the strip).
    #[test]
    fn deleting_a_strip_cancels_its_armed_learn() {
        let mut s = state();
        let uid = "in-mic".to_string();
        s.apply(UiAction::MidiLearnTarget {
            uid: uid.clone(),
            target: MidiTarget::Mute,
        });
        assert!(s.is_midi_learning(&uid));
        s.apply(UiAction::Delete { uid: uid.clone() });
        assert!(s.midi_learning.is_none());
        assert!(s.midi_flash.is_none());
        // The deleted strip's bindings are gone from the lookup table.
        assert_eq!(s.midi_map.lookup(crate::midi::MidiMessage::Note {
            channel: 0,
            note: 24,
            velocity: 1
        }), None);
    }

    /// The port picker lists what the shell found, and paints headless.
    #[test]
    fn the_port_picker_paints_what_the_shell_found() {
        let mut s = state();
        s.midi_ports = vec![
            "MIDI Mix:MIDI Mix MIDI 1 24:0".to_string(),
            "Midi Through:Midi Through Port-0 14:0".to_string(),
        ];
        s.midi_connected = true;
        s.midi_port_name = Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string());
        s.apply(UiAction::OpenMidiPortPicker);
        let picker = s.midi_port_picker.clone().expect("the dialog opens");
        assert_eq!(picker.selected, None, "nothing selected yet");
        let ctx = crate::test_support::headless_context();
        // An `egui::Window` needs two passes (measure, then paint).
        let _ = crate::test_support::run_pass(&ctx, egui::Vec2::new(900.0, 700.0), 0.0, |ui| {
            crate::dialogs::draw(&mut s, ui)
        });
        let primitives =
            crate::test_support::run_pass(&ctx, egui::Vec2::new(900.0, 700.0), 1.0, |ui| {
                crate::dialogs::draw(&mut s, ui)
            });
        assert!(!primitives.is_empty(), "the picker painted nothing");
        assert!(
            s.midi_port_picker.is_some(),
            "a paint pass must not close the dialog"
        );
        // A build without MIDI explains itself instead of showing a void.
        let mut s = state();
        s.midi_ports_error = Some("this build has no MIDI support".to_string());
        s.apply(UiAction::OpenMidiPortPicker);
        let _ = crate::test_support::run_pass(&ctx, egui::Vec2::new(900.0, 700.0), 2.0, |ui| {
            crate::dialogs::draw(&mut s, ui)
        });
    }

    /// The MIDI button's hover text lists what is bound: a mapping used to be
    /// invisible once learned (the Qt button just said `MIDI`).
    #[test]
    fn the_midi_summary_lists_the_bindings_and_the_controller() {
        let mut s = state();
        s.midi_bindings.insert(
            "in-mic".to_string(),
            vec![
                (MidiTarget::Volume, Binding::control(0, 19)),
                (MidiTarget::Mute, Binding::note(1, 3)),
            ],
        );
        let summary = s.midi_summary("in-mic");
        assert!(summary.contains("CC 19 ch.1 -> volume"), "{summary}");
        assert!(summary.contains("N 3 ch.2 -> mute"), "{summary}");
        assert!(summary.contains("no controller connected"), "{summary}");
        assert!(summary.contains("learn / clear"), "{summary}");
        // A strip with no mapping says so instead of showing an empty tooltip.
        assert!(s.midi_summary("in-web").contains("no MIDI mapping"));
        // The connected controller is named when there is one.
        s.midi_connected = true;
        s.midi_port_name = Some("MIDI Mix:MIDI Mix MIDI 1 24:0".to_string());
        assert!(s
            .midi_summary("in-mic")
            .contains("controller: MIDI Mix:MIDI Mix MIDI 1 24:0"));
    }

    #[test]
    fn a_learned_mapping_is_stored_in_config_and_drives_the_model() {
        let path = scratch_config("midi-learn");
        let mut s = AppState::from_config(&path);
        let uid = s
            .strips
            .iter()
            .find(|strip| strip.label == "Web")
            .map(|strip| strip.uid.clone())
            .expect("the fixture has a Web strip");
        let now = Instant::now();
        s.set_clock(now);
        // A fresh strip has no binding for the volume of that strip yet (the
        // fixture binds other strips).
        s.midi_learn(&uid, MidiTarget::Volume);
        s.handle_midi_message(
            crate::midi::MidiMessage::Control {
                channel: 4,
                control: 21,
                value: 90,
            },
            now,
        );
        // Learn mode ended and the mapping is in the model + the config store.
        assert!(s.midi_learning.is_none());
        assert_eq!(
            s.midi_bindings
                .get(&uid)
                .unwrap()
                .iter()
                .find(|(target, _)| *target == MidiTarget::Volume)
                .map(|(_, binding)| *binding),
            Some(Binding::control(4, 21))
        );
        s.save_pending(now + crate::config::SAVE_DEBOUNCE);
        let document = holaf_core::config::load_config(&path).unwrap();
        let entry = document["strips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["uid"].as_str() == Some(uid.as_str()))
            .unwrap();
        assert_eq!(entry["midi_volume"]["control"], 21);
        assert_eq!(entry["midi_volume"]["channel"], 4);

        // The mapping now drives the fader: `value / 127`, and the knob burst is
        // throttled so the model does not follow 400 messages per second.
        let before = s.strips[s.strip_index(&uid).unwrap()].volume;
        assert!(before != 0.5);
        s.handle_midi_message(
            crate::midi::MidiMessage::Control {
                channel: 4,
                control: 21,
                value: 64,
            },
            now + Duration::from_millis(100),
        );
        let volume = s.strips[s.strip_index(&uid).unwrap()].volume;
        assert!((volume - 64.0 / 127.0).abs() < 1e-6, "{volume}");
        // A different channel is somebody else's controller.
        s.handle_midi_message(
            crate::midi::MidiMessage::Control {
                channel: 5,
                control: 21,
                value: 127,
            },
            now + Duration::from_millis(200),
        );
        assert!((s.strips[s.strip_index(&uid).unwrap()].volume - volume).abs() < 1e-6);
        s.strips = Vec::new();
        s.config.as_mut().unwrap().cancel_save();
    }

    #[test]
    fn a_midi_button_toggles_and_lights_its_led() {
        let path = scratch_config("midi-button");
        let mut s = AppState::from_config(&path);
        let uid = s
            .strips
            .iter()
            .find(|strip| strip.label == "Web")
            .map(|strip| strip.uid.clone())
            .expect("the fixture has a Web strip");
        let now = Instant::now();
        s.set_clock(now);
        // Bind CC 30 to MONO on that strip, as a learn would.
        s.midi_learn(&uid, MidiTarget::Mono);
        s.handle_midi_message(
            crate::midi::MidiMessage::Note {
                channel: 0,
                note: 30,
                velocity: 100,
            },
            now,
        );
        let bindings = s.midi_bindings.get(&uid).cloned().unwrap();
        assert!(bindings.contains(&(MidiTarget::Mono, Binding::note(0, 30))));
        // The learn itself queued the LED sync of the *current* state.
        let feedback = s.take_midi_feedback();
        assert_eq!(
            feedback,
            vec![crate::midi::MidiMessage::Note {
                channel: 0,
                note: 30,
                velocity: 0
            }]
        );
        let before = s.strips[s.strip_index(&uid).unwrap()].is_mono;
        s.handle_midi_message(
            crate::midi::MidiMessage::Note {
                channel: 0,
                note: 30,
                velocity: 127,
            },
            now + Duration::from_millis(20),
        );
        assert_eq!(s.strips[s.strip_index(&uid).unwrap()].is_mono, !before);
        // …the press queued the LED, the release does nothing at all.
        let feedback = s.take_midi_feedback();
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback[0].value(), 127);
        s.handle_midi_message(
            crate::midi::MidiMessage::Note {
                channel: 0,
                note: 30,
                velocity: 0,
            },
            now + Duration::from_millis(40),
        );
        assert!(s.take_midi_feedback().is_empty());
        s.strips = Vec::new();
        s.config.as_mut().unwrap().cancel_save();
    }

    #[test]
    fn clearing_the_mappings_removes_them_from_the_config() {
        let path = scratch_config("midi-clear");
        let mut s = AppState::from_config(&path);
        let uid = s.strips[0].uid.clone();
        assert!(s.midi_bindings.get(&uid).is_some_and(|b| b.len() == 3));
        let entries_before = s.midi_map.len();
        s.clear_midi_bindings(&uid, Instant::now());
        assert!(!s.midi_bindings.contains_key(&uid));
        // The lookup table followed: that strip's three keys are gone, the
        // other strips keep theirs.
        assert_eq!(s.midi_map.len(), entries_before - 3);
        let now = Instant::now();
        s.set_clock(now);
        s.save_pending(now + crate::config::SAVE_DEBOUNCE);
        let document = holaf_core::config::load_config(&path).unwrap();
        assert_eq!(
            document["strips"][0]["midi_volume"],
            serde_json::Value::Null
        );
        s.strips = Vec::new();
        s.config.as_mut().unwrap().cancel_save();
    }

    // --- step 7: device identity --------------------------------------------

    fn device(name: &str, description: Option<&str>, media_class: &str) -> DeviceInfo {
        DeviceInfo {
            id: 7,
            name: name.to_string(),
            media_class: media_class.to_string(),
            description: description.map(str::to_string),
        }
    }

    #[test]
    fn a_renamed_device_is_re_bound_by_description_and_persisted() {
        let path = scratch_config("device-rename");
        let mut s = AppState::from_config(&path);
        s.set_clock(Instant::now());
        // The fixture's Casque strip points at the built-in analog output; the
        // machine rebooted and the PCI address changed.
        let casque = s
            .strips
            .iter()
            .position(|strip| strip.label == "Casque")
            .expect("the fixture has a Casque strip");
        let declared = s.strips[casque].device_name.clone().unwrap();
        // The card was re-plugged: same model, new USB serial, and the built-in
        // analog output is present too (it must NOT win: different profile).
        let devices = vec![
            device(
                "alsa_output.usb-PreSonus_Studio_26c_99999999-00.analog-surround-40",
                Some("PreSonus Studio 26c Analog Surround 4.0"),
                "Audio/Sink",
            ),
            device(
                "alsa_output.pci-0000_00_1f.3.analog-stereo",
                Some("Built-in Audio Analog Stereo"),
                "Audio/Sink",
            ),
            device(
                "alsa_input.usb-PreSonus_Studio_26c_01424548-00.analog-stereo",
                None,
                "Audio/Source",
            ),
        ];
        s.engine = EngineState::Connected;
        s.engine_devices = devices;
        s.reconcile_devices();
        // The name was rewritten to the device that is really there…
        assert_eq!(
            s.strips[casque].device_name.as_deref(),
            Some("alsa_output.usb-PreSonus_Studio_26c_99999999-00.analog-surround-40"),
            "declared was {declared}"
        );
        assert!(!s.missing_devices.contains(&s.strips[casque].uid));
        assert!(s
            .status
            .as_deref()
            .is_some_and(|status| status.contains("re-bound")));
        // …scheduled the debounced graph rebuild and a config write.
        assert!(s.device_restart.is_pending());
        assert!(s.config.as_ref().unwrap().save_pending());
        // A second pass is a no-op (idempotent: no oscillation between names).
        s.reconcile_devices();
        assert_eq!(
            s.strips[casque].device_name.as_deref(),
            Some("alsa_output.usb-PreSonus_Studio_26c_99999999-00.analog-surround-40")
        );
        s.strips = Vec::new();
        s.config.as_mut().unwrap().cancel_save();
    }

    #[test]
    fn an_absent_device_is_flagged_not_found_but_kept() {
        let mut s = state();
        let hp = s.strip_index("out-hp").unwrap();
        let declared = s.strips[hp].device_name.clone().unwrap();
        s.engine_devices = vec![device(
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            None,
            "Audio/Sink",
        )];
        s.reconcile_devices();
        // The strip keeps its (unplugged) device — unbinding it silently would
        // lose the user's choice when the headset comes back.
        assert_eq!(s.strips[hp].device_name.as_deref(), Some(declared.as_str()));
        assert!(s.missing_devices.contains(&s.strips[hp].uid));
        assert_eq!(
            s.device_presence(&s.strips[hp].uid.clone()),
            crate::devices::DevicePresence::Missing
        );
        // Without an authoritative list nothing is claimed missing.
        s.engine_devices.clear();
        s.reconcile_devices();
        assert!(s.missing_devices.is_empty());
        assert_eq!(
            s.device_presence("out-hp"),
            crate::devices::DevicePresence::Unknown
        );
    }
}
