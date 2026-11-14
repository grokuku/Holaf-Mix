//! `config.json` persistence for the Rust UI.
//!
//! The UI is **not** the owner of the config format: `holaf_core::config` does
//! the I/O (atomic replace), `holaf_core::migration::strip_from_dict` does the
//! migrations (old boolean FX entries, short LADSPA port names, …), exactly the
//! way `holafd` does it.  This module adds the three things only a UI needs:
//!
//! 1. **Unknown keys survive a save.**  The Python config carries UI-only data
//!    (`mode`, `assigned_apps`, `midi_volume`/`midi_mute`/`midi_mono`, the Qt
//!    `window_geometry` blob, `window_state`, …) which `holaf_core::model::Strip`
//!    does not model.  Every save therefore starts from the **document that was
//!    loaded**: the modeled keys (`label`, `volume`, `routes`, `effects`, …) are
//!    patched per strip (matched by `uid`), everything else is copied through
//!    untouched.  Silently dropping the user's MIDI mappings on the first
//!    volume move would be a data-loss bug, not a simplification.
//!
//!    `effects` follows the same rule *inside* the object: the five modeled
//!    effects are patched key by key, so an effect this crate does not know
//!    (`reverb`, `limiter`, … a chain added on the Python side) is never dropped.
//! 2. **Debounced writes.**  A fader drag produces up to 25 `Volume` actions per
//!    second; the config must be written once the gesture settles, not 25 times.
//!    [`SaveDebounce`] is the pure scheduling rule (trailing edge, injectable
//!    `Instant` so tests do not sleep).
//! 3. **A geometry that the Rust UI can read.**  Qt stores an opaque
//!    `saveGeometry()` hex blob under `window_geometry`; the Rust port reads the
//!    plain `window` object (`{x, y, height, …}`) the Python tree already keeps
//!    and only takes the **height** from it (the width is derived from the strip
//!    count — see [`crate::layout`]).  The hex blob is preserved verbatim.
//!
//! Nothing here panics on bad data: a missing file yields an empty document
//! ([`UiConfig::open`] reports `loaded == false`), and a corrupt file is an
//! error the caller turns into a visible warning (`AppState::from_config`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use holaf_core::config::{self, ConfigError};
use holaf_core::migration;
use holaf_core::model::{Strip, StripType};

/// How long a burst of edits is coalesced before one atomic write
/// (`DEBOUNCE_ENGINE_RESTART_MS`-style trailing debounce, 200 ms in Qt for the
/// engine restart; config writes are slower, so 400 ms).
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

/// The window size the UI remembers from one run to the next.
///
/// Only the height is authoritative: the width is locked to
/// `Σ section_width(strip count)` by [`crate::layout`], so a stored width is
/// informational (it is preserved for the Qt UI, which restores its own
/// geometry blob anyway).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowGeometry {
    pub width: Option<f32>,
    pub height: f32,
}

impl WindowGeometry {
    /// Read `{"height": 423, "width": 800, …}` (the shape `config.json` uses
    /// under `window`).  A missing/invalid height yields `None` — a geometry we
    /// cannot trust is worse than the theme's default.
    ///
    /// `height` is the Rust key.  The legacy Qt object (`window_state`) spells the
    /// same idea `h`, and it is only read as a **fallback** when no plain
    /// `height` is available: the two are not the same measurement (the Qt value
    /// is the frame height, decorations included — 595 for a 575 px content
    /// height here), and `window.height` is the one this UI writes, so it wins
    /// whenever it exists.
    pub fn from_json(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let height = obj
            .get("height")
            .and_then(Value::as_f64)
            .or_else(|| obj.get("h").and_then(Value::as_f64))? as f32;
        if !height.is_finite() || height < 1.0 {
            return None;
        }
        let width = obj
            .get("width")
            .and_then(Value::as_f64)
            .map(|w| w as f32)
            .filter(|w| w.is_finite() && *w >= 1.0);
        Some(Self { width, height })
    }

    /// Write this geometry into an existing `window` object, keeping its other
    /// keys (`x`, `y`, `maximized`, …).
    pub fn patch(&self, window: &mut Map<String, Value>) {
        window.insert("height".to_string(), Value::from(self.height));
        if let Some(width) = self.width {
            window.insert("width".to_string(), Value::from(width));
        }
    }
}

/// Trailing-edge debounce shared by every "write once the user stops" path of
/// the UI (config saves, FX parameter pushes).
///
/// Pure: the caller supplies `now`, so the tests never sleep and the behaviour
/// is identical on a loaded machine.
#[derive(Debug, Clone, Copy)]
pub struct SaveDebounce {
    interval: Duration,
    deadline: Option<Instant>,
}

impl SaveDebounce {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            deadline: None,
        }
    }

    /// Record an edit: the write happens `interval` after the **last** one.
    pub fn request(&mut self, now: Instant) {
        self.deadline = Some(now + self.interval);
    }

    /// Is a write scheduled at all?
    pub fn is_pending(&self) -> bool {
        self.deadline.is_some()
    }

    /// When the scheduled write is due (for `request_repaint_after`).
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Consume the scheduled write when it is due: `true` exactly once.
    pub fn take_due(&mut self, now: Instant) -> bool {
        match self.deadline {
            Some(deadline) if now >= deadline => {
                self.deadline = None;
                true
            }
            _ => false,
        }
    }

    /// Drop the schedule without writing (the caller saved explicitly).
    pub fn cancel(&mut self) {
        self.deadline = None;
    }
}

/// The UI's view of `config.json`: the loaded document plus the debounced
/// writer.
pub struct UiConfig {
    path: PathBuf,
    /// The whole document as loaded.  Kept so a save never loses a key this
    /// crate does not model.
    raw: Value,
    /// Per-strip raw objects, keyed by **the uid the UI uses** for that entry
    /// (see [`repair_uids`]), for the same reason at strip level (`mode`,
    /// `assigned_apps`, `midi_*`).
    extras: HashMap<String, Map<String, Value>>,
    /// The uid of every object entry of `strips[]`, in file order, so
    /// [`Self::strips`] and `extras` can never disagree — including for a legacy
    /// entry that has no `uid` in the file (it must keep its extras too).
    uids: Vec<String>,
    geometry: Option<WindowGeometry>,
    theme: Option<String>,
    loaded: bool,
    debounce: SaveDebounce,
}

impl std::fmt::Debug for UiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiConfig")
            .field("path", &self.path)
            .field("loaded", &self.loaded)
            .field("strips", &self.extras.len())
            .field("geometry", &self.geometry)
            .field("pending_save", &self.debounce.is_pending())
            .finish()
    }
}

