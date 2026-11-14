//! Metering: block maths, decay envelope and the lock-free level board.
//!
//! The Rust UI reproduces the Python meter engine
//! (`src/backend/metering.py`) without its two dependencies — no `sounddevice`
//! (PortAudio) and no `PULSE_SOURCE` environment juggling.  The chain is:
//!
//! ```text
//! pw-cat --record --target <source>   (a PipeWire capture stream, one process
//!        │                             per strip source, sees `src/capture.rs`)
//!        ▼ raw f32le, 2 ch, 48 kHz
//! block maths: rms + peak per channel     ← this module (pure, unit-tested)
//!        ▼
//! MeterBoard: Arc<[AtomicU32]>            ← the UI reads f32 bits, no lock
//!        ▼ polled at 25 Hz by AppState::poll_engine
//! decay envelope (level = max(new, prev × 0.85 per 40 ms))
//!        ▼
//! VU bars ([`crate::widgets::fader`])
//! ```
//!
//! The two scaling rules are the Qt ones, and they matter for the picture:
//!
//! * a block's **RMS** is scaled ×5 and clamped to 1.0
//!   (`min(1.0, rms[0] * 5)` in `metering.py`) — an RMS of 0.2 already pins the
//!   VU, which is why the Python meters look "hot";
//! * the **peak** is kept alongside (it is what catches a transient the RMS
//!   averages away) and is what [`MeterSample::peak_level`] exposes.
//!
//! Everything except [`MeterBoard`] is a pure function of its inputs, so the
//! DSP is verified with synthetic buffers (sine, silence, clipped saturation)
//! on a machine with no audio server at all.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Meter refresh cadence: 40 ms = 25 Hz (`METER_POLL_INTERVAL_MS` in Qt).
pub const METER_INTERVAL: Duration = Duration::from_millis(40);

/// How much a level keeps after one [`METER_INTERVAL`] of silence
/// (`level = max(new, prev * DECAY)`), so the bars slide down instead of
/// snapping to zero.
pub const DECAY_PER_INTERVAL: f32 = 0.85;

/// Frames per published block in the capture path: 480 frames at 48 kHz = 10 ms.
pub const BLOCK_FRAMES: usize = 480;

/// RMS scaling of the Python engine (`min(1.0, rms * 5)`).
pub const RMS_TO_LEVEL: f32 = 5.0;

/// Channels every capture stream is opened with (`pw-cat --channels 2`).
pub const METER_CHANNELS: usize = 2;

/// Sample rate the capture streams are opened with.
pub const METER_RATE: u32 = 48_000;

/// Upper bound of strips the board can serve *at once*.  Slots are assigned by
/// the engine thread and are only ever reused after a fresh assignment, so a
/// larger mixer simply recycles slots on every snapshot.
pub const MAX_METER_SLOTS: usize = 64;

/// One block's metering result.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterSample {
    /// Root-mean-square per channel, linear (0.0–1.0 for full-scale noise-free
    /// material; a sine at -6 dBFS gives ≈ 0.35).
    pub rms: [f32; 2],
    /// Absolute peak per channel, linear.
    pub peak: [f32; 2],
}

impl MeterSample {
    /// The level a VU bar shows: the Qt rule — RMS × 5, clipped to 1.0.
    pub fn level(&self) -> [f32; 2] {
        [
            (self.rms[0] * RMS_TO_LEVEL).min(1.0),
            (self.rms[1] * RMS_TO_LEVEL).min(1.0),
        ]
    }

    /// The same rule applied to the peak (transient-accurate).
    pub fn peak_level(&self) -> [f32; 2] {
        [
            (self.peak[0] * RMS_TO_LEVEL).min(1.0),
            (self.peak[1] * RMS_TO_LEVEL).min(1.0),
        ]
    }

    pub fn is_silent(&self) -> bool {
        self.peak == [0.0, 0.0]
    }
}

