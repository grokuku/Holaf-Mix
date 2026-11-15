//! Capture streams for the VU meters (feature `engine-pw`).
//!
//! # Why a `pw-cat` child process and not `pw::stream`
//!
//! The Rust `pipewire` crate's `Stream` must be created, connected and
//! iterated on the thread that owns its mainloop, exactly like the
//! `Context`/`Core` the `holaf-pw` engine keeps to itself.  The engine does not
//! expose either (`Backend` is a JSON RPC front over a private mainloop), and
//! opening a *second* PipeWire context inside the UI process would duplicate
//! the graph view for no benefit — so the UI records each source with
//! `pw-cat --record`, which **is** a `pw-stream` in input mode, one child
//! process per source:
//!
//! ```text
//! # a bus / output strip: record the MONITOR of a sink
//! pw-cat --record --target Holaf_Bus_Mic \
//!        -P {"stream.capture.sink":true} \
//!        --format f32 --rate 48000 --channels 2 -     → raw f32le on stdout
//! ```
//!
//! # The two traps this module exists to get right
//!
//! 1. **`--target` takes a node name, not a PulseAudio source name.**
//!    `Backend::meter_sources()` speaks the PulseAudio vocabulary the Python UI
//!    needed (`PULSE_SOURCE=Holaf_Bus_Mic.monitor`, `alsa_output.….monitor` for a
//!    device), because `sounddevice`/PortAudio reads through pipewire-pulse.
//!    `pw-cat` is a *native* PipeWire client: `--target Holaf_Bus_Mic.monitor`
//!    resolves to no node at all and — this is the part that made the meters go
//!    dark without a single error — the stream silently falls back to the
//!    **default source**.  Every capture then recorded the default microphone,
//!    published silence, and the VU bars stayed at zero while `pw-cat` ran
//!    happily.  [`capture_target`] therefore strips the `.monitor` suffix and
//!    flags the stream as a sink monitor.
//! 2. **A capture stream on a sink needs `stream.capture.sink=true`.**  Target
//!    a sink node without it and the session manager links the stream to the
//!    *default source* instead of that sink's monitor ports (verified on the
//!    headless sandbox: same command, RMS 0.0000 → 0.3497 once the property is
//!    there).  Device sources (`alsa_input…`) must **not** get it: they are real
//!    capture nodes and are linked directly.
//!
//! Measured on the headless sandbox (PipeWire 0.3.65): 10 ms of stereo audio
//! costs one 3840-byte read, a `pw-cat` process idles at ~0.2 % CPU, and a
//! sine played into a bus sink shows up on its monitor at the exact
//! amplitude.  The alternative (`pw::stream` in this process) buys nothing the
//! Python engine did not already do with PortAudio — and costs a mainloop
//! thread plus a `!Send` type inside the UI.
//!
//! The sources come from `Backend::meter_sources()` (the daemon names them;
//! spec §6), so this module never guesses a node name:
//! [`MeterCapture::sync`] is a pure reconciliation of "the strips need these
//! sources" against "these children are running".
//!
//! Failure handling: a source that cannot be opened (device gone, `pw-cat`
//! missing) is logged **once**, its meter publishes 0, and the strip stays
//! usable — metering is never allowed to break the mixer.  Nothing fails
//! silently: the child's stderr is drained into the log, and a child that dies
//! right after it was spawned is reported with what it said.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};

use crate::meters::{analyse_block, MeterBoard, BLOCK_FRAMES, METER_CHANNELS, METER_RATE};

/// Bytes of one published block (`480 frames × 2 ch × 4 B`).
const BLOCK_BYTES: usize = BLOCK_FRAMES * METER_CHANNELS * 4;

/// Default recording client.  Overridable (`HOLAF_PW_CAT`) so a test — or a
/// build where `pw-cat` is a wrapper — can point somewhere else without a
/// process-wide environment mutation.
pub fn default_recorder() -> String {
    std::env::var("HOLAF_PW_CAT").unwrap_or_else(|_| "pw-cat".to_string())
}

/// Suffix pipewire-pulse gives the monitor “source” of a sink.
pub const MONITOR_SUFFIX: &str = ".monitor";

/// Node properties that make a capture stream record a **sink's monitor ports**
/// instead of a source node.
///
/// `stream.capture.sink` is the property pipewire-pulse/wireplumber look at when
/// they choose the link target of a capture stream: with it, targeting a sink
/// node links to that sink's `monitor_FL/FR`; without it the stream is linked to
/// the **default source** and records something else entirely (silence, in a
/// mixer whose default source is nobody).  Passed as `pw-cat -P '<json>'`.
pub const MONITOR_PROPERTIES: &str = "{\"stream.capture.sink\":true}";

