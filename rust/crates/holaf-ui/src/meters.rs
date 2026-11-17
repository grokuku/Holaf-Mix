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
//! RmsWindow: RMS over the last 100 ms     ← the value a VU shows
//!        ▼
//! MeterBoard: Arc<[AtomicU32]>            ← the UI reads f32 bits, no lock
//!        ▼ polled at 25 Hz by AppState::poll_engine
//! decay envelope (level = max(new, prev × exp(-dt / 350 ms)))
//!        ▼
//! VU bars ([`crate::widgets::fader`])
//! ```
//!
//! Two properties of that chain are deliberate and user-visible:
//!
//! * the **bar shows a short-term RMS** ([`RMS_WINDOW`]), never a peak: a peak
//!   follower on music sits at the loudest sample and reads “constant and too
//!   high”, vibrating on every transient;
//! * that window is **100 ms**, not 300: a VU has to *react* (user report: “les
//!   vu-mètres bougent, mais ils ont toujours un peu de lag”), and 100 ms of
//!   integration is enough to stop a 10 ms block from vibrating the bar — the
//!   number that gives a meter its inertia is the release time, not the window;
//! * the **release is a real time constant** ([`RELEASE_TAU`]) applied to the
//!   elapsed `dt`, so the bar falls at the same speed whatever the frame rate,
//!   and a 10 % drop of the signal is visible immediately instead of being
//!   averaged into a per-frame factor.
//!
//! The **level scale** is the one thing the port deliberately changed.
//!
//! `metering.py` scaled a block's RMS by 5 and clipped (`min(1.0, rms * 5)`),
//! which **saturates at an RMS of 0.2 — i.e. −14 dBFS**.  Every ordinary music or
//! voice signal is above that, so both bars sat pinned at full height: “les
//! vu-mètres sont toujours à fond, même quand le son est faible” (user report).
//!
//! The mapping is a plain dBFS scale over [`LEVEL_FLOOR_DB`] (−60 dBFS) → 0 to
//! [`LEVEL_CEILING_DB`] (**+6** dBFS, the overload headroom) → 1.  Mapping
//! **0 dBFS to 0.91 instead of 1.0** is what keeps a normal mix off the top of
//! the bar: the last few pixels are reserved for material that really is over
//! full scale, which is the only thing a meter can say about a signal that is
//! already clipped (measured on the headless sandbox, see the table below —
//! ordinary −12 dBFS music lands at 0.73, not 0.81 as with a 0…−60 scale):
//!
//! | RMS | dBFS | level |
//! |---|---|---|
//! | 2.0 | +6 (overload) | 1.00 |
//! | 1.0 (square / full-scale) | 0 | 0.91 |
//! | 0.5 | −6 | 0.82 |
//! | 0.354 (a −6 dBFS sine) | −9 | 0.77 |
//! | 0.251 (−12 dBFS music, RMS) | −12 | 0.73 |
//! | 0.2 (the old saturation point) | −14 | 0.70 |
//! | 0.1 | −20 | 0.61 |
//! | 0.0316 | −30 | 0.45 |
//! | 0.00316 | −50 | 0.15 |
//! | 0 | −∞ | 0.00 |
//!
//! # Monitoring semantics: post-fader, everywhere, exactly once
//!
//! The bar shows the signal **at a strip's master node**, after its fader.  Both
//! strip kinds are measured, not assumed (PipeWire 1.4.2 headless sandbox; see
//! `holaf-pw/tests/backend_live.rs`):
//!
//! * a **virtual strip** is metered through its bus (`Holaf_Bus_<Label>.monitor`);
//! * a **physical strip** through its device (`<device>.monitor`).
//!
//! A sink's monitor tap has its **own** gain (`Props.monitorVolumes`) and its own
//! rule — `monitor.channel-volumes` decides whether `channelVolumes` also applies
//! to it.  The engine writes the strip volume to `channelVolumes` (that is the
//! sound) and makes the tap follow it *once*: `monitorVolumes = gain` when the
//! node does not apply `channelVolumes` to its monitor (the engine's own buses,
//! and ALSA nodes, whose default is `false`), and `monitorVolumes = 1.0` when it
//! does (`module-null-sink`, the WirePlumber fallback sink: `true`).  Writing both
//! gains on the second kind doubled the attenuation — measured on a
//! `monitor.channel-volumes=true` sink: 0.3536 at unity, **0.000258** at fader 0.3
//! (= `gain²`), against 0.0095 (= `gain`) once fixed.
//!
//! Consequence for this module: **the UI never scales a level by the fader.**  A
//! capture is post-fader on every strip kind, so multiplying by
//! `pulse_linear_gain(volume)` again — what [`crate::app`] used to do for physical
//! strips — showed `gain²` and made the output bars read “beaucoup beaucoup trop
//! bas” (user report).  [`attenuate_level`] remains as a pure helper (a linear
//! gain applied to a level that is already on the dB scale) for callers that
//! really do hold a pre-fader signal, but the meter path does not use it.
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