/// RMS + peak of an interleaved stereo f32 block.
///
/// A mono block (`frames.len()` not divisible by `METER_CHANNELS`) is treated as
/// duplicated channels, exactly like `metering.py` does when PortAudio hands it
/// one channel; an empty block is silence.
pub fn analyse_block(frames: &[f32], channels: usize) -> MeterSample {
    let channels = channels.max(1);
    if frames.is_empty() {
        return MeterSample::default();
    }
    let channel_count = if frames.len().is_multiple_of(channels) {
        channels
    } else {
        1
    };
    let mut sum_sq = [0.0_f64; METER_CHANNELS];
    let mut peak = [0.0_f32; METER_CHANNELS];
    let mut counts = [0_usize; METER_CHANNELS];
    for (index, sample) in frames.iter().enumerate() {
        let channel = index % channel_count;
        let sample = if sample.is_finite() { *sample } else { 0.0 };
        sum_sq[channel] += f64::from(sample) * f64::from(sample);
        peak[channel] = peak[channel].max(sample.abs());
        counts[channel] += 1;
    }
    let mut rms = [0.0_f32; METER_CHANNELS];
    for channel in 0..METER_CHANNELS {
        rms[channel] = if counts[channel] == 0 {
            0.0
        } else {
            (sum_sq[channel] / counts[channel] as f64).sqrt() as f32
        };
    }
    if channel_count == 1 {
        // Mono source: both bars show the same channel (no fake stereo).
        rms[METER_CHANNELS - 1] = rms[0];
        peak[METER_CHANNELS - 1] = peak[0];
    }
    MeterSample { rms, peak }
}

/// Per-strip decay envelope.  `level = max(new, prev * 0.85^(dt / 40 ms))`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Envelope {
    level: [f32; 2],
}

impl Envelope {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one sample in and get the displayed level after `dt`.
    pub fn update(&mut self, new: [f32; 2], dt: Duration) -> [f32; 2] {
        let factor = decay_factor(dt);
        for channel in 0..2 {
            let new = sanitize(new[channel]);
            let previous = self.level[channel] * factor;
            self.level[channel] = new.max(previous).clamp(0.0, 1.0);
        }
        self.level
    }

    /// Force the displayed level (a test, a demo animation, an explicit mute —
    /// no decay involved).
    pub fn set(&mut self, levels: [f32; 2]) {
        self.level = [sanitize(levels[0]), sanitize(levels[1])];
    }

    /// Current displayed level (no update).
    pub fn level(&self) -> [f32; 2] {
        self.level
    }

    /// Forget everything (mute, deleted strip, engine gone) — the next frame is
    /// dark instead of showing a frozen bar.
    pub fn reset(&mut self) {
        self.level = [0.0, 0.0];
    }
}

/// `0.85^(dt / 40 ms)`, i.e. exactly [`DECAY_PER_INTERVAL`] per meter tick but
/// frame-rate independent (a 60 fps UI decays 1.5× less per frame than a 25 fps
/// one, for the same wall-clock fall-off).
pub fn decay_factor(dt: Duration) -> f32 {
    let ticks = dt.as_secs_f32() / METER_INTERVAL.as_secs_f32();
    if !ticks.is_finite() || ticks <= 0.0 {
        return 1.0;
    }
    DECAY_PER_INTERVAL.powf(ticks).clamp(0.0, 1.0)
}

fn sanitize(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The lock-free level board shared by the engine thread (writer) and the UI
/// thread (reader).
///
/// `values` is a fixed `Arc<[AtomicU32]>` of `MAX_METER_SLOTS × 2` f32 bits: a
/// read is one relaxed load per channel and never blocks the audio path, which
/// is what makes a 25 Hz UI poll safe while capture threads publish at 100 Hz.
/// The uid → slot map is behind a `Mutex` because strips are added and removed
/// (a snapshot reassigns the whole map); the audio side never takes it after a
/// capture thread was spawned — the slot index is handed to the thread once.
#[derive(Clone)]
pub struct MeterBoard {
    values: Arc<[AtomicU32]>,
    slots: Arc<Mutex<HashMap<String, usize>>>,
}

impl Default for MeterBoard {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MeterBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeterBoard")
            .field("slots", &self.slots.lock().map(|m| m.len()).unwrap_or(0))
            .finish()
    }
}