impl UiConfig {
    /// Open `path`.  A **missing** file is not an error (first run): the store
    /// starts from an empty document and [`Self::loaded`] reports `false`.  A
    /// corrupt file *is* an error — the caller must show it instead of silently
    /// overwriting the user's config.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        let loaded = path.is_file();
        let raw = if loaded {
            config::load_config(&path)?
        } else {
            Value::Object(Map::new())
        };
        // A config that parses but is not a JSON object (a bare array, `null`)
        // has no keys to preserve; treat it as empty rather than failing.
        let raw = match raw {
            Value::Object(map) => Value::Object(map),
            _ => Value::Object(Map::new()),
        };
        let mut store = Self {
            path,
            raw,
            extras: HashMap::new(),
            uids: Vec::new(),
            geometry: None,
            theme: None,
            loaded,
            debounce: SaveDebounce::new(SAVE_DEBOUNCE),
        };
        store.reindex();
        Ok(store)
    }

    /// Re-read the derived views (per-strip extras, geometry, theme) from
    /// `raw`.  Called after a load and after every save.
    ///
    /// `extras` is keyed by the uid the **UI** will use for that entry, not by the
    /// raw one: a legacy entry without `uid` used to fall through this loop and
    /// then lose `mode` / `assigned_apps` / `midi_*` on the first save.
    fn reindex(&mut self) {
        self.extras.clear();
        let entries: Vec<Value> = self
            .raw
            .get("strips")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.uids = repair_uids(&entries);
        let objects = entries.iter().filter_map(Value::as_object);
        for (uid, obj) in self.uids.iter().zip(objects) {
            let mut obj = obj.clone();
            normalise_legacy_midi(&mut obj);
            self.extras.insert(uid.clone(), obj);
        }
        self.geometry = ["window", "window_geometry", "window_state"]
            .iter()
            .filter_map(|key| self.raw.get(*key))
            .find_map(WindowGeometry::from_json);
        self.theme = self
            .raw
            .get("theme")
            .and_then(Value::as_str)
            .map(str::to_string);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Did the file exist when we opened it?  `false` = first run (empty
    /// document), which is what makes `AppState::from_config` fall back to an
    /// empty mixer **without** pushing an empty graph to a running daemon.
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    pub fn geometry(&self) -> Option<WindowGeometry> {
        self.geometry
    }

    /// Did the loaded document carry a `window.width`?
    ///
    /// The UI locks the width to the strip count
    /// ([`crate::layout::enforce_window_width`]), so the width is never the
    /// user's choice: a save refreshes an **existing** width and never adds the
    /// key to a document that did not have it (the probe of the shipped config
    /// shows the Qt file stores no width at all).
    pub fn has_stored_width(&self) -> bool {
        self.raw
            .get("window")
            .and_then(Value::as_object)
            .is_some_and(|window| window.contains_key("width"))
    }

    /// `"theme"` key of the config, if any (a name or a path).
    pub fn theme(&self) -> Option<&str> {
        self.theme.as_deref()
    }

    /// The strips of the loaded document, migrated exactly like the daemon
    /// does, with uids repaired (a legacy entry without `uid` is given a
    /// deterministic one) and de-duplicated (the UI is keyed by uid, so two
    /// strips sharing one would make clicks land on the wrong one).
    pub fn strips(&self) -> Vec<Strip> {
        let entries = self
            .raw
            .get("strips")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut strips = Vec::with_capacity(entries.len());
        let mut object_index = 0usize;
        for (index, entry) in entries.iter().enumerate() {
            if !entry.is_object() {
                log::warn!("config: skipping strip #{index} (not an object)");
                continue;
            }
            let mut strip = migration::strip_from_dict(entry);
            // The repaired uid computed by `reindex` (it is what `extras` is
            // keyed by).  Recomputed only if this store was built without one.
            strip.uid = match self.uids.get(object_index) {
                Some(uid) => uid.clone(),
                None => repair_uids(&entries)
                    .into_iter()
                    .nth(object_index)
                    .unwrap_or_else(|| legacy_uid(index, strip.kind)),
            };
            object_index += 1;
            strips.push(strip);
        }
        strips
    }

    /// `assigned_apps` of every strip that has some, keyed by uid.
    ///
    /// `assigned_apps` is a **UI-only** key: the daemon never reads it and
    /// `holaf_core::model::Strip` does not model it (adding it there would put a
    /// Python-UI detail in the shared model).  It lives in `config.json`, which
    /// this store already keeps verbatim, so the Rust UI reads and writes it
    /// here — byte-compatible with the PySide UI, which does the same.
    pub fn assigned_apps(&self) -> HashMap<String, Vec<String>> {
        let mut map = HashMap::new();
        for (uid, entry) in &self.extras {
            let names: Vec<String> = entry
                .get("assigned_apps")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if !names.is_empty() {
                map.insert(uid.clone(), names);
            }
        }
        map
    }

    /// Store one strip's `assigned_apps` (empty = remove the entries).
    ///
    /// The value goes into the per-strip extra object, which
    /// [`Self::document`] copies through: the next save writes it, and the keys
    /// this crate does not model are still preserved.
    pub fn set_assigned_apps(&mut self, uid: &str, names: &[String]) {
        let entry = self.extras.entry(uid.to_string()).or_default();
        let array: Vec<Value> = names
            .iter()
            .map(|name| Value::String(name.clone()))
            .collect();
        entry.insert("assigned_apps".to_string(), Value::Array(array));
    }

    /// Every per-strip MIDI binding (`midi_volume` / `midi_mute` / `midi_mono`),
    /// keyed by uid.  Same UI-only-key rule as [`Self::assigned_apps`]: the
    /// daemon never reads MIDI, `Strip` does not model it, `config.json` does.
    pub fn midi_bindings(
        &self,
    ) -> HashMap<String, Vec<(crate::midi::MidiTarget, crate::midi::Binding)>> {
        let mut map: HashMap<String, Vec<(crate::midi::MidiTarget, crate::midi::Binding)>> =
            HashMap::new();
        for (uid, entry) in &self.extras {
            let mut bindings = Vec::new();
            for target in crate::midi::MidiTarget::ALL {
                let Some(value) = entry.get(target.config_key()) else {
                    continue;
                };
                // `null` is the Python shape for "not mapped" (Mix Out has all
                // three): skipping it silently keeps the log for real garbage.
                if value.is_null() {
                    continue;
                }
                match crate::midi::Binding::from_json(value) {
                    Some(binding) => bindings.push((target, binding)),
                    // A bare number (`"midi_volume": 61`) is not a shape either UI
                    // writes, but reading it beats dropping the user's mapping
                    // (the Qt side would crash on it).  Assumes channel 0 and the
                    // target's natural message type; still written back as an
                    // object, i.e. in the shape Python reads.
                    None => match crate::midi::Binding::from_legacy_number(target, value) {
                        Some(binding) => {
                            log::info!(
                                "config: strip {uid:?} has a bare-number {} mapping, read as {}",
                                target.config_key(),
                                binding.describe()
                            );
                            bindings.push((target, binding));
                        }
                        None => log::warn!(
                            "config: strip {uid:?} has an unreadable {} mapping, ignored",
                            target.config_key()
                        ),
                    },
                }
            }
            if !bindings.is_empty() {
                map.insert(uid.clone(), bindings);
            }
        }
        map
    }

    /// Store (or clear) one strip's MIDI mapping for `target`.
    pub fn set_midi_binding(
        &mut self,
        uid: &str,
        target: crate::midi::MidiTarget,
        binding: Option<crate::midi::Binding>,
    ) {
        let entry = self.extras.entry(uid.to_string()).or_default();
        let key = target.config_key().to_string();
        match binding {
            Some(binding) => {
                entry.insert(key, binding.to_json());
            }
            None => {
                entry.insert(key, Value::Null);
            }
        }
    }

    /// A Rust-only UI setting (`ui_rust.<key>`), `None` when absent.
    ///
    /// The Rust UI needs a few preferences the Qt UI has no concept of (the
    /// always-on-top flag, the tray behaviour, the MIDI port name).  They live
    /// under their own `ui_rust` object so a Python build reading the same file
    /// is unaffected — and so the Qt keys (`window_geometry`, `window_state`) are
    /// never touched.  Python's `load_config`/`save_config` copy the whole
    /// document, so `ui_rust` survives a round trip through the Qt UI.
    pub fn ui_rust_bool(&self, key: &str) -> Option<bool> {
        self.raw
            .get("ui_rust")
            .and_then(|value| value.get(key))
            .and_then(Value::as_bool)
    }

    /// Write one Rust-only UI setting (in memory; the next save persists it).
    pub fn set_ui_rust_bool(&mut self, key: &str, value: bool) {
        self.ui_rust_mut()
            .insert(key.to_string(), Value::Bool(value));
    }

    /// Does the `ui_rust` object declare `key` at all?
    ///
    /// The difference matters for `midi_port`: *absent* means "never chosen" (the
    /// shell may auto-select the controller it recognises), while *present and
    /// `null`* is an explicit "no controller, thanks" that must survive a restart.
    pub fn ui_rust_has(&self, key: &str) -> bool {
        self.raw
            .get("ui_rust")
            .and_then(|value| value.as_object())
            .is_some_and(|object| object.contains_key(key))
    }

    /// A Rust-only **string** setting (`ui_rust.<key>`), `None` when absent,
    /// empty, or not a string.  Used for `midi_port` (the controller name).
    pub fn ui_rust_string(&self, key: &str) -> Option<String> {
        self.raw
            .get("ui_rust")
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    /// Store (or forget, with `None`) one Rust-only string setting.
    pub fn set_ui_rust_string(&mut self, key: &str, value: Option<&str>) {
        let trimmed = value.map(str::trim).filter(|value| !value.is_empty());
        let entry = self.ui_rust_mut();
        match trimmed {
            Some(value) => {
                entry.insert(key.to_string(), Value::String(value.to_string()));
            }
            None => {
                entry.insert(key.to_string(), Value::Null);
            }
        }
    }

    /// The `ui_rust` object, created on demand (a non-object value is replaced:
    /// a hand-written `"ui_rust": 1` must not swallow the setting).
    fn ui_rust_mut(&mut self) -> &mut Map<String, Value> {
        let object = self
            .raw
            .as_object_mut()
            .expect("the document is always normalised to an object");
        let entry = object
            .entry("ui_rust".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        entry
            .as_object_mut()
            .expect("just ensured it is an object")
    }

    /// Drop a scheduled write without writing (a test, or a caller that just
    /// saved explicitly).
    pub fn cancel_save(&mut self) {
        self.debounce.cancel();
    }

    /// Schedule one debounced write (the caller still has to provide the strips
    /// at save time: [`Self::save_if_due`]).
    pub fn request_save(&mut self, now: Instant) {
        self.debounce.request(now);
    }

    pub fn save_pending(&self) -> bool {
        self.debounce.is_pending()
    }

    pub fn save_deadline(&self) -> Option<Instant> {
        self.debounce.deadline()
    }

    /// Write when the debounce elapsed.  `None` = nothing was due.
    pub fn save_if_due(
        &mut self,
        now: Instant,
        strips: &[Strip],
        geometry: Option<WindowGeometry>,
    ) -> Option<Result<(), ConfigError>> {
        if !self.debounce.take_due(now) {
            return None;
        }
        Some(self.save_now(strips, geometry))
    }

    /// Write immediately (window close, explicit save).
    pub fn save_now(
        &mut self,
        strips: &[Strip],
        geometry: Option<WindowGeometry>,
    ) -> Result<(), ConfigError> {
        let document = self.document(strips, geometry);
        config::save_config_atomic(&self.path, &document)?;
        self.raw = document;
        self.reindex();
        self.debounce.cancel();
        Ok(())
    }

    /// The document that *would* be written — pure, so the preservation rules
    /// are unit-tested without touching the filesystem.
    pub fn document(&self, strips: &[Strip], geometry: Option<WindowGeometry>) -> Value {
        let mut document = self.raw.clone();
        if !document.is_object() {
            document = Value::Object(Map::new());
        }
        let array: Vec<Value> = strips
            .iter()
            .map(|strip| Value::Object(self.strip_object(strip)))
            .collect();
        if let Some(obj) = document.as_object_mut() {
            obj.insert("strips".to_string(), Value::Array(array));
            if let Some(geometry) = geometry {
                let window = obj
                    .entry("window".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if !window.is_object() {
                    *window = Value::Object(Map::new());
                }
                geometry.patch(
                    window
                        .as_object_mut()
                        .expect("just ensured it is an object"),
                );
            }
        }
        document
    }

    /// One strip as a config.json entry: the loaded unknown keys plus the
    /// modeled ones, in the key order `Strip.to_dict` uses.
    fn strip_object(&self, strip: &Strip) -> Map<String, Value> {
        let mut obj = self.extras.get(&strip.uid).cloned().unwrap_or_default();
        // Computed **before** `device_name` is overwritten: it is the loaded
        // binding that decides whether `mode` still belongs to the file.
        let mode = mode_for(&obj, strip);
        obj.insert("uid".to_string(), Value::String(strip.uid.clone()));
        obj.insert("label".to_string(), Value::String(strip.label.clone()));
        obj.insert(
            "kind".to_string(),
            Value::String(strip.kind.as_str().to_string()),
        );
        obj.insert("mode".to_string(), Value::String(mode));
        obj.insert("volume".to_string(), Value::from(strip.volume));
        obj.insert("mute".to_string(), Value::Bool(strip.mute));
        obj.insert("is_mono".to_string(), Value::Bool(strip.is_mono));
        obj.insert(
            "routes".to_string(),
            Value::Array(
                strip
                    .routes
                    .iter()
                    .map(|r| Value::String(r.clone()))
                    .collect(),
            ),
        );
        obj.insert(
            "device_name".to_string(),
            match &strip.device_name {
                Some(name) => Value::String(name.clone()),
                None => Value::Null,
            },
        );
        obj.insert("is_default".to_string(), Value::Bool(strip.is_default));
        // Keys the Python `to_dict` always writes: add them when they were
        // absent, never overwrite a value we do not model.
        obj.entry("assigned_apps".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        obj.entry("midi_volume".to_string()).or_insert(Value::Null);
        obj.entry("midi_mute".to_string()).or_insert(Value::Null);
        obj.entry("midi_mono".to_string()).or_insert(Value::Null);
        patch_effects(&mut obj, strip);
        obj
    }
}

/// Merge the modeled effects into a strip object's `effects`, key by key.
///
/// `effects` is the one block a save could lose wholesale: rebuilding it from
/// `strip.effects` (five keys) would silently delete any effect this crate does
/// not model.  Known keys keep their position in the file, unknown keys are left
/// alone, and a missing known key is appended in [`EffectKey::all`] order (what
/// Python's `to_dict` writes).
fn patch_effects(obj: &mut Map<String, Value>, strip: &Strip) {
    let entry = obj
        .entry("effects".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    let effects = entry
        .as_object_mut()
        .expect("just ensured the effects value is an object");
    for (key, state) in &strip.effects {
        match serde_json::to_value(state) {
            Ok(value) => {
                effects.insert(key.as_str().to_string(), value);
            }
            Err(err) => log::warn!(
                "config: cannot serialise effect {:?} of {:?}: {err}",
                key.as_str(),
                strip.label
            ),
        }
    }
}

/// The `mode` to write for a strip: the file's value unless the device binding
/// changed.
///
/// Both UIs derive the mode from the device (`on_strip_device_changed` sets
/// PHYSICAL when a device is picked, VIRTUAL when it is cleared), but deriving it
/// on **every** save would rewrite a hand-written `"mode": "physical"` with
/// `"device_name": null` into `virtual` — a silent data change nobody asked for.
/// So the loaded binding decides: same device as in the file (both `None`
/// included) means the mode is not ours to touch.
fn mode_for(obj: &Map<String, Value>, strip: &Strip) -> String {
    let declared_device = obj.get("device_name").and_then(Value::as_str);
    if declared_device == strip.device_name.as_deref() {
        if let Some(mode) = obj.get("mode").and_then(Value::as_str) {
            if !mode.is_empty() {
                return mode.to_string();
            }
        }
    }
    if strip.device_name.is_some() {
        "physical".to_string()
    } else {
        "virtual".to_string()
    }
}

/// Rewrite a **bare-number** MIDI mapping (`"midi_volume": 61`) into the object
/// shape both UIs read.
///
/// Nothing writes the bare number today (Python's `Strip.to_dict` writes
/// `{type, channel, control|note}` and so does [`crate::midi::Binding::to_json`]),
/// but a hand-edited config can hold one — and the Qt side *crashes* on it
/// (`TypeError: 'int' object is not subscriptable`, in `_rebuild_midi_lookup`).
/// Repairing it at load time means the first save of the Rust UI leaves a file
/// the Python UI can open again, and it is exactly the shape
/// [`UiConfig::midi_bindings`] reads.
fn normalise_legacy_midi(obj: &mut Map<String, Value>) {
    for target in crate::midi::MidiTarget::ALL {
        let key = target.config_key();
        let Some(value) = obj.get(key) else { continue };
        if value.is_null() || value.is_object() {
            continue;
        }
        match crate::midi::Binding::from_legacy_number(target, value) {
            Some(binding) => {
                log::info!(
                    "config: {key} holds a bare number ({value}), rewritten as {}",
                    binding.describe()
                );
                obj.insert(key.to_string(), binding.to_json());
            }
            None => log::warn!("config: {key} holds {value}, which is not a mapping - kept as is"),
        }
    }
}

/// The uid the UI uses for each strip object of `strips[]`, in file order.
///
/// An entry without a `uid` (legacy config) gets a deterministic one and a
/// duplicate is suffixed, so that two strips never share a uid (the UI is keyed
/// by uid: a click would land on the wrong strip).  Positional and pure, so
/// [`UiConfig::strips`] and [`UiConfig::reindex`] always compute the same ids —
/// which is what lets a uid-less entry keep its extras.
fn repair_uids(entries: &[Value]) -> Vec<String> {
    let mut uids: Vec<String> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let declared = obj
            .get("uid")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let mut uid = if declared.is_empty() {
            let kind = match obj.get("kind").and_then(Value::as_str) {
                Some("input") => StripType::Input,
                _ => StripType::Output,
            };
            let uid = legacy_uid(index, kind);
            log::info!(
                "config: strip #{index} ({:?}) has no uid, assigned {uid:?}",
                obj.get("label").and_then(Value::as_str).unwrap_or("")
            );
            uid
        } else {
            declared.to_string()
        };
        if uids.contains(&uid) {
            let base = uid;
            let mut n = 2;
            while uids.contains(&format!("{base}-{n}")) {
                n += 1;
            }
            uid = format!("{base}-{n}");
            log::warn!("config: duplicate strip uid {base:?}, renamed to {uid:?}");
        }
        uids.push(uid);
    }
    uids
}

/// Deterministic uid for a legacy strip entry that has none.
fn legacy_uid(index: usize, kind: StripType) -> String {
    let prefix = match kind {
        StripType::Input => "in",
        StripType::Output => "out",
    };
    format!("{prefix}-legacy-{}", index + 1)
}

/// Load a whole UI state from `path` in one call: strips + geometry + theme
/// name.  Convenience for callers that do not need the store itself.
pub fn load(path: impl Into<PathBuf>) -> Result<UiConfig, ConfigError> {
    UiConfig::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holaf_core::model::EffectKey;
    use serde_json::json;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "holaf-ui-config-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The repository's own `config.json` — a realistic fixture with the
    /// UI-only keys (`mode`, `assigned_apps`, `midi_*`) the Rust model ignores.
    fn repo_config_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../config.json")
            .to_path_buf()
    }

    #[test]
    fn debounce_is_trailing_edge_and_fires_once() {
        let now = Instant::now();
        let mut d = SaveDebounce::new(Duration::from_millis(400));
        assert!(!d.is_pending());
        d.request(now);
        d.request(now + Duration::from_millis(300));
        assert!(
            !d.take_due(now + Duration::from_millis(600)),
            "still debouncing"
        );
        assert!(d.take_due(now + Duration::from_millis(700)));
        assert!(!d.take_due(now + Duration::from_millis(800)), "fires once");
        assert!(!d.is_pending());
    }

    #[test]
    fn debounce_can_be_cancelled() {
        let now = Instant::now();
        let mut d = SaveDebounce::new(Duration::from_millis(10));
        d.request(now);
        d.cancel();
        assert!(!d.is_pending());
        assert!(!d.take_due(now + Duration::from_secs(1)));
    }

    #[test]
    fn missing_file_is_an_empty_document_not_an_error() {
        let store = UiConfig::open(scratch("missing").join("config.json")).unwrap();
        assert!(!store.loaded());
        assert!(store.strips().is_empty());
        assert_eq!(store.geometry(), None);
        assert_eq!(store.theme(), None);
    }

    #[test]
    fn corrupt_file_is_reported() {
        let path = scratch("corrupt").join("config.json");
        std::fs::write(&path, "{ not json !!!").unwrap();
        let err = UiConfig::open(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Json(_)), "{err:?}");
        // …and the file is left untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json !!!");
    }

    #[test]
    fn the_repo_config_loads_with_its_ui_only_fields() {
        let store = UiConfig::open(repo_config_path()).unwrap();
        assert!(store.loaded());
        let strips = store.strips();
        assert_eq!(strips.len(), 7, "the shipped config has 7 strips");
        assert_eq!(
            strips.iter().filter(|s| s.kind == StripType::Input).count(),
            4
        );
        // The migration ran: the compressor holds the full LADSPA port set.
        let casque = strips.iter().find(|s| s.label == "Casque").unwrap();
        assert!(casque
            .effects
            .get(&EffectKey::Eq)
            .is_some_and(|eq| eq.params.contains_key("50Hz gain (low shelving)")));
        // The geometry comes from the `window` object (height only).
        let geometry = store.geometry().expect("window geometry");
        assert!(geometry.height > 100.0);
        // No theme key in this config.
        assert_eq!(store.theme(), None);
    }

    #[test]
    fn a_legacy_config_is_migrated() {
        let dir = scratch("legacy");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "strips": [
                    // No uid, boolean effects, short EQ port names.
                    {"label": "Old mic", "kind": "input",
                     "effects": {"gate": true, "eq": {"active": true,
                        "params": {"50Hz": -3.0, "100Hz": 2.0}}}},
                    // Duplicate uid: must be repaired, not merged.
                    {"uid": "u1", "label": "A", "kind": "output"},
                    {"uid": "u1", "label": "B", "kind": "output"},
                    // Not an object: skipped with a log, never a panic.
                    "garbage"
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let store = UiConfig::open(&path).unwrap();
        let strips = store.strips();
        assert_eq!(strips.len(), 3);
        assert_eq!(strips[0].uid, "in-legacy-1", "a missing uid is repaired");
        // Boolean FX entry → active with default params, short names renamed.
        assert!(strips[0].effects[&EffectKey::Gate].active);
        let eq = &strips[0].effects[&EffectKey::Eq];
        assert!(eq.active);
        assert_eq!(eq.params["50Hz gain (low shelving)"], -3.0);
        assert_eq!(eq.params["100Hz gain"], 2.0);
        // Duplicate uids stay distinct.
        assert_eq!(strips[1].uid, "u1");
        assert_eq!(strips[2].uid, "u1-2");
    }

    #[test]
    fn saving_preserves_unknown_keys_at_document_and_strip_level() {
        let dir = scratch("preserve");
        let path = dir.join("config.json");
        std::fs::copy(repo_config_path(), &path).unwrap();
        let mut store = UiConfig::open(&path).unwrap();
        let mut strips = store.strips();

        // Modeled change: a volume + a label, a new route.
        strips[0].volume = 0.42;
        strips[0].label = "Casque USB".to_string();
        strips[0].routes = vec![strips[1].uid.clone()];

        let document = store.document(
            &strips,
            Some(WindowGeometry {
                width: None,
                height: 900.0,
            }),
        );
        // Unknown top-level keys survive.
        assert!(document.get("window_geometry").is_some());
        assert!(document.get("window_state").is_some());
        // Geometry patched, the `window` object's other keys kept.
        assert_eq!(document["window"]["height"].as_f64(), Some(900.0));
        assert_eq!(document["window"]["x"], 0);
        // Unknown per-strip keys survive (MIDI mappings, mode, apps).
        let first = &document["strips"][0];
        assert_eq!(first["label"], "Casque USB");
        assert!((first["volume"].as_f64().unwrap() - 0.42).abs() < 1e-6);
        assert_eq!(first["mode"], "physical");
        assert_eq!(first["midi_volume"]["control"], 61);
        assert!(first["assigned_apps"].is_array());
        // …and the effects keep their port order (IndexMap, not HashMap).
        let params = first["effects"]["eq"]["params"].as_object().unwrap();
        let keys: Vec<&str> = params.keys().map(String::as_str).collect();
        assert_eq!(keys[0], "50Hz gain (low shelving)");
        assert_eq!(keys[14], "20000Hz gain");

        store
            .save_now(
                &strips,
                Some(WindowGeometry {
                    width: None,
                    height: 900.0,
                }),
            )
            .unwrap();
        let reread = UiConfig::open(&path).unwrap();
        let reloaded = reread.strips();
        assert_eq!(reloaded.len(), 7);
        assert_eq!(reloaded[0].label, "Casque USB");
        assert!((reloaded[0].volume - 0.42).abs() < 1e-6);
        assert_eq!(reloaded[0].routes, vec![reloaded[1].uid.clone()]);
        // The MIDI key is still there after a real save.
        let raw = config::load_config(&path).unwrap();
        assert_eq!(raw["strips"][0]["midi_volume"]["control"], 61);
        assert_eq!(raw["strips"][0]["midi_mute"]["note"], 24);
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_new_strip_is_written_with_the_python_key_set() {
        let dir = scratch("newstrip");
        let store = UiConfig::open(dir.join("config.json")).unwrap();
        let strip = Strip::with_uid("in-1", "Mic", StripType::Input);
        let document = store.document(&[strip], None);
        let entry = &document["strips"][0];
        for key in [
            "uid",
            "label",
            "kind",
            "mode",
            "volume",
            "mute",
            "is_mono",
            "routes",
            "device_name",
            "assigned_apps",
            "is_default",
            "midi_volume",
            "midi_mute",
            "midi_mono",
            "effects",
        ] {
            assert!(entry.get(key).is_some(), "missing key {key}: {entry}");
        }
        assert_eq!(entry["mode"], "virtual");
        assert_eq!(entry["device_name"], Value::Null);
        assert_eq!(entry["effects"].as_object().unwrap().len(), 5);
    }

    #[test]
    fn save_if_due_only_writes_after_the_debounce() {
        let dir = scratch("due");
        let path = dir.join("config.json");
        let mut store = UiConfig::open(&path).unwrap();
        let strips = vec![Strip::with_uid("in-1", "Mic", StripType::Input)];
        let now = Instant::now();
        store.request_save(now);
        assert!(store.save_if_due(now, &strips, None).is_none());
        assert!(!path.exists());
        let due = store
            .save_if_due(now + SAVE_DEBOUNCE, &strips, None)
            .unwrap();
        assert!(due.is_ok());
        assert!(path.exists());
        assert!(!store.save_pending());
    }

    #[test]
    fn assigned_apps_are_read_and_written_without_touching_the_other_keys() {
        let dir = scratch("apps");
        let path = dir.join("config.json");
        std::fs::copy(repo_config_path(), &path).unwrap();
        let mut store = UiConfig::open(&path).unwrap();
        // The shipped fixture assigns apps to some strip; whatever it is, it is
        // read back with the same shape.
        let initial = store.assigned_apps();
        let mut strips = store.strips();
        let uid = strips[0].uid.clone();
        store.set_assigned_apps(&uid, &["firefox".to_string(), "discord".to_string()]);
        let document = store.document(&strips, None);
        let entry = document["strips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["uid"].as_str() == Some(uid.as_str()))
            .unwrap();
        assert_eq!(entry["assigned_apps"], json!(["firefox", "discord"]));
        // Everything else about that strip is still there.
        assert_eq!(entry["midi_volume"]["control"], 61);
        assert_eq!(entry["mode"], "physical");
        store.save_now(&strips, None).unwrap();
        let reread = UiConfig::open(&path).unwrap();
        let saved = reread.assigned_apps();
        assert_eq!(
            saved.get(&uid).map(Vec::as_slice),
            Some(["firefox".to_string(), "discord".to_string()].as_slice())
        );
        // The other strips keep what they had.
        for (other, names) in &initial {
            if other != &uid {
                assert_eq!(saved.get(other), Some(names));
            }
        }
        // Clearing removes the names but keeps the key.
        strips[0].volume = 0.5;
        store.set_assigned_apps(&uid, &[]);
        let document = store.document(&strips, None);
        assert_eq!(document["strips"][0]["assigned_apps"], json!([]));
    }

    #[test]
    fn midi_bindings_are_read_and_written_in_the_python_shape() {
        use crate::midi::{Binding, MidiTarget};

        let dir = scratch("midi");
        let path = dir.join("config.json");
        std::fs::copy(repo_config_path(), &path).unwrap();
        let mut store = UiConfig::open(&path).unwrap();
        let strips = store.strips();
        let uid = strips[0].uid.clone();
        // The fixture's first strip is bound: channel 0, CC 61, note 24/22.
        let bindings = store.midi_bindings();
        let entry = bindings.get(&uid).expect("the fixture has MIDI mappings");
        assert!(
            entry.contains(&(MidiTarget::Volume, Binding::control(0, 61))),
            "{entry:?}"
        );
        assert!(
            entry.contains(&(MidiTarget::Mute, Binding::note(0, 24))),
            "{entry:?}"
        );
        assert!(
            entry.contains(&(MidiTarget::Mono, Binding::note(0, 22))),
            "{entry:?}"
        );

        // Learning a new volume knob rewrites only that key.
        store.set_midi_binding(&uid, MidiTarget::Volume, Some(Binding::control(3, 7)));
        let document = store.document(&strips, None);
        let strip = document["strips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["uid"].as_str() == Some(uid.as_str()))
            .unwrap();
        assert_eq!(strip["midi_volume"]["control"], 7);
        assert_eq!(strip["midi_volume"]["channel"], 3);
        assert_eq!(strip["midi_mute"]["note"], 24, "the other mappings survive");
        // Clearing writes a null (the Python shape), never removes the key.
        store.set_midi_binding(&uid, MidiTarget::Mute, None);
        let document = store.document(&strips, None);
        assert_eq!(document["strips"][0]["midi_mute"], Value::Null);
        store.save_now(&strips, None).unwrap();
        let reread = UiConfig::open(&path).unwrap();
        let mappings = reread.midi_bindings();
        assert!(!mappings
            .get(&uid)
            .unwrap()
            .iter()
            .any(|(target, _)| *target == MidiTarget::Mute));
    }

    #[test]
    fn rust_only_settings_live_under_their_own_key() {
        let dir = scratch("uirust");
        let path = dir.join("config.json");
        std::fs::copy(repo_config_path(), &path).unwrap();
        let mut store = UiConfig::open(&path).unwrap();
        assert_eq!(store.ui_rust_bool("always_on_top"), None);
        store.set_ui_rust_bool("always_on_top", false);
        store.set_ui_rust_bool("tray_enabled", true);
        let strips = store.strips();
        store.save_now(&strips, None).unwrap();
        let raw = config::load_config(&path).unwrap();
        assert_eq!(raw["ui_rust"]["always_on_top"], json!(false));
        assert_eq!(raw["ui_rust"]["tray_enabled"], json!(true));
        // The Qt geometry blob is untouched by a Rust-only setting.
        assert!(raw.get("window_geometry").is_some());
        let reread = UiConfig::open(&path).unwrap();
        assert_eq!(reread.ui_rust_bool("always_on_top"), Some(false));
        assert_eq!(reread.ui_rust_bool("tray_enabled"), Some(true));
    }

    /// `ui_rust.midi_port` is the one new persisted key of the MIDI work: the
    /// controller name must survive a save, an empty/null value must read as
    /// "no controller", and the Python side (which copies the document) must not
    /// lose it.
    #[test]
    fn the_midi_port_name_is_persisted_under_ui_rust() {
        let dir = scratch("uirust-midi");
        let path = dir.join("config.json");
        std::fs::copy(repo_config_path(), &path).unwrap();
        let mut store = UiConfig::open(&path).unwrap();
        assert_eq!(store.ui_rust_string("midi_port"), None, "fresh config");
        let strips = store.strips();
        // Nothing chosen yet: no key written (a save is not a decision).
        store.save_now(&strips, None).unwrap();
        let raw = config::load_config(&path).unwrap();
        assert!(raw
            .get("ui_rust")
            .and_then(|entry| entry.get("midi_port"))
            .is_none());

        store.set_ui_rust_string("midi_port", Some("  MIDI Mix:MIDI Mix MIDI 1 24:0  "));
        assert_eq!(
            store.ui_rust_string("midi_port").as_deref(),
            Some("MIDI Mix:MIDI Mix MIDI 1 24:0"),
            "the name is trimmed, it is matched against ALSA's exactly"
        );
        store.save_now(&strips, None).unwrap();
        let raw = config::load_config(&path).unwrap();
        assert_eq!(
            raw["ui_rust"]["midi_port"],
            json!("MIDI Mix:MIDI Mix MIDI 1 24:0")
        );
        // The MIDI mappings and the Qt blob are untouched by that write.
        assert_eq!(raw["strips"][0]["midi_volume"]["control"], 61);
        assert!(raw.get("window_geometry").is_some());
        let reread = UiConfig::open(&path).unwrap();
        assert_eq!(
            reread.ui_rust_string("midi_port").as_deref(),
            Some("MIDI Mix:MIDI Mix MIDI 1 24:0")
        );

        // Selecting "no controller" clears the name (null, not a stale string).
        let mut store = reread;
        store.set_ui_rust_string("midi_port", None);
        store.save_now(&strips, None).unwrap();
        let raw = config::load_config(&path).unwrap();
        assert_eq!(raw["ui_rust"]["midi_port"], Value::Null);
        assert_eq!(UiConfig::open(&path).unwrap().ui_rust_string("midi_port"), None);
        // A non-object `ui_rust` does not swallow the setting.
        let mut broken = UiConfig::open(&path).unwrap();
        broken.raw = json!({"ui_rust": 7, "strips": []});
        broken.set_ui_rust_string("midi_port", Some("X"));
        assert_eq!(broken.ui_rust_string("midi_port").as_deref(), Some("X"));
    }

    /// Both shapes of a stored mapping are read; only the object form is
    /// written (the one Python understands).
    #[test]
    fn a_bare_number_mapping_is_read_and_rewritten_as_an_object() {
        use crate::midi::{Binding, MidiTarget};

        let dir = scratch("midi-legacy");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            json!({
                "strips": [
                    {
                        "uid": "in-legacy",
                        "label": "Mic",
                        "kind": "input",
                        "volume": 1.0,
                        "mute": false,
                        "is_mono": false,
                        "routes": [],
                        "device_name": null,
                        "assigned_apps": [],
                        "is_default": false,
                        "midi_volume": 61,
                        "midi_mute": 24,
                        "midi_mono": null
                    },
                    {
                        "uid": "in-rich",
                        "label": "Web",
                        "kind": "input",
                        "midi_volume": {"type": "control_change", "channel": 2, "control": 27},
                        "midi_mute": {"type": "note_on", "channel": 0, "note": 9}
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let mut store = UiConfig::open(&path).unwrap();
        let bindings = store.midi_bindings();
        assert_eq!(
            bindings.get("in-legacy"),
            Some(&vec![
                (MidiTarget::Volume, Binding::control(0, 61)),
                (MidiTarget::Mute, Binding::note(0, 24)),
            ]),
            "a bare number is read as channel 0 + the target's natural shape"
        );
        assert_eq!(
            bindings.get("in-rich"),
            Some(&vec![
                (MidiTarget::Volume, Binding::control(2, 27)),
                (MidiTarget::Mute, Binding::note(0, 9)),
            ]),
            "the object form keeps its channel"
        );
        // A save rewrites them in the object shape Python reads.
        let strips = store.strips();
        store.save_now(&strips, None).unwrap();
        let raw = config::load_config(&path).unwrap();
        let legacy = raw["strips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["uid"] == json!("in-legacy"))
            .unwrap();
        assert_eq!(
            legacy["midi_volume"],
            json!({"type": "control_change", "channel": 0, "control": 61})
        );
        assert_eq!(
            legacy["midi_mute"],
            json!({"type": "note_on", "channel": 0, "note": 24})
        );
        // Still `null` for the unmapped one (the Python shape).
        assert_eq!(legacy["midi_mono"], Value::Null);
        let rich = raw["strips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["uid"] == json!("in-rich"))
            .unwrap();
        assert_eq!(rich["midi_volume"]["channel"], 2);
    }

    #[test]
    fn window_geometry_rejects_nonsense() {
        assert_eq!(WindowGeometry::from_json(&json!({})), None);
        assert_eq!(WindowGeometry::from_json(&json!({"height": 0})), None);
        assert_eq!(WindowGeometry::from_json(&json!({"height": -4})), None);
        assert_eq!(WindowGeometry::from_json(&json!("423")), None);
        assert_eq!(
            WindowGeometry::from_json(&json!({"height": 423, "width": 800})),
            Some(WindowGeometry {
                width: Some(800.0),
                height: 423.0
            })
        );
    }

    /// F6: `height` wins over the legacy Qt `h`; `h` is only a fallback (the
    /// shapes are not the same measurement, see `WindowGeometry::from_json`).
    #[test]
    fn the_qt_h_key_is_only_a_height_fallback() {
        assert_eq!(
            WindowGeometry::from_json(&json!({"x": 2560, "y": 0, "h": 575})),
            Some(WindowGeometry {
                width: None,
                height: 575.0
            })
        );
        assert_eq!(
            WindowGeometry::from_json(&json!({"height": 423})).map(|g| g.height),
            Some(423.0),
            "the Rust key wins when both are present"
        );
    }

    /// F1: `effects` is patched key by key, never rebuilt — an effect the model
    /// does not know (`reverb`, `limiter`, …) must survive a save.
    #[test]
    fn an_unknown_effect_survives_a_save() {
        let dir = scratch("unknown-fx");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "strips": [{
                    "uid": "in-1",
                    "label": "Mic",
                    "kind": "input",
                    "mode": "physical",
                    "device_name": "alsa_input.test",
                    "effects": {
                        "gate": {"active": true, "params": {"Threshold (dB)": -40.0}},
                        "reverb": {"active": true, "params": {"Room size": 0.7, "Wet": 0.3}},
                        "limiter": {"active": false, "params": {"Ceiling (dB)": -1.0}},
                        "eq": {"active": false, "params": {"100Hz gain": 2.0}}
                    }
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let mut store = UiConfig::open(&path).unwrap();
        let mut strips = store.strips();
        assert_eq!(strips.len(), 1);
        // The modeled part loaded, including the migrated/missing ports.
        assert!(strips[0].effects[&EffectKey::Gate].active);
        strips[0].volume = 0.5;
        strips[0].effects.get_mut(&EffectKey::Eq).unwrap().active = true;
        store.save_now(&strips, None).unwrap();

        let raw = config::load_config(&path).unwrap();
        let effects = raw["strips"][0]["effects"].as_object().unwrap();
        // Unknown effects: still there, untouched (values and key order).
        assert_eq!(effects["reverb"]["active"], json!(true));
        assert_eq!(effects["reverb"]["params"]["Room size"], json!(0.7));
        assert_eq!(effects["limiter"]["params"]["Ceiling (dB)"], json!(-1.0));
        let keys: Vec<&str> = effects.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "gate",
                "reverb",
                "limiter",
                "eq",
                "noise_cancel",
                "tube",
                "compressor"
            ],
            "known keys keep their place, missing ones are appended"
        );
        // …and the modeled edit did land.
        assert_eq!(effects["eq"]["active"], json!(true));
        assert_eq!(raw["strips"][0]["volume"], json!(0.5f32));
    }

    /// F2: `mode` is the file's business as long as the device binding did not
    /// change — `physical` + `device_name: null` must not become `virtual`.
    #[test]
    fn mode_is_not_rewritten_when_the_device_did_not_change() {
        let dir = scratch("mode");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "strips": [
                    {"uid": "in-1", "label": "A", "kind": "input",
                     "mode": "physical", "device_name": null, "volume": 1.0},
                    {"uid": "in-2", "label": "B", "kind": "input",
                     "mode": "virtual", "device_name": "alsa_input.b", "volume": 1.0},
                    // No `mode` at all: the derived one is written.
                    {"uid": "out-1", "label": "C", "kind": "output", "volume": 1.0}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let store = UiConfig::open(&path).unwrap();
        let mut strips = store.strips();
        for strip in &mut strips {
            strip.volume = 0.25;
        }
        let document = store.document(&strips, None);
        assert_eq!(
            document["strips"][0]["mode"], "physical",
            "kept: null device, no change"
        );
        assert_eq!(
            document["strips"][1]["mode"], "virtual",
            "kept: odd but the user's"
        );
        assert_eq!(
            document["strips"][2]["mode"], "virtual",
            "derived when the file has none"
        );

        // A real device change does re-derive it.
        strips[0].device_name = Some("alsa_input.new".to_string());
        strips[1].device_name = None;
        let document = store.document(&strips, None);
        assert_eq!(document["strips"][0]["mode"], "physical");
        assert_eq!(document["strips"][1]["mode"], "virtual");
    }

    /// F3: a strip with no `uid` keeps its extras (`mode`, `assigned_apps`,
    /// `midi_*`) — the uid is repaired **before** the extras are mapped.
    #[test]
    fn a_strip_without_uid_keeps_its_extras() {
        let dir = scratch("no-uid");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "strips": [{
                    "label": "Legacy mic",
                    "kind": "input",
                    "mode": "physical",
                    "device_name": "alsa_input.legacy",
                    "assigned_apps": ["Firefox"],
                    "midi_volume": {"channel": 1, "control": 21, "type": "control_change"},
                    "fx_custom": {"anything": true}
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let mut store = UiConfig::open(&path).unwrap();
        let mut strips = store.strips();
        assert_eq!(strips[0].uid, "in-legacy-1");
        // The extras are reachable through the repaired uid, not lost.
        assert_eq!(
            store.assigned_apps().get("in-legacy-1").map(Vec::as_slice),
            Some(["Firefox".to_string()].as_slice())
        );
        assert_eq!(
            store.midi_bindings().get("in-legacy-1").map(Vec::len),
            Some(1)
        );
        strips[0].volume = 0.5;
        store.save_now(&strips, None).unwrap();

        let raw = config::load_config(&path).unwrap();
        let entry = &raw["strips"][0];
        assert_eq!(entry["uid"], "in-legacy-1", "the repaired uid is persisted");
        assert_eq!(entry["mode"], "physical");
        assert_eq!(entry["device_name"], "alsa_input.legacy");
        assert_eq!(entry["assigned_apps"], json!(["Firefox"]));
        assert_eq!(entry["midi_volume"]["control"], 21);
        assert_eq!(entry["fx_custom"], json!({"anything": true}));
        assert_eq!(entry["volume"], json!(0.5f32));
    }

    /// F5: the width is only refreshed when the document already had one — the
    /// shipped Qt config has no `window.width`, and a save must not add it.
    #[test]
    fn the_width_is_only_written_when_the_document_had_one() {
        let without = scratch("width-none");
        let path = without.join("config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "strips": [],
                "window": {"x": 0, "y": 0, "height": 423, "maximized": false}
            }))
            .unwrap(),
        )
        .unwrap();
        let store = UiConfig::open(&path).unwrap();
        assert!(!store.has_stored_width());
        let document = store.document(
            &[],
            Some(WindowGeometry {
                width: None,
                height: 600.0,
            }),
        );
        assert_eq!(document["window"]["height"], json!(600.0));
        assert!(document["window"].get("width").is_none(), "no width added");
        assert_eq!(document["window"]["x"], 0, "the other keys are kept");

        let with = scratch("width-some");
        let path = with.join("config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "strips": [],
                "window": {"x": 0, "y": 0, "width": 800, "height": 423}
            }))
            .unwrap(),
        )
        .unwrap();
        let store = UiConfig::open(&path).unwrap();
        assert!(store.has_stored_width());
        let document = store.document(
            &[],
            Some(WindowGeometry {
                width: Some(794.0),
                height: 600.0,
            }),
        );
        assert_eq!(
            document["window"]["width"],
            json!(794.0),
            "an existing width is refreshed"
        );
    }
}