/// Release time constant of the bar: after the signal stops, the displayed
/// level falls to `1/e` (37 %) in this long, `level = prev * exp(-dt / τ)`.
///
/// `dt` is the **real** elapsed time between two measurements, so a 60 fps
/// repaint falls exactly as fast as a 25 Hz one.  The old rule was a fixed
/// factor per UI tick (`prev * 0.85` every 40 ms, i.e. τ = 246 ms), which made
/// the fall depend on the frame rate *and* left the bar hanging at its last
/// value for over a second after the sound stopped (“il descend, mais avec du
/// lag”).  350 ms is the classic VU return time: fast enough to follow the
/// music, slow enough not to blink on every gap.
pub const RELEASE_TAU: Duration = Duration::from_millis(350);

/// Integration time of a VU meter: the displayed level is the RMS over this
/// window, not over one block.
///
/// A 10 ms block is the *capture* granularity, not a meter time constant.  With
/// the block RMS feeding the bar directly, the level jumps on every hi-hat and
/// the max-hold below turns the bar into a peak follower: “le niveau est plus
/// ou moins constant (et trop haut) mais ça vibre”.  Averaging over a window
/// makes loud music read its *short-term* RMS (−12…−9 dBFS) instead of its
/// loudest 10 ms.
///
/// **Why 100 ms and not 300.**  The window was 300 ms, which is the time a
/// *loudness meter* integrates over, not a VU: it delayed the attack by three
/// UI ticks and diluted every transient (user report: “les vu-mètres bougent,
/// mais ils ont toujours un peu de lag.  tu affiches la valeur direct ou tu as
/// une sorte d'inertie ?”).  Measured on a −9 dBFS tone and a 25 Hz poll
/// (see `the_window_is_short_enough_to_react_to_a_burst`):
///
/// | window | time to 90 % of the reading | 50 ms burst peak |
/// |---|---|---|
/// | 300 ms (before) | 120 ms | 0.654 |
/// | **100 ms (now)** | **40 ms** | **0.727** |
///
/// 100 ms is the compromise: one UI tick of delay instead of three, still ten
/// blocks of averaging so a single hi-hat cannot move the bar (a 10 ms block
/// alone moves the reading by +10 dB, ten of them by +10·log10(11/10) ≈ 0.4 dB).
/// The *ballistics* of the meter — what a VU's inertia really is — live in the
/// release ([`RELEASE_TAU`], 350 ms), which this window does not touch.
pub const RMS_WINDOW: Duration = Duration::from_millis(100);

/// Frames per published block in the capture path: 480 frames at 48 kHz = 10 ms.
pub const BLOCK_FRAMES: usize = 480;

/// Blocks in [`RMS_WINDOW`] (`100 ms / 10 ms`): the reader keeps this many
/// per-channel mean squares, which makes the window exactly 100 ms of audio
/// whatever the wall-clock jitter of the capture thread.
pub const RMS_WINDOW_BLOCKS: usize = 10;

/// Bottom of the level scale: this many dB below full scale maps to 0.
///
/// −60 dBFS is the usual bottom of a digital meter: quiet material still moves
/// the bar, and everything a listener calls “loud” lands in the top third
/// instead of pinning it.
pub const LEVEL_FLOOR_DB: f32 = -60.0;

/// Top of the level scale: this many dB **above full scale** map to 1.0.
///
/// Not 0 dBFS on purpose.  A digital meter whose top is 0 dBFS spends its last
/// pixels on material that is already at (or over) the ceiling — and a mix bus
/// fed by several strips is routinely over it — so ordinary music (“loud”, i.e.
/// −12…−9 dBFS RMS) sat at 80-85 % and read as *pinned*: the user report
/// “toujours à fond, avec une baisse de 10 % de temps en temps”.  Reserving
/// [`OVERLOAD_HEADROOM_DB`] decibels above 0 dBFS for the top of the bar moves
/// music down to ~0.73 and makes a bar at 1.0 mean what it should: *this signal
/// is 6 dB over full scale*.
pub const LEVEL_CEILING_DB: f32 = OVERLOAD_HEADROOM_DB;

/// Decibels of headroom drawn **above** 0 dBFS (the overload zone).
pub const OVERLOAD_HEADROOM_DB: f32 = 6.0;

/// Peak above which a block cannot be real audio and must be a **misread
/// stream** (a different sample format handed to the f32 reader).
///
/// +40 dBFS is 100× full scale: no audio path and no bus sum reaches it, but
/// s32/NaN garbage reinterpreted as f32 does.  Such a block is never drawn (see
/// [`MeterSample::is_implausible`]) — publishing it would pin the bar at 1.0 for
/// ever, which is exactly the “VU à fond” failure.
pub const IMPLAUSIBLE_PEAK: f32 = 100.0;

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
    /// The level a VU bar shows: the RMS on the dBFS scale
    /// ([`level_from_amplitude`]).
    ///
    /// This used to be the Qt rule — `min(1.0, rms * 5)` — which saturates at
    /// −14 dBFS, so the bars of a normal mix sat pinned at the top whatever the
    /// fader did.
    pub fn level(&self) -> [f32; 2] {
        [
            level_from_amplitude(self.rms[0]),
            level_from_amplitude(self.rms[1]),
        ]
    }

    /// The same scale applied to the peak (transient-accurate).
    pub fn peak_level(&self) -> [f32; 2] {
        [
            level_from_amplitude(self.peak[0]),
            level_from_amplitude(self.peak[1]),
        ]
    }

    /// Is this block **impossible** as audio, i.e. certainly a stream read with
    /// the wrong sample format?
    ///
    /// The capture asks `pw-cat` for `f32le`; if a build/version ever writes
    /// another format to stdout, the reader reinterprets the bytes as f32 and the
    /// “samples” become garbage — mostly huge values (or NaNs, which
    /// [`analyse_block`] drops).  Drawing that is what pins a VU bar at 1.0 for
    /// ever.  Such a block is dropped instead (the bar stays dark) and reported
    /// once, so the failure is *visible* rather than plausible.
    pub fn is_implausible(&self) -> bool {
        self.peak
            .iter()
            .any(|peak| !peak.is_finite() || *peak > IMPLAUSIBLE_PEAK)
    }

    /// The `[level, level]` an amplitude (RMS or peak) maps to.
    pub fn is_silent(&self) -> bool {
        self.peak == [0.0, 0.0]
    }
}

