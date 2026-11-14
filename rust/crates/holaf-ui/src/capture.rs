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
//! pw-cat --record --target <source>.monitor --format f32 --rate 48000 \
//!        --channels 2 -            → raw f32le on stdout
//! ```
//!
//! Measured on the headless sandbox (PipeWire 0.3.65): 10 ms of stereo audio
//! costs one 3840-byte read, a `pw-cat` process idles at ~0.2 % CPU, and a
//! sine played into a bus sink shows up on its `.monitor` at the exact
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
//! usable — metering is never allowed to break the mixer.

use std::collections::{HashMap, HashSet};
use std::io::Read;
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

/// One running capture: the child process plus its reader thread.
struct CaptureStream {
    source: String,
    child: Child,
    reader: Option<JoinHandle<()>>,
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
        // 1. stop everything that is no longer wanted or has moved.
        let stale: Vec<String> = self
            .streams
            .iter()
            .filter(|(uid, stream)| match wanted.get(*uid) {
                // Still wanted on the same source: keep the running child.
                Some(source) => source != &stream.source,
                // No longer wanted at all.
                None => true,
            })
            .map(|(uid, _)| uid.clone())
            .collect();
        for uid in stale {
            if let Some(mut stream) = self.streams.remove(&uid) {
                log::debug!("metering: stop {} ({})", uid, stream.source);
                stop_stream(&mut stream);
                board.publish(slots.get(&uid).copied().unwrap_or(0), [0.0, 0.0]);
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
            match spawn_stream(&self.recorder, uid, source, slot, board.clone()) {
                Ok(stream) => {
                    log::info!("metering: recording {source} for {uid} (slot {slot})");
                    self.streams.insert(uid.clone(), stream);
                }
                Err(err) => {
                    if self.warned.insert(source.clone()) {
                        log::warn!("metering: cannot record {source}: {err}");
                    }
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
    let mut child = Command::new(recorder)
        .arg("--record")
        .arg("--target")
        .arg(source)
        .arg("--format")
        .arg("f32")
        .arg("--rate")
        .arg(METER_RATE.to_string())
        .arg("--channels")
        .arg(METER_CHANNELS.to_string())
        // `-` = raw stream on stdout (verified on pw-cat 0.3.65).
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("{recorder}: {err}"))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "pw-cat has no stdout".to_string())?;
    let label = uid.to_string();
    let reader = thread::Builder::new()
        .name(format!("holaf-meter-{uid}"))
        .spawn(move || {
            let mut buffer = vec![0_u8; BLOCK_BYTES];
            loop {
                match stdout.read_exact(&mut buffer) {
                    Ok(()) => {
                        let samples: Vec<f32> = buffer
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|chunk| f32::from_le_bytes(*chunk))
                            .collect();
                        // RMS × 5 clipped to 1.0 — the Qt rule; `analyse_block`
                        // also yields the peak (used by tests/diagnostics).
                        board.publish(slot, analyse_block(&samples, METER_CHANNELS).level());
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
        .map_err(|err| format!("cannot start the meter reader: {err}"))?;

    Ok(CaptureStream {
        source: source.to_string(),
        child,
        reader: Some(reader),
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

    #[test]
    fn a_uid_without_a_board_slot_is_not_captured() {
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder("true");
        let mut wanted = HashMap::new();
        wanted.insert("ghost".to_string(), "whatever.monitor".to_string());
        capture.sync(&wanted, &HashMap::new(), &board);
        assert!(capture.is_empty());
    }
}