impl MeterBoard {
    pub fn new() -> Self {
        let values: Arc<[AtomicU32]> = (0..MAX_METER_SLOTS * 2)
            .map(|_| AtomicU32::new(0))
            .collect();
        Self {
            values,
            slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Assign slots to `uids` in order (engine side, after a snapshot).
    /// Returns the mapping so the caller can hand each capture thread its slot.
    /// Every previously assigned slot is zeroed first, so a removed strip never
    /// keeps a frozen level.
    pub fn assign(&self, uids: &[String]) -> HashMap<String, usize> {
        let mut mapping = HashMap::with_capacity(uids.len());
        for (slot, uid) in uids.iter().take(MAX_METER_SLOTS).enumerate() {
            mapping.insert(uid.clone(), slot);
            self.publish(slot, [0.0, 0.0]);
        }
        if uids.len() > MAX_METER_SLOTS {
            log::warn!(
                "metering: {} strips, only {MAX_METER_SLOTS} meter slots — the rest stays dark",
                uids.len()
            );
        }
        if let Ok(mut slots) = self.slots.lock() {
            *slots = mapping.clone();
        }
        mapping
    }

    /// Publish one slot's `[left, right]` level (0.0–1.0).
    pub fn publish(&self, slot: usize, levels: [f32; 2]) {
        let base = slot.saturating_mul(2);
        if base + 1 >= self.values.len() {
            return;
        }
        self.values[base].store(sanitize(levels[0]).to_bits(), Ordering::Relaxed);
        self.values[base + 1].store(sanitize(levels[1]).to_bits(), Ordering::Relaxed);
    }

    /// Current level of a strip, `None` when the board never had one.
    pub fn read(&self, uid: &str) -> Option<[f32; 2]> {
        let slot = *self.slots.lock().ok()?.get(uid)?;
        let base = slot.saturating_mul(2);
        if base + 1 >= self.values.len() {
            return None;
        }
        Some([
            f32::from_bits(self.values[base].load(Ordering::Relaxed)),
            f32::from_bits(self.values[base + 1].load(Ordering::Relaxed)),
        ])
    }

    /// Current level of a slot, whatever uid it belongs to (capture threads and
    /// tests use this form).
    pub fn read_slot(&self, slot: usize) -> [f32; 2] {
        let base = slot.saturating_mul(2);
        if base + 1 >= self.values.len() {
            return [0.0, 0.0];
        }
        [
            f32::from_bits(self.values[base].load(Ordering::Relaxed)),
            f32::from_bits(self.values[base + 1].load(Ordering::Relaxed)),
        ]
    }

    pub fn slots(&self) -> HashMap<String, usize> {
        self.slots
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full-scale sine: RMS = 1/√2.
    fn sine(frames: usize, amplitude: f32, frequency: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            let sample = amplitude
                * (2.0 * std::f32::consts::PI * frequency * n as f32 / METER_RATE as f32).sin();
            out.push(sample);
            out.push(sample);
        }
        out
    }

    #[test]
    fn rms_of_a_full_scale_sine_is_the_textbook_value() {
        let block = sine(METER_RATE as usize / 10, 1.0, 1000.0);
        let sample = analyse_block(&block, 2);
        let expected = std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (sample.rms[0] - expected).abs() < 1e-3,
            "rms {} vs {expected}",
            sample.rms[0]
        );
        assert!((sample.rms[1] - expected).abs() < 1e-3);
        // 1/√2 × 5 clipped to 1.0 → the Qt meter is pinned.
        assert_eq!(sample.level(), [1.0, 1.0]);
        assert!((sample.peak[0] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn a_quiet_sine_is_not_pinned() {
        // 0.02 amplitude → RMS ≈ 0.0141 → level ≈ 0.071.
        let block = sine(METER_RATE as usize / 10, 0.02, 1000.0);
        let sample = analyse_block(&block, 2);
        assert!(sample.level()[0] < 0.1, "{:?}", sample.level());
        assert!(sample.level()[0] > 0.0);
    }

    #[test]
    fn silence_is_silent_and_empty_blocks_do_not_panic() {
        assert!(analyse_block(&[], 2).is_silent());
        assert!(analyse_block(&[0.0; 960], 2).is_silent());
        let mut envelope = Envelope::new();
        assert_eq!(envelope.update([0.0, 0.0], METER_INTERVAL), [0.0, 0.0]);
    }

    #[test]
    fn saturation_is_clamped_not_wrapped() {
        // Clipped material: peaks at ±1.0, RMS 1.0 → level exactly 1.0.
        let block = vec![1.0_f32, 1.0, -1.0, -1.0, 3.5, -9.0];
        let sample = analyse_block(&block, 2);
        assert_eq!(sample.level(), [1.0, 1.0]);
        assert_eq!(sample.peak_level(), [1.0, 1.0]);
        // Non-finite samples are dropped (a NaN must never poison the meter).
        let nan = analyse_block(&[f32::NAN, f32::INFINITY, 0.5, 0.5], 2);
        assert!(nan.level()[0].is_finite() && nan.level()[1].is_finite());
        assert!((nan.peak[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_mono_block_feeds_both_channels() {
        let mono = vec![0.5_f32; 480];
        let sample = analyse_block(&mono, 2);
        assert!((sample.rms[0] - 0.5).abs() < 1e-6);
        assert_eq!(sample.rms[0], sample.rms[1]);
    }

    #[test]
    fn the_envelope_decays_by_fifteen_percent_per_meter_tick() {
        let mut envelope = Envelope::new();
        assert_eq!(envelope.update([1.0, 0.5], Duration::ZERO), [1.0, 0.5]);
        let after = envelope.update([0.0, 0.0], METER_INTERVAL);
        assert!((after[0] - DECAY_PER_INTERVAL).abs() < 1e-6, "{after:?}");
        assert!((after[1] - 0.5 * DECAY_PER_INTERVAL).abs() < 1e-6);
        // Two ticks: 0.7225.
        let two = envelope.update([0.0, 0.0], METER_INTERVAL);
        assert!((two[0] - 0.85 * 0.85).abs() < 1e-6, "{two:?}");
        // A new peak wins immediately (attack is instantaneous).
        assert_eq!(envelope.update([0.9, 0.9], METER_INTERVAL), [0.9, 0.9]);
    }

    #[test]
    fn the_envelope_is_frame_rate_independent() {
        // 2 × 20 ms must decay like 1 × 40 ms.
        let mut split = Envelope::new();
        split.update([1.0, 1.0], Duration::ZERO);
        split.update([0.0, 0.0], Duration::from_millis(20));
        let split_level = split.update([0.0, 0.0], Duration::from_millis(20));
        let mut whole = Envelope::new();
        whole.update([1.0, 1.0], Duration::ZERO);
        let whole_level = whole.update([0.0, 0.0], Duration::from_millis(40));
        assert!((split_level[0] - whole_level[0]).abs() < 1e-5);
        assert!((split_level[0] - DECAY_PER_INTERVAL).abs() < 1e-5);
        // It keeps falling towards zero, never below.
        let mut envelope = Envelope::new();
        envelope.update([1.0, 1.0], Duration::ZERO);
        for _ in 0..200 {
            envelope.update([0.0, 0.0], METER_INTERVAL);
        }
        assert!(envelope.level()[0] < 1e-6);
        assert!(envelope.level()[0] >= 0.0);
    }

    #[test]
    fn the_board_maps_uid_to_level_and_back() {
        let board = MeterBoard::new();
        let uids = vec!["in-mic".to_string(), "out-hp".to_string()];
        let slots = board.assign(&uids);
        assert_eq!(slots["in-mic"], 0);
        assert_eq!(slots["out-hp"], 1);
        board.publish(slots["in-mic"], [0.4, 0.6]);
        assert_eq!(board.read("in-mic"), Some([0.4, 0.6]));
        assert_eq!(board.read_slot(0), [0.4, 0.6]);
        assert_eq!(board.read("out-hp"), Some([0.0, 0.0]));
        assert_eq!(board.read("ghost"), None);
        // Out-of-range input is clamped, never stored as garbage.
        board.publish(1, [f32::NAN, 9.0]);
        assert_eq!(board.read("out-hp"), Some([0.0, 1.0]));
        // A new assignment resets the previous levels (a removed strip must not
        // keep a frozen bar).
        board.assign(&["out-hp".to_string()]);
        assert_eq!(board.read("in-mic"), None);
        assert_eq!(board.read("out-hp"), Some([0.0, 0.0]));
        // Slot overflow is ignored instead of panicking.
        board.publish(MAX_METER_SLOTS + 5, [0.5, 0.5]);
        assert_eq!(board.read_slot(MAX_METER_SLOTS + 5), [0.0, 0.0]);
    }

    #[test]
    fn the_board_shares_levels_across_threads() {
        let board = MeterBoard::new();
        let writer = board.clone();
        let handle = std::thread::spawn(move || {
            let slots = writer.assign(&["in-mic".to_string()]);
            for step in 0..50 {
                writer.publish(slots["in-mic"], [step as f32 / 100.0, 0.5]);
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        handle.join().unwrap();
        assert_eq!(board.read("in-mic"), Some([0.49, 0.5]));
    }

    #[test]
    fn block_constants_match_the_python_meter() {
        assert_eq!(METER_INTERVAL, Duration::from_millis(40));
        assert_eq!(BLOCK_FRAMES, 480);
        assert_eq!(METER_CHANNELS, 2);
        assert_eq!(METER_RATE, 48_000);
        // 480 frames × 2 channels × 4 bytes = one 3840-byte read.
        assert_eq!(BLOCK_FRAMES * METER_CHANNELS * 4, 3840);
    }
}