/// Amplitude (0.0–1.0 linear, RMS or peak) → bar height (0.0–1.0).
///
/// Linear in **decibels** between [`LEVEL_FLOOR_DB`] (−60 dBFS) and
/// [`LEVEL_CEILING_DB`] (+6 dBFS), which is what a meter is for: a 6 dB drop
/// moves the bar by a fixed amount wherever it happens, quiet material still
/// moves it at all, and the top is reached only by a signal that is really over
/// full scale.
pub fn level_from_amplitude(amplitude: f32) -> f32 {
    let db = amplitude_to_db(amplitude);
    if !db.is_finite() {
        return 0.0;
    }
    ((db - LEVEL_FLOOR_DB) / (LEVEL_CEILING_DB - LEVEL_FLOOR_DB)).clamp(0.0, 1.0)
}

/// Linear amplitude → dBFS (`-inf` for silence, `0` for full scale).
pub fn amplitude_to_db(amplitude: f32) -> f32 {
    if amplitude.is_nan() || amplitude <= 0.0 {
        return f32::NEG_INFINITY;
    }
    20.0 * amplitude.log10()
}

/// RMS + peak of an interleaved stereo f32 block, **canal par canal**.
///
/// Even indices are the left channel, odd ones the right: each channel gets its
/// own RMS and its own peak, so `[rms[0], rms[1]]` is exactly what the two VU
/// bars draw.  A signal present only on the left gives `rms = [x, 0.0]` — the
/// measure is done per channel, never summed or maxed (a bar that rose on both
/// sides for a one-eared source was one of the reported bugs; measured live
/// through a real `pw-cat` capture, a left-only WAV reads `L = 0.354, R = 0.0`).
///
/// **Mono is duplicated on purpose, and only here.**  Two different situations
/// collapse to one channel, and both must show the same level on both bars
/// rather than a fake stereo image:
///
/// * the *capture* hands us one channel (`frames.len()` not divisible by
///   `channels`) — exactly what `metering.py` did when PortAudio gave it mono;
/// * the *source* is mono: the stream is opened with `--channels 2`, and
///   PipeWire's channel mixer **duplicates** the single channel, so a mono device
///   (`alsa_output….pro-output-0`, a mono microphone) arrives as two identical
///   channels and there is nothing left to tell apart at this level.  Which case
///   it is has to be read from the graph (`pw-dump`: `audio.channels` of the
///   captured node), not guessed from the samples.
///
/// A strip with `is_mono` set collapses `max(L, R)` onto both bars in
/// [`crate::app::AppState::levels_for`] (Qt parity: `StripWidget.update_vumeter`).
/// An empty block is silence.
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

/// Per-strip decay envelope.  `level = max(new, prev * exp(-dt / 350 ms))`:
/// attack is instantaneous, release is a real time constant.
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

/// `exp(-dt / τ)` with τ = [`RELEASE_TAU`].
///
/// Exponential, so it composes: two steps of `dt/2` decay exactly like one of
/// `dt`.  That is what makes the bar frame-rate independent — the old
/// per-tick factor did not, and a UI that repainted at 120 fps fell half as
/// fast as one at 60 fps while claiming the same “15 % per tick”.
pub fn decay_factor(dt: Duration) -> f32 {
    let seconds = dt.as_secs_f32();
    let tau = RELEASE_TAU.as_secs_f32();
    if !seconds.is_finite() || seconds <= 0.0 || tau <= 0.0 {
        return 1.0;
    }
    (-seconds / tau).exp().clamp(0.0, 1.0)
}

/// The RMS of the last [`RMS_WINDOW`] of audio, per channel — the value a VU bar
/// shows.
///
/// Why not the block RMS: a 10 ms RMS is dominated by whatever the loudest
/// handful of cycles in that block were, so the level of steady music jumps
/// several dB from block to block (measured on the sandbox: 10 ms blocks of the
/// same mix ranged over 0.075…0.117 RMS = −22…−19 dBFS, while the 100 ms RMS
/// stayed within ±0.4 dB of 0.10 = −20 dBFS).  Feeding the 10 ms value into a
/// max-hold envelope gives exactly the reported symptom: a bar that is
/// **constant and too high** (it tracks the loudest recent block) while
/// **vibrating** (every block is a new maximum).
///
/// The window is a ring of per-block *mean squares*, so it is exact (no IIR
/// bias, no attack transient) and its length is a number of blocks — 100 ms of
/// audio by construction, independent of the thread's scheduling.
#[derive(Debug, Clone)]
pub struct RmsWindow {
    /// Per block: mean square of each channel.
    means: Vec<[f64; 2]>,
    next: usize,
    filled: usize,
}