/// What `--target` must be given for a `meter_sources()` entry, and whether the
/// capture is a **sink monitor**.
///
/// `Backend::meter_sources()` answers in the PulseAudio vocabulary (that is what
/// the Python UI fed to `PULSE_SOURCE`): `Holaf_Bus_Mic.monitor` for a virtual
/// bus, `alsa_output.….monitor` for a physical output, `alsa_input.…` for a real
/// capture device.
///
/// `pw-cat --target` matches a **node name**, so the `.monitor` suffix has to go:
/// it is the *id of a pulse source*, not a node.  Left in place it silently
/// resolves to nothing and the stream records the default source — the exact bug
/// that made every VU meter stay dark.  The suffix is not thrown away: it is
/// what tells [`spawn_stream`] to ask for the sink's monitor ports.
///
/// ```text
/// Holaf_Bus_Mic.monitor          → ("Holaf_Bus_Mic", true)
/// alsa_output.usb-X.analog.monitor → ("alsa_output.usb-X.analog", true)
/// alsa_input.usb-X.analog-stereo → ("alsa_input.usb-X.analog-stereo", false)
/// ```
pub fn capture_target(source: &str) -> (&str, bool) {
    match source.strip_suffix(MONITOR_SUFFIX) {
        Some(node) if !node.is_empty() => (node, true),
        // A source that merely *ends* with the suffix but is empty without it is
        // not a monitor name: record it as it is.
        _ => (source, false),
    }
}

/// One running capture: the child process plus its reader thread.
struct CaptureStream {
    source: String,
    /// The board slot the reader publishes into.  Kept so a reassignment (a
    /// strip was added or removed) restarts the capture instead of letting it
    /// paint somebody else's bar.
    slot: usize,
    child: Child,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
}

/// All capture streams of the mixer, keyed by strip uid.
pub struct MeterCapture {
    streams: HashMap<String, CaptureStream>,
    /// Sources already reported as unopenable, so the log is not spammed on
    /// every snapshot.
    warned: HashSet<String>,
    /// The recording client (see [`default_recorder`]).
    recorder: String,
}