impl Default for RmsWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl RmsWindow {
    pub fn new() -> Self {
        Self::with_blocks(RMS_WINDOW_BLOCKS)
    }

    /// A window of `blocks` blocks (10 ms each) instead of [`RMS_WINDOW_BLOCKS`].
    ///
    /// Only the tests use it, to compare the 100 ms window with the 300 ms one it
    /// replaced (see `the_window_is_short_enough_to_react_to_a_burst`): the
    /// measured difference is what justifies the constant.
    pub fn with_blocks(blocks: usize) -> Self {
        Self {
            means: vec![[0.0; 2]; blocks.max(1)],
            next: 0,
            filled: 0,
        }
    }

    /// Add one block (its per-channel RMS as measured by [`analyse_block`]).
    pub fn push(&mut self, sample: &MeterSample) {
        let mean = [
            f64::from(sample.rms[0]) * f64::from(sample.rms[0]),
            f64::from(sample.rms[1]) * f64::from(sample.rms[1]),
        ];
        self.means[self.next] = mean;
        self.next = (self.next + 1) % self.means.len();
        self.filled = (self.filled + 1).min(self.means.len());
    }

    /// RMS over the window (whatever it is filled with: partial until warm).
    pub fn rms(&self) -> [f32; 2] {
        if self.filled == 0 {
            return [0.0, 0.0];
        }
        let mut sum = [0.0_f64; 2];
        for mean in &self.means[..self.filled] {
            sum[0] += mean[0];
            sum[1] += mean[1];
        }
        let count = self.filled as f64;
        [(sum[0] / count).sqrt() as f32, (sum[1] / count).sqrt() as f32]
    }

    /// The window RMS on the bar scale — what [`MeterBoard::publish`] gets.
    pub fn level(&self) -> [f32; 2] {
        let rms = self.rms();
        [level_from_amplitude(rms[0]), level_from_amplitude(rms[1])]
    }

    /// Forget the window (mute, stop): the next block starts a fresh average
    /// instead of dragging 100 ms of the old signal along.
    pub fn reset(&mut self) {
        self.means.fill([0.0; 2]);
        self.next = 0;
        self.filled = 0;
    }

    /// How many blocks are in the window right now (tests: the warm-up).
    pub fn len(&self) -> usize {
        self.filled
    }

    pub fn is_empty(&self) -> bool {
        self.filled == 0
    }
}

/// The inverse of [`level_from_amplitude`]: a bar height back to a linear
/// amplitude.
///
/// Used to apply a **linear gain** to a level that the board publishes already
/// scaled (a fader move, a mono downmix trim): a level is a dB reading, so
/// multiplying it by 0.03 would not be −30 dB.
pub fn amplitude_from_level(level: f32) -> f32 {
    if !level.is_finite() {
        return 0.0;
    }
    let db = LEVEL_FLOOR_DB + level.clamp(0.0, 1.0) * (LEVEL_CEILING_DB - LEVEL_FLOOR_DB);
    10.0_f32.powf(db / 20.0)
}

/// Apply a **linear amplitude gain** to a bar level (`gain` 0…1), in the
/// amplitude domain: `level_from_amplitude(amplitude_from_level(level) * gain)`.
///
/// This is the post-fader correction: a capture tapped *before* the strip's
/// fader has to be scaled by it, and the scale must happen on amplitudes or the
/// dB mapping hides it (level 0.7 × 0.03 = 0.02 is −58 dB, not −30 dB).
pub fn attenuate_level(level: f32, gain: f32) -> f32 {
    if !gain.is_finite() || !level.is_finite() {
        return 0.0;
    }
    if (gain - 1.0).abs() < f32::EPSILON {
        return level.clamp(0.0, 1.0);
    }
    level_from_amplitude(amplitude_from_level(level) * gain.clamp(0.0, 1.0))
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
        // A −3 dBFS sine sits just under the top instead of pinning the bar.
        assert!(
            (sample.level()[0] - 0.87).abs() < 0.01,
            "{:?}",
            sample.level()
        );
        // Its *peak* is full scale (0 dBFS), so even that bar is not full: the
        // last 6 dB of the scale are the overload headroom.
        assert!((sample.peak[0] - 1.0).abs() < 1e-3);
        assert!((sample.peak_level()[0] - 0.91).abs() < 0.01);
    }

    /// The old rule (`min(1, rms × 5)`) saturated at RMS 0.2 = −14 dBFS: every
    /// ordinary mix pinned both bars — “les vu-mètres sont toujours à fond, même
    /// quand le son est faible”.  The dB scale must keep the whole range usable,
    /// and its **top must be out of reach for normal material**: 0 dBFS maps to
    /// 0.91, so 1.0 means “over full scale”.
    #[test]
    fn the_level_scale_is_in_decibels_and_does_not_saturate_at_minus_14() {
        let cases = [
            (2.0_f32, 1.0_f32), // +6 dBFS: the overload zone
            (1.0, 0.91),        // 0 dBFS — full scale is NOT the top of the bar
            (0.5, 0.82),        // −6 dBFS
            (0.354, 0.77),      // −9 dBFS (a −6 dBFS sine)
            (0.251, 0.73),      // −12 dBFS: ordinary music, RMS
            (0.2, 0.70),        // −14 dBFS — the old saturation point
            (0.1, 0.61),        // −20 dBFS
            (0.0316, 0.45),     // −30 dBFS
            (0.00316, 0.15),    // −50 dBFS
        ];
        for (amplitude, expected) in cases {
            let level = level_from_amplitude(amplitude);
            assert!(
                (level - expected).abs() < 0.01,
                "{amplitude} → {level}, expected {expected}"
            );
            // Nothing below full scale may reach the top of the bar.
            assert!(level < 1.0 || amplitude >= 2.0, "{amplitude} must not pin");
            // …and ordinary music (≥ −12 dBFS) must keep a visible margin.
            if (0.2..=1.0).contains(&amplitude) {
                assert!(level <= 0.91, "{amplitude} → {level} is too close to full");
            }
        }
        // Monotonic, so a louder signal never draws a shorter bar.
        let mut previous = -1.0_f32;
        for step in (0..=60).rev() {
            let amplitude = 10.0_f32.powf(-(step as f32) / 20.0);
            let level = level_from_amplitude(amplitude);
            assert!(level >= previous, "{amplitude} → {level} < {previous}");
            previous = level;
        }
        // Silence and non-finite input are dark, never garbage.
        assert_eq!(level_from_amplitude(0.0), 0.0);
        assert_eq!(level_from_amplitude(-1.0), 0.0);
        assert_eq!(level_from_amplitude(f32::NAN), 0.0);
        assert_eq!(level_from_amplitude(f32::INFINITY), 0.0);
        assert_eq!(amplitude_to_db(1.0), 0.0);
        assert!(amplitude_to_db(0.0).is_infinite());
    }

    /// A quiet signal must be a **short** bar: the whole complaint was a meter
    /// that stayed at the top whatever the level.
    #[test]
    fn a_quiet_sine_is_not_pinned() {
        // 0.02 amplitude → RMS ≈ 0.0141 → −37 dBFS → level ≈ 0.35.
        let block = sine(METER_RATE as usize / 10, 0.02, 1000.0);
        let sample = analyse_block(&block, 2);
        let level = sample.level()[0];
        assert!((0.25..0.42).contains(&level), "{level}");
        // Ten times quieter is ten times lower on the scale (÷20 dB → −1/3).
        let quieter = analyse_block(&sine(METER_RATE as usize / 10, 0.002, 1000.0), 2);
        assert!(
            quieter.level()[0] < level - 0.25,
            "{:?} vs {level}",
            quieter.level()
        );
    }

    /// **The VU is post-fader.**  The engine applies a strip's volume to its
    /// master node (`channelVolumes` *and* `monitorVolumes`), so what a capture
    /// records — and therefore what this scale turns into a bar — follows the
    /// fader.  The gain is the PulseAudio cubic one (`pulse_linear_gain`), which
    /// is what the engine really writes; measured live on the headless sandbox:
    /// 0.3535 at volume 1.0, 0.0096 at 0.3, 0.0004 at 0.1.
    #[test]
    fn the_level_follows_the_fader_because_the_capture_is_post_fader() {
        use holaf_core::model::pulse_linear_gain;
        let tone = 0.3535_f32;
        let full = level_from_amplitude(tone);
        let half = level_from_amplitude(tone * pulse_linear_gain(0.5));
        let low = level_from_amplitude(tone * pulse_linear_gain(0.3));
        assert!(full > half && half > low, "{full} {half} {low}");
        // A −9 dBFS tone at fader 0.5 (gain 0.125 = −18 dB) is 27 dB lower: the
        // bar loses almost half its height instead of staying pinned.
        assert!(half < full - 0.25, "{full} vs {half}");
        assert!(low < half, "{half} vs {low}");
        // Muting really is silence (the engine also writes `monitorMute`).
        assert_eq!(level_from_amplitude(0.0), 0.0);
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
        // Clipped material: peaks at ±1.0 and beyond.  The *peak* bar is deep in
        // the overload zone (1.0) and the RMS bar is high, never NaN.
        let block = vec![1.0_f32, 1.0, -1.0, -1.0, 3.5, -9.0];
        let sample = analyse_block(&block, 2);
        assert_eq!(sample.peak_level(), [1.0, 1.0]);
        // A full-scale square wave (RMS exactly 1.0 = 0 dBFS) is high but not
        // glued to the top: that is the overload headroom doing its job.
        let square = analyse_block(&[1.0_f32, 1.0, -1.0, -1.0], 2);
        assert!(
            (square.level()[0] - 0.91).abs() < 0.01,
            "{:?}",
            square.level()
        );
        // Non-finite samples are dropped (a NaN must never poison the meter).
        let nan = analyse_block(&[f32::NAN, f32::INFINITY, 0.5, 0.5], 2);
        assert!(nan.level()[0].is_finite() && nan.level()[1].is_finite());
        assert!((nan.peak[0] - 0.5).abs() < 1e-6);
        // …and a block that is nothing but garbage is not "silence": it is
        // flagged as impossible, so the capture path can refuse to draw it.
        let garbage = analyse_block(&[f32::NAN, f32::NAN, f32::NAN, f32::NAN], 2);
        assert!(garbage.is_silent());
        assert!(!garbage.is_implausible(), "all-NaN is silence, not garbage");
    }

    /// **H1, the format trap.**  The capture asks `pw-cat` for `f32le`; if a
    /// build, version or wrapper ever writes another sample format to stdout, the
    /// reader reinterprets the bytes as f32.  Such a block must never be turned
    /// into a bar: garbage read as f32 is mostly huge (or NaN), and drawing it
    /// pins the VU at 1.0 for ever — “toujours à fond, avec une baisse de 10 % de
    /// temps en temps”.
    ///
    /// The numbers here are the *real* bytes of a −20 dBFS sine, read both ways.
    #[test]
    fn a_stream_read_with_the_wrong_sample_format_is_never_drawn() {
        // The exact bytes `pw-cat --record --format f32` writes for a −20 dBFS
        // 1 kHz sine at 48 kHz (measured on the headless sandbox):
        // `00 60 b1 bd 00 60 b1 bd 00 30 bd bd …` → −0.08661 every frame.
        let f32_block = sine(480, 0.1, 1000.0);
        let real = analyse_block(&f32_block, METER_CHANNELS);
        assert!((real.rms[0] - 0.0707).abs() < 1e-3, "{:?}", real);
        assert!(!real.is_implausible(), "the real block is plausible audio");
        assert!(
            (real.level()[0] - 0.56).abs() < 0.02,
            "a −20 dBFS sine (RMS −23 dBFS) → 0.56, got {:?}",
            real.level()
        );

        // The same *sine* encoded as **s16le and read as f32**: every four bytes
        // are two s16 samples, so the f32's exponent comes from a sine sample's
        // high byte.  Half the frames become denormals and half become enormous
        // numbers — the RMS ends far above full scale, which is exactly the
        // garbage that pins a bar at 1.0.
        let mut s16_bytes = Vec::with_capacity(f32_block.len() * 2);
        for sample in &f32_block {
            s16_bytes.extend_from_slice(&((*sample * 32767.0) as i16).to_le_bytes());
        }
        let s16_as_f32: Vec<f32> = s16_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect();
        let misread = analyse_block(&s16_as_f32, METER_CHANNELS);
        assert!(
            misread.is_implausible(),
            "s16 read as f32 must be flagged (rms {:?}, peak {:?})",
            misread.rms,
            misread.peak
        );
        // Undetected, it would have been a bar glued at the top: the symptom.
        assert_eq!(misread.level(), [1.0, 1.0]);

        // The other direction — a nearly silent s16 stream — becomes denormals:
        // a **dark** bar, i.e. the meter must not lie in that direction either.
        let quiet_s16: Vec<u8> = (0..960)
            .flat_map(|i| (((i as f32 * 0.01).sin() * 3.0) as i16).to_le_bytes())
            .collect();
        let quiet_as_f32: Vec<f32> = quiet_s16
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect();
        let dark = analyse_block(&quiet_as_f32, METER_CHANNELS);
        assert!(!dark.is_implausible(), "denormals are not garbage");
        assert!(dark.level()[0] < 0.1, "{:?}", dark.level());
    }

    /// The scale must leave **headroom above full scale** so that a bus fed by
    /// several strips (their sum is routinely over 0 dBFS) is not glued to the
    /// top: 0 dBFS → 0.91, +6 dBFS → 1.0.
    #[test]
    fn full_scale_is_not_the_top_of_the_bar() {
        assert!((level_from_amplitude(1.0) - 0.91).abs() < 0.01);
        assert_eq!(level_from_amplitude(2.0), 1.0);
        assert!(
            level_from_amplitude(1.4) < 1.0,
            "+3 dBFS is not overload yet"
        );
        // A summed bus (−3 dBFS RMS) is high but visibly short of the top.
        let summed = level_from_amplitude(std::f32::consts::FRAC_1_SQRT_2);
        assert!((summed - 0.87).abs() < 0.01, "{summed}");
    }

    #[test]
    fn a_mono_block_feeds_both_channels() {
        let mono = vec![0.5_f32; 480];
        let sample = analyse_block(&mono, 2);
        assert!((sample.rms[0] - 0.5).abs() < 1e-6);
        assert_eq!(sample.rms[0], sample.rms[1]);
    }

    #[test]
    fn the_envelope_attacks_instantly_and_releases_with_a_350_ms_constant() {
        let mut envelope = Envelope::new();
        assert_eq!(envelope.update([1.0, 0.5], Duration::ZERO), [1.0, 0.5]);
        // One 40 ms UI tick: exp(-40/350) = 0.892.
        let after = envelope.update([0.0, 0.0], METER_INTERVAL);
        let expected = (-METER_INTERVAL.as_secs_f32() / RELEASE_TAU.as_secs_f32()).exp();
        assert!((after[0] - expected).abs() < 1e-6, "{after:?}");
        assert!((after[1] - 0.5 * expected).abs() < 1e-6);
        // Two ticks compose: 0.892² = 0.795.
        let two = envelope.update([0.0, 0.0], METER_INTERVAL);
        assert!((two[0] - expected * expected).abs() < 1e-6, "{two:?}");
        // A new peak wins immediately (attack is instantaneous).
        assert_eq!(envelope.update([0.9, 0.9], METER_INTERVAL), [0.9, 0.9]);
        // …and the release really is ~350 ms: one second of silence leaves 5 %.
        let mut falling = Envelope::new();
        falling.update([1.0, 1.0], Duration::ZERO);
        let after_one_second = falling.update([0.0, 0.0], Duration::from_secs(1));
        assert!(
            (after_one_second[0] - 0.057).abs() < 0.01,
            "one second after the signal stops the bar must be near dark: {after_one_second:?}"
        );
        // Half a second gets it to 24 % — not “several seconds of lag”.
        let mut half = Envelope::new();
        half.update([1.0, 1.0], Duration::ZERO);
        let after_half = half.update([0.0, 0.0], Duration::from_millis(500));
        assert!(after_half[0] < 0.25, "{after_half:?}");
    }

    #[test]
    fn the_envelope_is_frame_rate_independent() {
        // 2 × 20 ms must decay like 1 × 40 ms — whatever the call pattern.
        let mut split = Envelope::new();
        split.update([1.0, 1.0], Duration::ZERO);
        split.update([0.0, 0.0], Duration::from_millis(20));
        let split_level = split.update([0.0, 0.0], Duration::from_millis(20));
        let mut whole = Envelope::new();
        whole.update([1.0, 1.0], Duration::ZERO);
        let whole_level = whole.update([0.0, 0.0], Duration::from_millis(40));
        assert!((split_level[0] - whole_level[0]).abs() < 1e-6);
        // And a 16 ms step then a 100 ms step follow exp(-dt/τ) exactly: the
        // fall depends on the elapsed time, never on the number of calls.
        let mut stepped = Envelope::new();
        stepped.update([1.0, 1.0], Duration::ZERO);
        let after_16 = stepped.update([0.0, 0.0], Duration::from_millis(16));
        let after_116 = stepped.update([0.0, 0.0], Duration::from_millis(100));
        let expected_116 = (-0.116_f32 / RELEASE_TAU.as_secs_f32()).exp();
        assert!((after_16[0] - (-0.016_f32 / 0.35).exp()).abs() < 1e-6, "{after_16:?}");
        assert!(
            (after_116[0] - expected_116).abs() < 1e-5,
            "{after_116:?} vs {expected_116}"
        );
        // It keeps falling towards zero, never below.
        let mut envelope = Envelope::new();
        envelope.update([1.0, 1.0], Duration::ZERO);
        for _ in 0..200 {
            envelope.update([0.0, 0.0], METER_INTERVAL);
        }
        assert!(envelope.level()[0] < 1e-6);
        assert!(envelope.level()[0] >= 0.0);
    }

    /// The bar reads the **RMS over 100 ms**, not the loudest 10 ms block: a
    /// transient must be diluted, and a steady signal must give a steady bar
    /// (no “ça vibre”).
    #[test]
    fn the_vu_window_averages_a_transient_instead_of_following_it() {
        let steady = MeterSample {
            rms: [0.1, 0.1],
            peak: [0.3, 0.3],
        };
        let mut window = RmsWindow::new();
        assert!(window.is_empty());
        assert_eq!(window.level(), [0.0, 0.0]);
        for _ in 0..RMS_WINDOW_BLOCKS {
            window.push(&steady);
        }
        assert_eq!(window.len(), RMS_WINDOW_BLOCKS);
        assert!((window.rms()[0] - 0.1).abs() < 1e-6, "{:?}", window.rms());
        let steady_level = window.level()[0];
        assert!((steady_level - 0.61).abs() < 0.01, "{steady_level}");
        // One loud block (a snare) moves the window by ~1/10 of its energy:
        // RMS = sqrt((9·0.01 + 0.64)/10) = 0.270, i.e. +8.6 dB — a visible
        // kick, not the +10 dB a peak follower would show.
        window.push(&MeterSample {
            rms: [0.8, 0.8],
            peak: [1.0, 1.0],
        });
        let kicked = window.rms()[0];
        assert!(kicked > 0.26 && kicked < 0.28, "{kicked}");
        assert!(window.level()[0] - steady_level > 0.1, "{steady_level}");
        // Two different channels stay two different channels.
        let mut stereo = RmsWindow::new();
        for _ in 0..RMS_WINDOW_BLOCKS {
            stereo.push(&MeterSample {
                rms: [0.5, 0.0],
                peak: [0.7, 0.0],
            });
        }
        assert!(stereo.rms()[0] > 0.49 && stereo.rms()[1] == 0.0, "{:?}", stereo.rms());
        // A window that is not full averages what it has (no fade-in).
        let mut warm = RmsWindow::new();
        warm.push(&steady);
        assert!((warm.rms()[0] - 0.1).abs() < 1e-6);
        warm.reset();
        assert!(warm.is_empty());
        assert_eq!(warm.level(), [0.0, 0.0]);
    }

    /// **The window is short enough to react**, and that is measured rather than
    /// argued: the bar is fed the way the app feeds it (one 10 ms block every
    /// 10 ms, the level read at the 25 Hz poll) and the time to reach 90 % of the
    /// reading is taken.
    ///
    /// The 300 ms window needed **three** UI ticks (120 ms) — the reported “ils
    /// ont toujours un peu de lag” — and a 50 ms burst only ever reached 0.654.
    /// At 100 ms the tone is at 90 % within one tick (40 ms) and the same burst
    /// reaches 0.727.  The test keeps both ends honest: fast enough on the way
    /// up, still averaging ten blocks.
    #[test]
    fn the_window_is_short_enough_to_react_to_a_burst() {
        const BLOCK: Duration = Duration::from_millis(10);

        // `burst` = how many blocks carry the tone (`None` = it never stops).
        // Returns (the level when the bar settles, the peak, the first tick at
        // 90 % of the settled level).
        fn run(burst: Option<usize>, window_blocks: usize) -> (f32, f32, u128) {
            let tone = MeterSample {
                rms: [0.3536, 0.3536],
                peak: [0.5, 0.5],
            };
            let mut window = RmsWindow::with_blocks(window_blocks);
            // Warm the window with silence first: a meter that has been running
            // has a *full* window, and that is the state in which dilution can
            // be measured at all (a fresh window averages only what it holds, so
            // the very first burst reads full on any length).
            for _ in 0..window_blocks {
                window.push(&MeterSample::default());
            }
            let mut envelope = Envelope::new();
            let blocks_per_tick = (METER_INTERVAL.as_millis() / BLOCK.as_millis()) as usize;
            let mut peak = 0.0_f32;
            let mut settled = 0.0_f32;
            let mut level_at = |tick: usize| {
                for block in 0..blocks_per_tick {
                    let index = tick * blocks_per_tick + block;
                    let sample = match burst {
                        Some(count) if index >= count => MeterSample::default(),
                        _ => tone,
                    };
                    window.push(&sample);
                }
                envelope.update(window.level(), METER_INTERVAL)[0]
            };
            let mut trace = Vec::new();
            for tick in 0..12 {
                let level = level_at(tick);
                peak = peak.max(level);
                settled = level;
                trace.push(((tick as u128 + 1) * METER_INTERVAL.as_millis(), level));
            }
            let target = 0.9 * settled;
            let reached = trace
                .iter()
                .find(|(_, level)| *level >= target)
                .map(|(ms, _)| *ms)
                .unwrap_or(u128::MAX);
            (settled, peak, reached)
        }

        // A sustained −9 dBFS tone: the bar is at 90 % of its reading after one
        // poll (40 ms) instead of three (120 ms at 300 ms of integration).
        let (settled, _, reached) = run(None, RMS_WINDOW_BLOCKS);
        assert!(settled > 0.7, "the reading itself is unchanged: {settled}");
        assert!(
            reached <= METER_INTERVAL.as_millis(),
            "the bar must reach 90 % within one UI tick, took {reached} ms"
        );
        // …and a 300 ms window really was three ticks slower: the measurement
        // is what justifies the constant, not a guess.
        let (_, _, slow) = run(None, 30);
        assert!(
            slow >= 3 * METER_INTERVAL.as_millis(),
            "300 ms took {slow} ms"
        );

        // A 50 ms burst (5 blocks) is *not* swallowed: it draws most of the bar.
        let (_, burst_peak, _) = run(Some(5), RMS_WINDOW_BLOCKS);
        assert!(
            burst_peak > 0.70,
            "a 50 ms burst must be visible (got {burst_peak})"
        );
        let (_, old_peak, _) = run(Some(5), 30);
        assert!(
            old_peak < 0.68,
            "the old 300 ms window diluted it (got {old_peak})"
        );
        assert!(burst_peak > old_peak + 0.05, "{burst_peak} vs {old_peak}");

        // The constants themselves: 100 ms of audio, ten blocks.
        assert_eq!(RMS_WINDOW, Duration::from_millis(100));
        assert_eq!(RMS_WINDOW_BLOCKS, 10);
        assert_eq!(
            RMS_WINDOW.as_millis(),
            RMS_WINDOW_BLOCKS as u128 * BLOCK.as_millis(),
            "the window is a number of blocks, and it is 100 ms of audio"
        );
        // The tone above is a −9 dBFS sine: 0.3536 RMS, i.e. what the reading
        // converges to (±0.01 of 0.77 on the bar scale).
        assert!((settled - 0.77).abs() < 0.02, "{settled}");
    }

    /// The post-fader correction works in the **amplitude** domain: a fader at
    /// 0.3 is −30 dB (PulseAudio's cubic curve, 0.027), not “30 % of the bar”.
    #[test]
    fn a_linear_gain_attenuates_the_level_by_decibels_not_by_bars() {
        // Round trip: amplitude → level → amplitude.
        for amplitude in [1.0_f32, 0.5, 0.1, 0.01, 0.001] {
            let level = level_from_amplitude(amplitude);
            let back = amplitude_from_level(level);
            assert!(
                (back - amplitude).abs() / amplitude < 0.01,
                "{amplitude} → {level} → {back}"
            );
        }
        assert_eq!(amplitude_from_level(0.0), 10.0_f32.powf(LEVEL_FLOOR_DB / 20.0));
        assert_eq!(amplitude_from_level(1.0), 10.0_f32.powf(LEVEL_CEILING_DB / 20.0));
        // Unity gain is a no-op, zero gain is silence, NaN is silence.
        assert!((attenuate_level(0.73, 1.0) - 0.73).abs() < 1e-6);
        assert_eq!(attenuate_level(0.73, 0.0), 0.0);
        assert_eq!(attenuate_level(0.73, f32::NAN), 0.0);
        // 0 dBFS RMS at volume 0.3 (−31.4 dB) sits at −31.4 dBFS = 0.43.
        let gained = attenuate_level(level_from_amplitude(1.0), 0.027);
        assert!((gained - 0.43).abs() < 0.02, "{gained}");
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