impl std::fmt::Debug for MeterCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeterCapture")
            .field("streams", &self.streams.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for MeterCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl MeterCapture {
    pub fn new() -> Self {
        Self::with_recorder(default_recorder())
    }

    /// Same, with an explicit recording client (tests use `true`, which spawns
    /// and exits without producing a block — the "source is unreadable" path).
    pub fn with_recorder(recorder: impl Into<String>) -> Self {
        Self {
            streams: HashMap::new(),
            warned: HashSet::new(),
            recorder: recorder.into(),
        }
    }

    /// How many captures are running (a strip with no resolvable source has
    /// none).
    pub fn len(&self) -> usize {
        self.streams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    /// The `(uid, source)` pairs currently being captured.
    pub fn running(&self) -> Vec<(String, String)> {
        self.streams
            .iter()
            .map(|(uid, stream)| (uid.clone(), stream.source.clone()))
            .collect()
    }

    /// Reconcile the running captures with what the engine says the strips need.
    ///
    /// * a new uid → spawn a `pw-cat` recording its source into its board slot;
    /// * a uid that disappeared (strip deleted) → kill its child;
    /// * a uid whose source changed (device rebind) → kill + respawn;
    /// * an unchanged uid → left alone (no audio glitch on every snapshot).
    ///
    /// `slots` is the board assignment the engine thread just made; a uid with
    /// no slot is not captured (the board is full).
    pub fn sync(
        &mut self,
        wanted: &HashMap<String, String>,
        slots: &HashMap<String, usize>,
        board: &MeterBoard,
    ) {
        // 1. stop everything that is no longer wanted, has moved, or whose board
        //    slot was reassigned.
        let stale: Vec<String> = self
            .streams
            .iter()
            .filter(|(uid, stream)| match wanted.get(*uid) {
                // Still wanted on the same source and slot: keep the child.
                Some(source) => {
                    source != &stream.source || slots.get(*uid).copied() != Some(stream.slot)
                }
                // No longer wanted at all.
                None => true,
            })
            .map(|(uid, _)| uid.clone())
            .collect();
        for uid in stale {
            if let Some(mut stream) = self.streams.remove(&uid) {
                log::debug!("metering: stop {} ({})", uid, stream.source);
                stop_stream(&mut stream);
                board.publish(stream.slot, [0.0, 0.0]);
            }
        }

        // 2. start what is missing.
        for (uid, source) in wanted {
            if self.streams.contains_key(uid) {
                continue;
            }
            let Some(slot) = slots.get(uid).copied() else {
                continue;
            };
            // A source that already failed to start is not retried on every
            // snapshot (a missing device would spawn a doomed child every 2 s
            // and flood the log): the warning is already there.
            if self.warned.contains(source) {
                board.publish(slot, [0.0, 0.0]);
                continue;
            }
            match spawn_stream(&self.recorder, uid, source, slot, board.clone()) {
                Ok(stream) => {
                    let (node, monitor) = capture_target(source);
                    log::info!(
                        "metering: recording {source} for {uid} (slot {slot}, target {node}{})",
                        if monitor { ", sink monitor" } else { "" }
                    );
                    self.streams.insert(uid.clone(), stream);
                }
                Err(err) => {
                    self.warned.insert(source.clone());
                    log::warn!("metering: cannot record {source}: {err}");
                    board.publish(slot, [0.0, 0.0]);
                }
            }
        }
    }

    /// Stop every capture (engine shutdown / `Drop`).  Joins each reader, so no
    /// thread or child outlives the bridge.
    pub fn stop_all(&mut self) {
        let streams: Vec<(String, CaptureStream)> = self.streams.drain().collect();
        for (uid, mut stream) in streams {
            log::debug!("metering: stopping {uid} ({})", stream.source);
            stop_stream(&mut stream);
        }
        self.warned.clear();
    }
}

impl Drop for MeterCapture {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Start one `pw-cat --record` and its reader thread.
fn spawn_stream(
    recorder: &str,
    uid: &str,
    source: &str,
    slot: usize,
    board: MeterBoard,
) -> Result<CaptureStream, String> {
    // `--target` wants a node name; a `.monitor` source is a sink monitor and
    // needs `stream.capture.sink` (see the module docs — without either, the
    // stream quietly records the default source and the meter stays dark).
    let (node, monitor) = capture_target(source);
    let mut command = Command::new(recorder);
    command.arg("--record");
    if monitor {
        command.arg("-P").arg(MONITOR_PROPERTIES);
    }
    let mut child = command
        .arg("--target")
        .arg(node)
        .arg("--format")
        .arg("f32")
        .arg("--rate")
        .arg(METER_RATE.to_string())
        .arg("--channels")
        .arg(METER_CHANNELS.to_string())
        // `-` = raw stream on stdout (verified on pw-cat 0.3.65 and 1.x).
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Kept, and drained into the log: a `pw-cat` that cannot open its
        // target says so on stderr, and swallowing it is how a broken meter
        // became invisible.
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("{recorder}: {err}"))?;

    // Every failure path below must reap the child: a `pw-cat` left running with
    // no reader would hold the graph open for the whole session.
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("pw-cat has no stdout".to_string());
    };
    let label = uid.to_string();
    let reader = thread::Builder::new()
        .name(format!("holaf-meter-{uid}"))
        .spawn(move || {
            let mut buffer = vec![0_u8; BLOCK_BYTES];
            // One-shot reporters: a stream that is misread must say so once, and
            // the first block's numbers are what a bug report needs (they are the
            // only place where the *real* RMS of a strip is visible).
            let mut announced = false;
            let mut warned = false;
            loop {
                match stdout.read_exact(&mut buffer) {
                    Ok(()) => {
                        let samples: Vec<f32> = buffer
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|chunk| f32::from_le_bytes(*chunk))
                            .collect();
                        let sample = analyse_block(&samples, METER_CHANNELS);
                        // A stream read with the wrong sample format yields huge
                        // “samples”: drawing them pins the bar at 1.0 for ever
                        // (the “VU toujours à fond” report).  Drop the block and
                        // say it once — a dark bar plus a loud log line is the
                        // honest picture.
                        if sample.is_implausible() {
                            if !warned {
                                warned = true;
                                log::warn!(
                                    "metering: {label}: impossible block (peak {:?}, rms {:?}) — \
                                     the recorder is not writing f32le to stdout. The VU stays \
                                     dark instead of pinning at full scale.",
                                    sample.peak,
                                    sample.rms
                                );
                            }
                            board.publish(slot, [0.0, 0.0]);
                            continue;
                        }
                        if !announced {
                            announced = true;
                            log::info!(
                                "metering: {label}: first block rms {:.4}/{:.4} peak {:.4}/{:.4} \
                                 → level {:.2}/{:.2}",
                                sample.rms[0],
                                sample.rms[1],
                                sample.peak[0],
                                sample.peak[1],
                                sample.level()[0],
                                sample.level()[1]
                            );
                        }
                        board.publish(slot, sample.level());
                    }
                    Err(err) => {
                        if err.kind() != std::io::ErrorKind::UnexpectedEof {
                            log::warn!("metering: {label}: stream ended: {err}");
                        }
                        // The source disappeared (unplugged device, killed
                        // engine): a dark VU is the honest picture.
                        board.publish(slot, [0.0, 0.0]);
                        return;
                    }
                }
            }
        })
        .map_err(|err| format!("cannot start the meter reader: {err}"));

    let reader = match reader {
        Ok(reader) => reader,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };

    let stderr = child.stderr.take().and_then(|stderr| {
        let label = uid.to_string();
        thread::Builder::new()
            .name(format!("holaf-meter-err-{uid}"))
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    // "Cannot open the target", "device not found": loud.  The
                    // rest of what `pw-cat` says is diagnostic detail, not a
                    // failure — a normal run prints nothing at all.
                    let fatal = ["error", "fail", "cannot", "no such", "not found"]
                        .iter()
                        .any(|needle| line.to_ascii_lowercase().contains(needle));
                    if fatal {
                        log::warn!("metering: {label}: {line}");
                    } else {
                        log::debug!("metering: {label}: {line}");
                    }
                }
            })
            .ok()
    });

    Ok(CaptureStream {
        source: source.to_string(),
        slot,
        child,
        reader: Some(reader),
        stderr,
    })
}

/// Kill a capture's child and join its reader.
fn stop_stream(stream: &mut CaptureStream) {
    // Dropping the child's stdout (when the reader owns it) unblocks
    // `read_exact`; killing the process first is the deterministic order.
    let _ = stream.child.kill();
    // Waiting reaps the zombie (a defunct `pw-cat` would look alive to `pgrep`).
    let _ = stream.child.wait();
    if let Some(reader) = stream.reader.take() {
        let _ = reader.join();
    }
    // The stderr drainer ends with the pipe (both writers are gone).
    if let Some(stderr) = stream.stderr.take() {
        let _ = stderr.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_starts_and_stops_streams_for_the_requested_sources() {
        // `true` is a stand-in for `pw-cat`: it exits immediately without
        // producing a block, which is exactly the "source cannot be read" path.
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder("true");
        let slots = board.assign(&["in-mic".to_string(), "out-hp".to_string()]);
        let mut wanted = HashMap::new();
        wanted.insert("in-mic".to_string(), "Holaf_Bus_Mic.monitor".to_string());
        wanted.insert("out-hp".to_string(), "alsa.monitor".to_string());

        capture.sync(&wanted, &slots, &board);
        assert_eq!(capture.len(), 2);
        let running = capture.running();
        assert!(running
            .iter()
            .any(|(uid, source)| uid == "in-mic" && source == "Holaf_Bus_Mic.monitor"));

        // Unchanged set: nothing is restarted (no audio glitch on snapshots).
        capture.sync(&wanted, &slots, &board);
        assert_eq!(capture.len(), 2);

        // A moved source restarts that stream, a dropped one disappears.
        wanted.insert("out-hp".to_string(), "other.monitor".to_string());
        wanted.remove("in-mic");
        capture.sync(&wanted, &slots, &board);
        assert_eq!(capture.len(), 1);
        let running = capture.running();
        assert_eq!(running[0].0, "out-hp");
        assert_eq!(running[0].1, "other.monitor");
        // The removed strip's board slot was zeroed (the mapping is only
        // reassigned by `MeterBoard::assign`, from the engine thread).
        assert_eq!(board.read("in-mic"), Some([0.0, 0.0]));

        capture.stop_all();
        assert!(capture.is_empty());
    }

    /// The two rules that made every VU meter stay dark without a single error:
    /// `pw-cat --target` matches a **node**, and a sink needs
    /// `stream.capture.sink`.
    #[test]
    fn a_pulse_source_name_becomes_a_pw_cat_node_target() {
        // A virtual bus / output strip: record the sink's monitor.
        assert_eq!(
            capture_target("Holaf_Bus_Mic.monitor"),
            ("Holaf_Bus_Mic", true)
        );
        assert_eq!(
            capture_target("alsa_output.usb-PreSonus_Studio_26c-00.analog-surround-40.monitor"),
            (
                "alsa_output.usb-PreSonus_Studio_26c-00.analog-surround-40",
                true
            )
        );
        // A real capture device is its own node: no suffix, no sink property.
        assert_eq!(
            capture_target("alsa_input.usb-PreSonus_Studio_26c-00.analog-stereo"),
            ("alsa_input.usb-PreSonus_Studio_26c-00.analog-stereo", false)
        );
        // Only a *trailing* `.monitor` is a monitor marker.
        assert_eq!(capture_target("monitor"), ("monitor", false));
        assert_eq!(
            capture_target("foo.monitor.bar"),
            ("foo.monitor.bar", false)
        );
        // `.monitor` alone would leave an empty node name: keep the source.
        assert_eq!(capture_target(".monitor"), (".monitor", false));
        // The property handed to `pw-cat -P` is valid JSON for one boolean key.
        let parsed: serde_json::Value =
            serde_json::from_str(MONITOR_PROPERTIES).expect("a JSON object");
        assert_eq!(parsed["stream.capture.sink"], serde_json::json!(true));
    }

    /// A board reassignment (a strip was added or removed) must restart the
    /// capture: a reader keeps the slot index it was born with, so leaving it
    /// running would paint another strip's bar.
    #[test]
    fn a_reassigned_slot_restarts_the_capture() {
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder("true");
        let mut wanted = HashMap::new();
        wanted.insert("in-mic".to_string(), "alsa_input.mic".to_string());
        let mut slots = board.assign(&["in-mic".to_string()]);
        capture.sync(&wanted, &slots, &board);
        assert_eq!(capture.len(), 1);
        // `out-hp` is inserted before `in-mic`: same source, new slot.
        slots = board.assign(&["out-hp".to_string(), "in-mic".to_string()]);
        capture.sync(&wanted, &slots, &board);
        assert_eq!(
            capture.streams.get("in-mic").map(|stream| stream.slot),
            Some(1),
            "the running child must publish into its new slot"
        );
        capture.stop_all();
    }

    #[test]
    fn a_missing_recorder_is_reported_once_and_publishes_zero() {
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder("/nonexistent/holaf-pw-cat");
        let slots = board.assign(&["in-mic".to_string()]);
        let mut wanted = HashMap::new();
        wanted.insert("in-mic".to_string(), "Holaf_Bus_Mic.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        assert!(capture.is_empty());
        assert_eq!(board.read("in-mic"), Some([0.0, 0.0]));
        assert!(capture.warned.contains("Holaf_Bus_Mic.monitor"));
        // A second sync does not warn again but still publishes zero.
        capture.sync(&wanted, &slots, &board);
        assert!(capture.is_empty());
    }

    /// **End to end, with a real PipeWire** (opt-in): a tone played into a sink
    /// must reach the `MeterBoard` through `pw-cat`, i.e. the whole VU chain —
    /// `meter_sources()` name → `capture_target` → child process → block maths →
    /// board slot — really carries audio.
    ///
    /// Skipped unless both variables are set, so `cargo test` stays hermetic:
    ///
    /// ```bash
    /// eval "$(./scripts/start-pipewire-headless.sh --export)"
    /// ./dist/holafctl apply-snapshot --config config.json
    /// pw-cat --playback --target Holaf_Bus_Mix_Out /tmp/tone.wav &
    /// HOLAF_METER_LIVE_SINK=Holaf_Bus_Mix_Out \
    ///   HOLAF_METER_LIVE_WAV=/tmp/tone.wav \
    ///   cargo test -p holaf-ui --lib a_live_capture_feeds_the_board
    /// ```
    #[test]
    fn a_live_capture_feeds_the_board() {
        let (Ok(sink), Ok(wav)) = (
            std::env::var("HOLAF_METER_LIVE_SINK"),
            std::env::var("HOLAF_METER_LIVE_WAV"),
        ) else {
            eprintln!(
                "SKIP a_live_capture_feeds_the_board: set HOLAF_METER_LIVE_SINK and \
                 HOLAF_METER_LIVE_WAV (see the doc comment) to run it"
            );
            return;
        };
        // A tone into the sink, so its monitor has something to record.
        let mut player = Command::new(default_recorder())
            .args(["--playback", "--target", &sink, &wav])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("playback child");
        std::thread::sleep(std::time::Duration::from_millis(400));

        let board = MeterBoard::new();
        let mut capture = MeterCapture::new();
        let uid = "live".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        // Exactly what `meter_sources()` answers for a bus strip.
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), format!("{sink}.monitor"));
        capture.sync(&wanted, &slots, &board);

        let mut peak = 0.0_f32;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let level = board.read(&uid).unwrap_or([0.0, 0.0]);
            peak = peak.max(level[0]).max(level[1]);
            if peak > 0.1 {
                break;
            }
        }
        capture.stop_all();
        let _ = player.kill();
        let _ = player.wait();
        assert!(
            peak > 0.1,
            "the monitor of {sink} recorded only silence (peak {peak}): \
             `--target`/`stream.capture.sink` did not link to the sink"
        );
    }

    #[test]
    fn a_uid_without_a_board_slot_is_not_captured() {
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder("true");
        let mut wanted = HashMap::new();
        wanted.insert("ghost".to_string(), "whatever.monitor".to_string());
        capture.sync(&wanted, &HashMap::new(), &board);
        assert!(capture.is_empty());
    }

    // -----------------------------------------------------------------
    // The reader, against a stand-in recorder (no PipeWire needed)
    // -----------------------------------------------------------------

    /// An executable script that replays `payload` on stdout for ever — a
    /// `pw-cat --record` stand-in, so the *whole* reader path (child process →
    /// `read_exact` → f32 decode → block maths → board) is exercised without an
    /// audio server.
    fn fake_recorder(payload: &[u8]) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "holaf-capture-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.join("block.bin");
        std::fs::write(&data, payload).unwrap();
        let script = dir.join("recorder.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nwhile :; do cat '{}'; done\n", data.display()),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        script.to_string_lossy().into_owned()
    }

    /// Read the board for up to 2 s, returning the last level seen.
    fn wait_for_level(board: &MeterBoard, uid: &str) -> [f32; 2] {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut last = [0.0, 0.0];
        while std::time::Instant::now() < deadline {
            last = board.read(uid).unwrap_or([0.0, 0.0]);
            if last != [0.0, 0.0] {
                return last;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        last
    }

    /// A real f32le block (what `pw-cat --format f32` writes) reaches the board as
    /// the level the dBFS scale says — measured, not assumed: a −20 dBFS sine has
    /// RMS 0.0707 → −23 dBFS → level ≈ 0.56.
    #[test]
    fn a_valid_f32_block_reaches_the_board_with_the_expected_level() {
        let mut payload = Vec::with_capacity(BLOCK_BYTES);
        for n in 0..BLOCK_FRAMES {
            let sample = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
            payload.extend_from_slice(&sample.to_le_bytes());
            payload.extend_from_slice(&sample.to_le_bytes());
        }
        assert_eq!(payload.len(), BLOCK_BYTES);
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(fake_recorder(&payload));
        let uid = "tone".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_Tone.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        let level = wait_for_level(&board, &uid);
        assert!(
            (level[0] - 0.56).abs() < 0.02,
            "a −20 dBFS sine must read 0.56 on the bar, got {level:?}"
        );
        capture.stop_all();
    }

    /// **The “VU à fond” regression.**  The reader assumes `f32le` because it asks
    /// for `--format f32`; a stream handed over in another sample format decodes
    /// to huge “samples”, and drawing them pins the bar at 1.0 for ever (the user
    /// report: “toujours à fond, avec une baisse de 10 % de temps en temps”).
    /// Such a block is dropped: the bar stays **dark** and the log says why,
    /// which is a diagnosable failure instead of a plausible lie.
    #[test]
    fn a_stream_in_the_wrong_sample_format_never_pins_the_bar() {
        // 0xCC0C34F3 little-endian = -1.6e31 as f32: what s16 audio looks like
        // when it is read as f32.
        let mut payload = Vec::with_capacity(BLOCK_BYTES);
        for _ in 0..BLOCK_FRAMES * 2 {
            payload.extend_from_slice(&[0xF3, 0x34, 0x0C, 0xCC]);
        }
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(fake_recorder(&payload));
        let uid = "misread".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_Misread.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        // Give the reader plenty of blocks: it must publish zeros every time.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
        while std::time::Instant::now() < deadline {
            assert_eq!(
                board.read(&uid),
                Some([0.0, 0.0]),
                "a misread stream must never publish a level"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        capture.stop_all();
    }

    /// Silence stays silence (a real zero-filled f32 stream, not a misread one).
    #[test]
    fn a_zero_filled_stream_is_dark() {
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(fake_recorder(&[0_u8; BLOCK_BYTES]));
        let uid = "silent".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_Silent.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(board.read(&uid), Some([0.0, 0.0]));
        capture.stop_all();
    }
}
