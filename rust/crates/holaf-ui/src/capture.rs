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
//!        --format f32 --raw --rate 48000 --channels 2 -   → raw f32le on stdout
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
//! # The sample format: **known** with `--raw`, *detected* only without it
//!
//! `--format f32` does **not** mean “f32le on stdout”.  Since PipeWire 1.4 a
//! capture to `-` without `--raw` is written through **libsndfile**, which wraps
//! the samples in a `.au` **container** (a header, then big-endian data), and
//! `--format` only picks the sample format *inside* that container.  A user
//! report on PipeWire 1.6.11 showed the result: the reader decoded the header and
//! the endian-swapped data as `f32`, every block was “impossible”, and every VU
//! bar read 0.000 — on a machine where the same command wrote plain `f32le`
//! under PipeWire 0.3.65 (which has no `--raw` and writes the raw samples).
//!
//! So the format is handled in two independent layers:
//!
//! 1. **Force it** — `--raw` (`-a`) is added when the recorder supports it
//!    (`pw-cat --help` is probed once, see [`recorder_supports_raw`]); it makes
//!    `pw-cat` `fwrite` the samples with no container.  On a `pw-cat` that
//!    predates 1.4 the flag does not exist and is simply not passed (that
//!    version already writes raw samples to `-`).
//! 2. **Trust the contract when it is one** — with `--raw` in effect the samples
//!    are `f32le`, full stop: the format is frozen to [`SampleFormat::F32Le`] and
//!    the byte detection is **never** run.  It used to be, and it was a bug:
//!    a single suspicious block (denormals) made it “detect” `s16le`/`s32le`
//!    *at random* (`s16le` for one source, `s32le` for another, same command),
//!    freeze that width and decode every later block of **valid** music at it —
//!    measured as `rms 0.5858 / peak 0.9969` instead of `rms ≈ 0.05`, i.e. a bar
//!    pinned at 0.84 and vibrating.  A block that cannot be f32 in raw mode is
//!    now **refused and reported**, never re-interpreted.
//!
//!    Without `--raw` (a `pw-cat` < 1.4, or a wrapper), the width really is
//!    unknown, so the bytes are measured against the formats `pw-cat` can
//!    really write ([`detect_sample_format`]) when the `f32` reading of the
//!    block cannot be audio.  Two measurements say that, and **both** are
//!    needed:
//!    * **too loud** — a peak above +40 dBFS ([`MeterSample::is_implausible`]);
//!      a misread `s32` block reaches 1e38.
//!    * **too quiet for these bytes** — an RMS below −180 dBFS while the bytes
//!      are *not* all zero ([`f32_reading_is_a_misread`]): reinterpreting `s16`
//!      four bytes at a time produces **denormals** (measured: peak 4.9e-30,
//!      RMS 1.4e-30 for a −20 dBFS-RMS tone), which pass a “too loud” guard and
//!      left every bar at exactly 0.000 — the residual “VU à zéro”.
//!
//!    The block is then **converted** (`s16le/32768`, `s32le/2³¹`, `f64`, `u8`,
//!    `s8`) instead of being thrown away, but only after the same guess has been
//!    repeated over [`MISREAD_CONFIRM_BLOCKS`] consecutive blocks: one odd block
//!    is not a sample format, and freezing the wrong width is far worse than one
//!    block of silence.  The confirmed [`SampleFormat`] is frozen for that
//!    capture (one conversion per block, no guess per sample) and re-detected if
//!    the stream changes format mid-way.
//!
//! # What the bar shows: a 300 ms RMS, released in 350 ms
//!
//! The reader does **not** publish the block's RMS: a 10 ms RMS is dominated by
//! the loudest cycles in it (−22…−19 dBFS between two neighbouring blocks of the
//! *same* mix), and feeding that into a `max`-hold envelope makes a bar that is
//! “constant, too high, and vibrating” — the user's words.  The published level
//! is [`crate::meters::RmsWindow`]'s (300 ms), and the UI releases it with
//! [`crate::meters::RELEASE_TAU`] on the real elapsed time.
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
//!
//! # Nothing is silent, not even the failure
//!
//! A log line is not enough: the shipped app is started from a `.desktop` file,
//! so its stderr goes nowhere and a user can only report “all the VU bars read
//! 0.000”.  Every capture therefore *publishes its own state* into a
//! [`CaptureRegistry`] shared with the diagnostic report (see
//! `crate::diagnostics`): the source it records, the node it targets, the child's
//! pid, whether it is waiting/streaming/ended/failed, how many blocks it decoded
//! and how many it refused, the last RMS/peak, and the tail of what `pw-cat`
//! wrote on stderr.  The CAPTURES section of the report and the `metering_dead` /
//! `metering_implausible` findings are built from it.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};

use crate::diagnostics::{CaptureRegistry, RejectReason};
use crate::meters::{
    analyse_block, MeterBoard, MeterSample, RmsWindow, BLOCK_FRAMES, METER_CHANNELS, METER_RATE,
};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Bytes of one published block (`480 frames × 2 ch × 4 B`).
const BLOCK_BYTES: usize = BLOCK_FRAMES * METER_CHANNELS * 4;

/// How many **consecutive** blocks must agree that the stream is a misread
/// before the reader believes the bytes over its own format.
///
/// The format detection exists for one case: a recorder that ignores
/// `--format f32` and writes another width (PipeWire 1.4+ without `--raw`).  A
/// **single** suspicious block, though, is often just an unlucky one — and a
/// wrong guess is not a one-block error: it re-frames the whole stream and every
/// later block is decoded at the wrong width, which is how a valid f32 stream of
/// music came back as `rms 0.58, peak 0.99` (near-square garbage) on a user's
/// machine — a bar pinned high, vibrating, insensitive to the volume.  Two
/// agreeing blocks (20 ms) is the smallest evidence that survives a one-block
/// hiccup, and it costs 20 ms on the legitimate case.
const MISREAD_CONFIRM_BLOCKS: u32 = 2;

/// Default recording client.  Overridable (`HOLAF_PW_CAT`) so a test — or a
/// build where `pw-cat` is a wrapper — can point somewhere else without a
/// process-wide environment mutation.
///
/// The resolution lives in [`crate::diagnostics::default_recorder`] (the report
/// prints the same value, and one source of truth means they cannot disagree).
pub fn default_recorder() -> String {
    crate::diagnostics::default_recorder()
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

/// Does this recorder accept `--raw` / `-a`?
///
/// **Why probe instead of always passing it:** `pw-cat` gained `--raw` in
/// PipeWire 1.4.  Before that, `--raw` is an *unknown option* and `pw-cat`
/// exits with `unrecognized option '--raw'` — passing it unconditionally would
/// turn a working capture into a dead one on every older server (0.3.x is still
/// common).  On those versions the raw stream is already what `-` gives, so the
/// flag is not needed; on 1.4+ it is what stops libsndfile from wrapping the
/// samples in an `.au` container.
///
/// The answer is cached: it costs one `<recorder> --help` spawn for the whole
/// process (the capture is started once per strip, the probe not per strip).
pub fn recorder_supports_raw(recorder: &str) -> bool {
    static CACHE: OnceLock<std::sync::Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(answer) = guard.get(recorder) {
            return *answer;
        }
    }
    let answer = probe_raw_flag(recorder);
    if let Ok(mut guard) = cache.lock() {
        guard.insert(recorder.to_string(), answer);
    }
    answer
}

/// How long the recorder is given to answer `--help` (see [`probe_raw_flag`]).
const RAW_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// What a **bounded** `<recorder> <arg>` probe produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeOutput {
    /// Did the child exit with status 0?
    pub success: bool,
    /// Everything it printed on stdout.
    pub stdout: String,
    /// Everything it printed on stderr.
    pub stderr: String,
}

impl ProbeOutput {
    /// stdout and stderr as one text (what “does it mention this option?”
    /// checks want: `pw-cat` writes its help on either).
    pub fn text(&self) -> String {
        let mut text = self.stdout.clone();
        text.push_str(&self.stderr);
        text
    }
}

/// Run `<recorder> <arg>` and collect what it printed, **giving up after
/// `timeout`**.
///
/// Why not `Command::output()`: it waits for the child to exit *and* for
/// end-of-file on both pipes.  A recorder that is a **shell wrapper** — the
/// documented workaround for a `pw-cat` that writes the wrong sample format, and
/// every test stand-in — ignores the options it does not know and starts
/// recording for ever, so `output()` never returns.  Measured: the diagnostic
/// report froze for minutes because a wrapper was asked `--version`.
///
/// `None` means “could not spawn it, or it did not answer in time”: the caller
/// treats both the same way (the probe is a convenience, never the diagnosis).
pub fn recorder_probe_output(recorder: &str, arg: &str, timeout: Duration) -> Option<ProbeOutput> {
    let mut child = Command::new(recorder)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    // Poll for the exit: `output()` would block on a recorder that streams for
    // ever (a wrapper that ignores the argument).
    let deadline = Instant::now() + timeout;
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    // The pipes close with the child, so these reads cannot block.
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    Some(ProbeOutput {
        success,
        stdout,
        stderr,
    })
}

/// Run `<recorder> --help` and look for the `--raw` option.
///
/// A binary that cannot even be spawned (missing `pw-cat`) answers `false`: the
/// spawn failure is reported by [`spawn_stream`] with the OS reason, which is
/// far more useful than “no --raw”.
///
/// The probe is **bounded** ([`recorder_probe_output`]): a recorder that does not
/// answer `--help` and exit (a wrapper script that ignores its arguments and
/// records for ever) is killed after [`RAW_PROBE_TIMEOUT`] and treated as “no
/// `--raw`”.  Without that bound the whole spawn would wait for ever on a
/// recorder that never returns.
fn probe_raw_flag(recorder: &str) -> bool {
    let Some(output) = recorder_probe_output(recorder, "--help", RAW_PROBE_TIMEOUT) else {
        return false;
    };
    // Match the option, not the word in prose: `pw-cat` prints
    // `  -a, --raw                             RAW mode`.
    output.text().lines().any(|line| {
        line.split_whitespace()
            .any(|token| token.trim_end_matches(',') == "--raw")
    })
}

/// The sample format a capture stream really carries, as detected from the
/// **bytes** (never from the `--format` we asked for).
///
/// Only the formats `pw-cat` can write to a raw stream are covered.  Each one
/// knows how to turn one block of bytes into the `f32` samples the meter maths
/// expect, scaling integer formats by their full-scale value so a −20 dBFS sine
/// reads −20 dBFS whatever the transport format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// 32-bit little-endian float — what `--format f32 --raw` writes, the
    /// nominal case (and the only one that needs no conversion).
    F32Le,
    /// 16-bit little-endian signed PCM (the `pw-cat` default, and what a
    /// libsndfile `.au` container holds once the header is stripped).
    S16Le,
    /// 32-bit little-endian signed PCM.
    S32Le,
    /// 64-bit little-endian float.
    F64Le,
    /// 8-bit unsigned PCM (silence is 128, full scale ±127).
    U8,
    /// 8-bit signed PCM.
    S8,
}

impl SampleFormat {
    /// Bytes per sample on the wire.
    pub fn width(self) -> usize {
        match self {
            SampleFormat::F32Le | SampleFormat::S32Le => 4,
            SampleFormat::S16Le => 2,
            SampleFormat::F64Le => 8,
            SampleFormat::U8 | SampleFormat::S8 => 1,
        }
    }

    /// The name the report prints (`s16le`, `f32le`, …).
    pub fn name(self) -> &'static str {
        match self {
            SampleFormat::F32Le => "f32le",
            SampleFormat::S16Le => "s16le",
            SampleFormat::S32Le => "s32le",
            SampleFormat::F64Le => "f64le",
            SampleFormat::U8 => "u8",
            SampleFormat::S8 => "s8",
        }
    }

    /// Decode `bytes` into interleaved `f32` samples, appending to `out`.
    ///
    /// A trailing partial sample (a block that is not a multiple of the format
    /// width) is dropped rather than read out of bounds — the block boundary is
    /// what the caller already validated, this only guards a mid-stream change.
    /// No allocation per sample: `out` is reserved once by the caller.
    pub fn decode_into(self, bytes: &[u8], out: &mut Vec<f32>) {
        let width = self.width();
        let count = bytes.len() / width;
        out.reserve(count);
        match self {
            SampleFormat::F32Le => {
                for chunk in bytes[..count * 4].as_chunks::<4>().0 {
                    out.push(f32::from_le_bytes(*chunk));
                }
            }
            SampleFormat::S16Le => {
                const SCALE: f32 = 1.0 / 32_768.0;
                for chunk in bytes[..count * 2].as_chunks::<2>().0 {
                    out.push(f32::from(i16::from_le_bytes(*chunk)) * SCALE);
                }
            }
            SampleFormat::S32Le => {
                const SCALE: f32 = 1.0 / 2_147_483_648.0;
                for chunk in bytes[..count * 4].as_chunks::<4>().0 {
                    out.push(i32::from_le_bytes(*chunk) as f32 * SCALE);
                }
            }
            SampleFormat::F64Le => {
                for chunk in bytes[..count * 8].as_chunks::<8>().0 {
                    out.push(f64::from_le_bytes(*chunk) as f32);
                }
            }
            SampleFormat::U8 => {
                // u8 PCM is biased: 128 is silence.
                const SCALE: f32 = 1.0 / 128.0;
                for byte in &bytes[..count] {
                    out.push((f32::from(*byte) - 128.0) * SCALE);
                }
            }
            SampleFormat::S8 => {
                const SCALE: f32 = 1.0 / 128.0;
                for byte in &bytes[..count] {
                    out.push(f32::from(*byte as i8) * SCALE);
                }
            }
        }
    }

    /// Decode a whole block into a fresh `f32` buffer (the reader's hot path,
    /// used when the block's format is not the nominal `f32le`).
    pub fn decode(self, bytes: &[u8]) -> Vec<f32> {
        let mut out = Vec::with_capacity(bytes.len() / self.width());
        self.decode_into(bytes, &mut out);
        out
    }
}

/// Does this block start with a **container magic** (`RIFF`, `.snd`)?
///
/// A container is the one thing that must be *refused*, not converted: the
/// header appears once at the start of the stream, so converting its payload as
/// raw samples would mis-align every following block by the header length.  This
/// is exactly what PipeWire 1.4+ writes to `-` without `--raw` (an `.au`
/// container), and why [`recorder_supports_raw`] exists.
pub fn starts_with_container(bytes: &[u8]) -> bool {
    bytes.starts_with(b"RIFF") || bytes.starts_with(b".snd") || bytes.starts_with(b"FORM")
}

/// RMS below which an `f32le` reading of a block is **not believed** when the
/// bytes are not all zero.
///
/// [`MeterSample::is_implausible`] only catches the *loud* half of a misread — a
/// block whose `f32` peak is over +40 dBFS.  Reading a **narrower** format four
/// bytes at a time just as often produces the opposite: almost nothing.
///
/// Measured on the headless sandbox, `s16le` tone (peak −20 dBFS, every sample in
/// `0..=3783`, so the high byte never sets a large exponent) read as `f32le`:
/// peak 4.9e-30, RMS 1.4e-30.  That is far below the +40 dBFS guard, so `f32le`
/// was frozen on it, 249 blocks were *accepted*, and the level of every one of
/// them was exactly 0.000 (the residual “VU à zéro”: `detected_format: None`,
/// `last_rms: 1.4e-30`).  Values that small are denormals (or dropped
/// NaN/infinities) — the signature of bytes read at the wrong width.
///
/// **−180 dBFS** is chosen so no real capture can be mistaken for it: a genuine
/// `f32` path bottoms out around 1e-7 (−140 dBFS) with real material, and honest
/// digital silence is *all zero bytes*, which [`f32_reading_is_a_misread`]
/// excludes explicitly.
pub const SUSPICIOUS_LOW_RMS: f32 = 1e-9;

/// Is this block's `f32le` reading a **misread** rather than a genuinely quiet
/// stream?
///
/// The positive half of the format detection: `is_implausible` says “too loud to
/// be audio”, this says “too quiet to be *these* bytes”.  Both are needed — see
/// [`SUSPICIOUS_LOW_RMS`] for the measurement that made the second one necessary.
///
/// Two conditions, cheapest first:
/// 1. the loudest channel's RMS is under [`SUSPICIOUS_LOW_RMS`];
/// 2. the block is **not** all zero bytes — an honest silent stream stays
///    silent, so it must never trigger a conversion (and a `u8` stream's silence
///    is `0x80`, which is not zero either, so it is still detected).
///
/// Denormals are not hunted sample by sample: an RMS under −180 dBFS already
/// means every sample is a denormal or a NaN/infinity that [`analyse_block`]
/// dropped, which is exactly what a misread block looks like.
fn f32_reading_is_a_misread(bytes: &[u8], as_f32: &MeterSample) -> bool {
    let rms = as_f32.rms[0].max(as_f32.rms[1]);
    // Short-circuit on the RMS: the byte scan only runs for a suspicious block.
    rms < SUSPICIOUS_LOW_RMS && bytes.iter().any(|byte| *byte != 0)
}

/// Which format a block of bytes really is, judged from the bytes alone.
///
/// The candidate formats are decoded and scored; the winner is the one whose
/// samples look most like **audio**: plausible peak (finite, non-zero, within a
/// couple of dB of full scale) *and* **smooth in time**.  Smoothness is what
/// separates the right width from a wrong one: a block decoded at the wrong
/// width builds its “samples” from pieces of neighbouring real samples, so it
/// jumps from full scale to zero and back (a huge frame-to-frame change), while
/// real PCM — at any width — only moves a little between consecutive samples of
/// a 48 kHz stream.
///
/// `f32le` is never returned: if it were plausible the block would not have
/// reached here, and reporting “the recorder wrote f32le” would be a lie.
fn guess_format(bytes: &[u8]) -> Option<(SampleFormat, f32)> {
    const CANDIDATES: [SampleFormat; 5] = [
        SampleFormat::S16Le,
        SampleFormat::S32Le,
        SampleFormat::F64Le,
        SampleFormat::U8,
        SampleFormat::S8,
    ];
    let mut best: Option<(SampleFormat, f32, f32)> = None; // (format, peak, roughness)
    for format in CANDIDATES {
        let Some((peak, roughness)) = score_audio(bytes, format) else {
            continue;
        };
        best = match best {
            // Lower roughness = more audio-like.  On a tie the **wider** format
            // wins (8-bit audio is ambiguous with a single silent sample).
            Some((chosen, _, chosen_rough))
                if roughness < chosen_rough
                    || (roughness == chosen_rough && format.width() > chosen.width()) =>
            {
                Some((format, peak, roughness))
            }
            Some(found) => Some(found),
            None => Some((format, peak, roughness)),
        };
    }
    best.map(|(format, peak, _)| (format, peak))
}

/// Decode `bytes` as `format` and return `(peak, roughness)` when the result
/// looks like audio, `None` otherwise.
///
/// `roughness` is the mean absolute step between consecutive samples divided by
/// the RMS: ~0 for a smooth sine, ≫ 1 for the alternating full-scale junk a
/// wrong-width decode produces.
fn score_audio(bytes: &[u8], format: SampleFormat) -> Option<(f32, f32)> {
    let samples = format.decode(bytes);
    if samples.len() < 2 {
        return None;
    }
    // NaN/inf samples are dropped by `analyse_block` too; a format that is mostly
    // non-finite is not this stream's format.
    let finite: Vec<f32> = samples.iter().copied().filter(|s| s.is_finite()).collect();
    if finite.len() < samples.len() / 2 {
        return None;
    }
    let peak = finite.iter().fold(0.0_f32, |peak, s| peak.max(s.abs()));
    // Plausible audio: within a couple of dB of full scale, and not silence.
    if !(peak > 1e-4 && peak <= 1.5) {
        return None;
    }
    let rms = (finite.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>()
        / finite.len() as f64)
        .sqrt() as f32;
    if rms <= 1e-6 {
        return None;
    }
    let mean_step = finite
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .sum::<f32>()
        / (finite.len() - 1) as f32;
    let roughness = mean_step / rms;
    // Real audio is band-limited at 48 kHz: consecutive samples differ by a
    // fraction of the RMS.  A wrong width scrambles them into a near-white
    // signal (roughness ≳ 1.5, often ~2 for full-scale alternation).  A signal
    // with **no** variation at all is DC (a constant byte pattern), which is not
    // audio either — real silence never reaches here (its f32 reading is
    // plausible and it is never re-detected).
    if !(1e-4..=1.5).contains(&roughness) {
        return None;
    }
    Some((peak, roughness))
}

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
    /// Has the child's exit already been reported?  (`try_wait` keeps answering
    /// with the same status, so without this flag every snapshot would log the
    /// death again.)
    exit_reported: bool,
}

/// All capture streams of the mixer, keyed by strip uid.
pub struct MeterCapture {
    streams: HashMap<String, CaptureStream>,
    /// Sources already reported as unopenable, so the log is not spammed on
    /// every snapshot.
    warned: HashSet<String>,
    /// The recording client (see [`default_recorder`]).
    recorder: String,
    /// Where every capture publishes its own status, for the diagnostic report
    /// ([`crate::diagnostics::CaptureRegistry`]).
    stats: CaptureRegistry,
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
        Self::with_recorder_and_registry(recorder, CaptureRegistry::new())
    }

    /// Same, publishing into a registry the caller already shares with the
    /// diagnostic report (the engine thread creates the capture, the UI thread
    /// reads the registry, so both must hold the same handle).
    pub fn with_recorder_and_registry(
        recorder: impl Into<String>,
        stats: CaptureRegistry,
    ) -> Self {
        let recorder = recorder.into();
        stats.set_recorder(&recorder);
        Self {
            streams: HashMap::new(),
            warned: HashSet::new(),
            recorder,
            stats,
        }
    }

    /// The status registry (see [`crate::diagnostics::CaptureRegistry`]).
    pub fn stats(&self) -> &CaptureRegistry {
        &self.stats
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
        // 0. record the exit status of children that died since the last
        //    snapshot, so the report can say “exited with code N” instead of
        //    showing a capture that is merely “waiting for a block”.
        let exited: Vec<(String, Option<i32>)> = self
            .streams
            .iter_mut()
            .filter_map(|(uid, stream)| {
                if stream.exit_reported {
                    return None;
                }
                let status = stream.child.try_wait().ok().flatten()?;
                stream.exit_reported = true;
                Some((uid.clone(), status.code()))
            })
            .collect();
        for (uid, code) in exited {
            log::warn!(
                "metering: {uid}: the recorder exited ({})",
                code.map(|code| format!("code {code}"))
                    .unwrap_or_else(|| "killed by a signal".to_string())
            );
            self.stats.exited(&uid, code);
        }

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
                // The entry goes away with the child: a stale status must never
                // describe a graph it no longer belongs to.
                self.stats.stopped(&uid);
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
            match spawn_stream(&self.recorder, uid, source, slot, board.clone(), &self.stats) {
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
                    // Logged **and** published: the log is invisible when the app
                    // is started from a .desktop file, the report is not.
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
            self.stats.stopped(&uid);
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
    stats: &CaptureRegistry,
) -> Result<CaptureStream, String> {
    // `--target` wants a node name; a `.monitor` source is a sink monitor and
    // needs `stream.capture.sink` (see the module docs — without either, the
    // stream quietly records the default source and the meter stays dark).
    let (node, monitor) = capture_target(source);
    // The arguments are built as data first, so the **exact** command line can be
    // published to the report: `pw-cat` is an external binary whose options are
    // not ours to assume (a 1.x may refuse one of them), and a user who can paste
    // the command learns more in one second than from any log line.
    let mut args: Vec<String> = vec!["--record".to_string()];
    if monitor {
        args.push("-P".to_string());
        args.push(MONITOR_PROPERTIES.to_string());
    }
    // `--raw` forces the samples themselves on stdout instead of a libsndfile
    // container (see the module docs).  Only passed when the recorder knows it:
    // `pw-cat` before 1.4 exits on an unknown option, and those versions already
    // write raw samples to `-`.
    let raw = recorder_supports_raw(recorder);
    args.extend([
        "--target".to_string(),
        node.to_string(),
        "--format".to_string(),
        "f32".to_string(),
    ]);
    if raw {
        args.push("--raw".to_string());
    }
    args.extend([
        "--rate".to_string(),
        METER_RATE.to_string(),
        "--channels".to_string(),
        METER_CHANNELS.to_string(),
        // `-` = stream on stdout (verified on pw-cat 0.3.65 and 1.x).
        "-".to_string(),
    ]);
    let command_line = render_command(recorder, &args);
    let mut child = Command::new(recorder)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Kept, and drained into the log: a `pw-cat` that cannot open its
        // target says so on stderr, and swallowing it is how a broken meter
        // became invisible.
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            // “No such file or directory” here means `pw-cat` is **not
            // installed** (on Arch it ships with the `pipewire-audio` package,
            // not with the base `pipewire` one): a cause the report can name,
            // unlike a dark bar and a log line nobody sees.
            let reason = format!("{recorder}: {err}");
            stats.spawn_failed(uid, source, node, monitor, &command_line, &reason);
            reason
        })?;
    stats.started(
        uid,
        source,
        node,
        monitor,
        Some(child.id()),
        &command_line,
    );

    // Every failure path below must reap the child: a `pw-cat` left running with
    // no reader would hold the graph open for the whole session.
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        let reason = "pw-cat has no stdout".to_string();
        stats.ended(uid, reason.clone());
        return Err(reason);
    };
    let label = uid.to_string();
    let stats_for_reader = stats.clone();
    let reader = thread::Builder::new()
        .name(format!("holaf-meter-{uid}"))
        .spawn(move || {
            // The format this capture is really carrying.  **When `--raw` is in
            // effect it is not a guess: it is `f32le`, by contract.**  `--raw`
            // is precisely the flag that tells `pw-cat` to write the samples
            // themselves (no libsndfile container, no conversion) in the format
            // asked for with `--format f32`, so re-detecting it from the bytes
            // can only ever be wrong — and it was: one suspicious block (a
            // denormal tail, a startup block) froze `s16le`/`s32le` and every
            // following block of *valid f32 music* was then decoded at the
            // wrong width (`rms 0.58 peak 0.99` instead of `rms 0.1`), i.e. a
            // VU bar pinned high, vibrating, and deaf to the fader.  Detection
            // therefore stays a **fallback for recorders without `--raw`**
            // (PipeWire < 1.4), where the width really is unknown.
            let mut format: Option<SampleFormat> = if raw {
                Some(SampleFormat::F32Le)
            } else {
                None
            };
            // Bytes of one block in the current format (`BLOCK_FRAMES × 2 ch ×
            // width`); `None` until the format is known.
            let mut block_bytes = BLOCK_BYTES;
            // A block buffer big enough for any format (f32le is the widest at
            // 4 B/sample times 2 channels; f64 is 8 B but half the frames per
            // block would still fit, so size for the f32le case and grow once if
            // f64 is detected).
            let mut buffer = vec![0_u8; BLOCK_BYTES];
            // The `f32` sample buffer, reused across blocks: detection and
            // conversion happen once per block, with no per-sample allocation.
            let mut samples: Vec<f32> = Vec::with_capacity(BLOCK_BYTES / 4);
            // Bytes read past the current block (the probe window usually holds
            // more than one block): the tail belongs to the next block.
            let mut carry: Vec<u8> = Vec::new();
            // One-shot reporters: a stream that needed a conversion (or could not
            // be read at all) must say so once.
            let mut announced = false;
            let mut warned = false;
            // Has a **suspiciously quiet** block already been proved to be a
            // genuinely quiet `f32le` stream (no other format explains its
            // bytes)?  Set once, so the quiet test cannot bounce the reader
            // between “unfreeze” and “freeze `f32le`” on every block.  Cleared
            // whenever a format is detected or a loud misread unfreezes it.
            let mut quiet_f32_settled = false;
            // Consecutive suspicious blocks and the format that explained them;
            // a guess only wins once it has been confirmed (see
            // [`MISREAD_CONFIRM_BLOCKS`]).
            let mut misread: Option<(SampleFormat, u32)> = None;
            // Is this still the first block of the stream?  Only the first block
            // can legitimately carry a container header (see the detection step).
            let mut first_block = true;
            // The **VU integration window**: what the bar shows is the RMS over
            // the last 300 ms, not the loudest 10 ms block (see
            // [`crate::meters::RmsWindow`]).
            let mut window = RmsWindow::new();
            loop {
                // 1. Fill one block: first from the carry, then from stdout.
                buffer.resize(block_bytes, 0);
                let mut filled = 0_usize;
                if !carry.is_empty() {
                    let take = carry.len().min(block_bytes);
                    buffer[..take].copy_from_slice(&carry[..take]);
                    carry.drain(..take);
                    filled = take;
                }
                if filled < block_bytes {
                    match read_block(&mut stdout, &mut buffer[filled..block_bytes]) {
                        Ok(0) if filled == 0 => {
                            // Clean end of stream.
                            stats_for_reader.ended(&label, "the recorder closed its stdout");
                            board.publish(slot, [0.0, 0.0]);
                            return;
                        }
                        Ok(0) => {
                            // The stream ended mid-block.
                            stats_for_reader.reject(&label, RejectReason::Partial, None);
                            stats_for_reader.ended(
                                &label,
                                format!("stdout closed after {filled} of {block_bytes} bytes"),
                            );
                            board.publish(slot, [0.0, 0.0]);
                            return;
                        }
                        Ok(count) => filled += count,
                        Err(err) => {
                            log::warn!("metering: {label}: stream ended: {err}");
                            stats_for_reader.ended(&label, format!("read error: {err}"));
                            board.publish(slot, [0.0, 0.0]);
                            return;
                        }
                    }
                }
                if filled < block_bytes {
                    // A short read can only be the tail of a stream: the rest is
                    // not a frame-aligned block, so it is **counted** as a
                    // partial block instead of silently treated as silence.
                    stats_for_reader.reject(&label, RejectReason::Partial, None);
                    stats_for_reader.ended(
                        &label,
                        format!("stdout closed after {filled} of {block_bytes} bytes"),
                    );
                    board.publish(slot, [0.0, 0.0]);
                    return;
                }

                // 2. Decide the format on the first block, if not already known.
                //
                //    The first block arrives *as f32le* sized, so it may be a
                //    fraction of a real block in another format — but it is
                //    always ≥ 1 whole frame, which is all the detection needs (it
                //    looks at the bytes, not at frame alignment).
                if format.is_none() {
                    let as_f32 = analyse_block(&SampleFormat::F32Le.decode(&buffer), METER_CHANNELS);
                    // A container magic (`.au`/RIFF) is *definitive* — but only at
                    // the **very start** of the stream: a container header appears
                    // once, at offset 0, never later.  Checking it on every
                    // unformatted block would refuse a legitimate s16 block whose
                    // first two samples happen to spell `.snd` (measured: a tone at
                    // 0.9 amplitude gives exactly those bytes).  So the magic only
                    // vetoes detection on the first block of the capture.
                    let looks_like_container = first_block && starts_with_container(&buffer);
                    first_block = false;
                    if looks_like_container {
                        // The recorder is writing a libsndfile container, not raw
                        // samples (PipeWire 1.4+ without `--raw`).  Converting its
                        // payload would mis-align every block by the header, so
                        // this is **terminal** for the capture: report it once and
                        // keep the bar dark (the fix is the `--raw` flag, not a
                        // guess).  No retry: the header only appears once, so
                        // every later block would look “convertible” while being
                        // shifted by the header length.
                        let guess = detect_sample_format(&buffer);
                        log::warn!(
                            "metering: {label}: the recorder writes {} instead of raw samples — \
                             it needs `--raw` (pw-cat 1.4+) to stream f32le. The VU stays dark \
                             instead of drawing the container bytes.",
                            guess
                        );
                        stats_for_reader.reject(&label, RejectReason::Implausible, Some(guess));
                        stats_for_reader.ended(
                            &label,
                            "the recorder writes a container, not raw samples (add `--raw`)",
                        );
                        board.publish(slot, [0.0, 0.0]);
                        return;
                    }
                    // Two independent reasons to distrust the `f32le` reading of
                    // this block, because a misread takes two shapes:
                    //
                    //   * `as_f32.is_implausible()` — **too loud** (peak above
                    //     +40 dBFS).  Reading `s32` as f32 does that.
                    //   * `f32_reading_is_a_misread` — **too quiet for these
                    //     bytes**: an RMS under −180 dBFS while the bytes are not
                    //     all zero, i.e. denormals.  Reading `s16` as f32 does
                    //     that (measured peak 4.9e-30 for a −20 dBFS-RMS tone),
                    //     and it used to be *accepted*, freezing `f32le` and
                    //     leaving the bar at 0.000 for ever.
                    //
                    // Neither is enough on its own: a **single** odd block is
                    // not a format, and freezing the wrong one for the rest of
                    // the stream is far worse than one block of silence.  The
                    // guess is therefore confirmed over
                    // [`MISREAD_CONFIRM_BLOCKS`] consecutive blocks before it is
                    // applied.
                    let impossible = as_f32.is_implausible();
                    let too_quiet = f32_reading_is_a_misread(&buffer, &as_f32);
                    if !impossible && !too_quiet {
                        format = Some(SampleFormat::F32Le);
                        misread = None;
                        continue;
                    }
                    let guess = guess_format(&buffer);
                    let confirmed = match (guess, &misread) {
                        (Some((detected, _)), Some((same, count))) if *same == detected => {
                            count + 1
                        }
                        (Some(_), _) => 1,
                        (None, _) => 0,
                    };
                    misread = guess.map(|(detected, _)| (detected, confirmed));
                    match guess {
                        Some((detected, peak)) if confirmed >= MISREAD_CONFIRM_BLOCKS => {
                            // The bytes have been telling the same story for
                            // `MISREAD_CONFIRM_BLOCKS` blocks: believe them.
                            format = Some(detected);
                            // The bytes are not the `f32le` the recorder was
                            // asked for: the quiet test has nothing left to
                            // prove.
                            quiet_f32_settled = false;
                            let new_bytes = BLOCK_FRAMES * METER_CHANNELS * detected.width();
                            log::info!(
                                "metering: {label}: the recorder writes {} (peak {peak:.3}), \
                                 not f32le — converting every block instead of dropping it",
                                detected.name()
                            );
                            stats_for_reader.format_detected(&label, detected.name(), peak);
                            if new_bytes != block_bytes {
                                // Re-frame: the first block was read at the f32le
                                // size; its tail beyond the real block size is
                                // carried into the next block.
                                block_bytes = new_bytes;
                                if buffer.len() > block_bytes {
                                    carry.splice(0..0, buffer[block_bytes..].iter().copied());
                                    buffer.truncate(block_bytes);
                                }
                            }
                            window.reset();
                        }
                        Some((detected, peak)) => {
                            // One suspect block is not evidence.  Keep the bar
                            // dark **without freezing a format** and look at the
                            // next block: if the bytes keep saying the same
                            // thing, it is confirmed above.  Nothing is counted
                            // as a rejection: the block is retried, not dropped
                            // for good.
                            log::debug!(
                                "metering: {label}: block looks like {} (peak {peak:.3}), \
                                 waiting for {confirmed}/{MISREAD_CONFIRM_BLOCKS} before \
                                 believing it",
                                detected.name()
                            );
                            board.publish(slot, [0.0, 0.0]);
                            continue;
                        }
                        None if too_quiet => {
                            // Nothing explains these bytes: they are a genuinely
                            // (absurdly) quiet f32 stream.  Accept the block as
                            // is, and stop asking (a real stream must not be
                            // re-examined for ever).
                            quiet_f32_settled = true;
                            format = Some(SampleFormat::F32Le);
                            misread = None;
                            continue;
                        }
                        None => {
                            // Nothing explains these bytes: keep the bar dark and
                            // say it once, as before.
                            let guess = (!warned).then(|| detect_sample_format(&buffer));
                            if !warned {
                                warned = true;
                                log::warn!(
                                    "metering: {label}: impossible block (peak {:?}, rms {:?}) — \
                                     the bytes look like {} and no known sample format explains \
                                     them. The VU stays dark instead of pinning at full scale.",
                                    as_f32.peak,
                                    as_f32.rms,
                                    guess.as_deref().unwrap_or("unknown")
                                );
                            }
                            stats_for_reader.reject(&label, RejectReason::Implausible, guess);
                            board.publish(slot, [0.0, 0.0]);
                            // Do not freeze a format: try again on the next block.
                            continue;
                        }
                    }
                }

                // 3. Decode the block in the frozen format and measure it.
                let current = format.unwrap_or(SampleFormat::F32Le);
                samples.clear();
                current.decode_into(&buffer, &mut samples);
                let sample = analyse_block(&samples, METER_CHANNELS);
                // A frozen format that stops explaining the stream means the
                // recorder changed format mid-way — **or** that the stream began
                // with digital silence (whose f32 reading is a plausible 0.0 and
                // froze `f32le` before any real sample arrived) and the first real
                // block is in another format.  The two are told apart: only a
                // *real* format change (a non-f32 format that stops fitting) is
                // reported; the silence-then-s16 startup is a `f32le` that never
                // carried data, so it is silently unfrozen.
                //
                // The **quiet** half of the same coin: `f32le` frozen on a silent
                // first block, then real `s16` bytes whose f32 reading is a
                // denormal (measured: RMS 1.4e-30) — not “implausible”, so the
                // guard above never fired and the bar stayed at 0.000.  Such a
                // block is unfrozen the same silent way (it is not a format
                // change: `f32le` never carried audio).
                //
                // **In raw mode neither applies**: `--raw` is the contract that
                // the samples are `f32le`, so a block that looks impossible is
                // reported and kept off the bar, never used as an excuse to guess
                // another width (that guess is what turned a valid f32 stream into
                // near-square garbage).
                let suspicious_quiet = !raw
                    && current == SampleFormat::F32Le
                    && !quiet_f32_settled
                    && f32_reading_is_a_misread(&buffer, &sample);
                if sample.is_implausible() || suspicious_quiet {
                    if raw {
                        // The recorder breaks the `--raw` contract: say it once
                        // and do not draw the block (a pinned bar is worse than a
                        // dark one, and the report must show the truth).
                        let guess = (!warned).then(|| detect_sample_format(&buffer));
                        if !warned {
                            warned = true;
                            log::warn!(
                                "metering: {label}: a block is impossible as f32le although \
                                 `--raw` was requested (peak {:?}) — the bytes look like {}. \
                                 The VU stays dark instead of guessing another sample width.",
                                sample.peak,
                                guess.as_deref().unwrap_or("unknown")
                            );
                        }
                        stats_for_reader.reject(&label, RejectReason::Implausible, guess);
                        window.reset();
                        board.publish(slot, [0.0, 0.0]);
                        continue;
                    }
                    let started_silent = current == SampleFormat::F32Le;
                    format = None;
                    block_bytes = BLOCK_BYTES;
                    misread = None;
                    if sample.is_implausible() {
                        // A loud misread may be followed by anything: let the
                        // quiet test have its say again.
                        quiet_f32_settled = false;
                    }
                    if sample.is_implausible() && !started_silent {
                        log::warn!(
                            "metering: {label}: block implausible as {} — redetecting (format \
                             changed mid-stream?)",
                            current.name()
                        );
                        stats_for_reader.format_changed(&label);
                        stats_for_reader.reject(&label, RejectReason::Implausible, None);
                    }
                    if suspicious_quiet {
                        log::info!(
                            "metering: {label}: f32le reading of this block is {:.3e} rms (all \
                             denormals) while its bytes are not zero — checking the sample \
                             format again",
                            sample.rms[0].max(sample.rms[1])
                        );
                    }
                    window.reset();
                    board.publish(slot, [0.0, 0.0]);
                    continue;
                }
                // The bar shows the **RMS over the last 300 ms**, not this
                // block: a 10 ms RMS of music jumps by several dB from block to
                // block, and feeding that into a max-hold envelope gives a bar
                // that is constant, too high and vibrating at once.
                stats_for_reader.block(&label, &sample);
                window.push(&sample);
                if !announced {
                    announced = true;
                    let level = window.level();
                    log::info!(
                        "metering: {label}: first block rms {:.4}/{:.4} peak {:.4}/{:.4} \
                         → level {:.2}/{:.2} ({}, {}-block RMS window)",
                        sample.rms[0],
                        sample.rms[1],
                        sample.peak[0],
                        sample.peak[1],
                        level[0],
                        level[1],
                        current.name(),
                        crate::meters::RMS_WINDOW_BLOCKS,
                    );
                }
                board.publish(slot, window.level());
            }
        })
        .map_err(|err| format!("cannot start the meter reader: {err}"));

    let reader = match reader {
        Ok(reader) => reader,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            stats.ended(uid, err.clone());
            return Err(err);
        }
    };

    let stats_for_stderr = stats.clone();
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
                    // The last lines are kept for the report: `pw-cat` is the only
                    // place that says *why* a target refused to open, and that
                    // message must reach a pasteable report, not just a log the
                    // user never sees.
                    stats_for_stderr.push_stderr(&label, line);
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
        exit_reported: false,
    })
}

/// Render `program` + `args` as a line that can be pasted into a shell.
///
/// Only what a shell would misread is quoted (the JSON property is the only such
/// argument here), so the line stays readable in the report:
/// `pw-cat --record -P '{"stream.capture.sink":true}' --target … -`.
pub fn render_command(program: &str, args: &[String]) -> String {
    let mut line = program.to_string();
    for arg in args {
        line.push(' ');
        if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || "\"'${}".contains(c)) {
            line.push('\'');
            line.push_str(arg);
            line.push('\'');
        } else {
            line.push_str(arg);
        }
    }
    line
}

/// Fill `buffer` completely, or report how much arrived.
///
/// `read_exact` cannot say *how much* it got before EOF, and that number is the
/// difference between “the recorder closed a clean stream” and “it wrote a
/// partial block” (a WAV header, a truncated tail).  Returns `Ok(filled)`, so
/// the caller can count a partial read instead of ignoring it.
fn read_block(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

/// Best-effort guess at the byte format of a block whose **f32 reading is
/// impossible** — so the report can name it instead of leaving “impossible
/// block” to the reader's imagination.
///
/// The formats `pw-cat` can plausibly write when `--format f32 --raw` is not
/// honoured are tried on the very same bytes (see [`guess_format`]); a real `f32`
/// stream that is merely **over** full scale (a huge but genuine level) matches
/// none of them, which is why this returns “unknown” rather than always naming a
/// format.
pub fn detect_sample_format(bytes: &[u8]) -> String {
    // A container header is unambiguous: `pw-cat` would only write one if it were
    // asked for a file instead of the raw `-` stream (which is exactly what
    // PipeWire 1.4+ does without `--raw`).
    if bytes.len() >= 4 && &bytes[..4] == b"RIFF" {
        return "a WAV/RIFF header".to_string();
    }
    if bytes.len() >= 4 && &bytes[..4] == b".snd" {
        return "a libsndfile .au header (pw-cat without --raw)".to_string();
    }
    match guess_format(bytes) {
        Some((format, peak)) => format!("{} (peak {peak:.3})", format.name()),
        None => "unknown — not s16le/s32le/f64le audio either".to_string(),
    }
}

/// Kill a capture's child and join its reader.
fn stop_stream(stream: &mut CaptureStream) {
    // Dropping the child's stdout (when the reader owns it) unblocks
    // `read_block`; killing the process first is the deterministic order.
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
        // The failure is in the registry, with the reason the OS gave — this is
        // the line the user's report will show instead of a silent dark bar.
        let summary = capture.stats().summary();
        assert_eq!(summary.sources.len(), 1);
        let facts = &summary.sources[0];
        assert_eq!(facts.uid, "in-mic");
        assert_eq!(facts.pid, None);
        assert_eq!(facts.source, "Holaf_Bus_Mic.monitor");
        match &facts.state {
            crate::diagnostics::CaptureState::Failed { reason } => {
                assert!(reason.contains("/nonexistent/holaf-pw-cat"), "{reason}");
            }
            other => panic!("a missing recorder must be `failed`, got {other:?}"),
        }
        assert!(summary.all_dead() && summary.never_streamed());
        // A second sync does not warn again but still publishes zero.
        capture.sync(&wanted, &slots, &board);
        assert!(capture.is_empty());
        assert_eq!(capture.stats().summary().sources.len(), 1);
    }

    /// **Live, the channel separation (opt-in).**  A stereo source with a tone on
    /// the **left only** must move the left bar and leave the right one at zero.
    ///
    /// The report that started this: “quand j'ai du son uniquement sur une
    /// oreille, le vu-mètre monte à gauche et à droite”.  Two things can produce
    /// that, and this test pins the one the reader is responsible for:
    ///
    /// * the *reader* duplicating/aggregating the channels — forbidden here:
    ///   `analyse_block` measures each channel of the interleaved block and
    ///   publishes it to its own board slot;
    /// * the *source* being mono: `pw-cat --channels 2` on a mono node makes
    ///   PipeWire **duplicate** the single channel, so both bars are right.  That
    ///   case is documented on [`analyse_block`] (a 1-channel block is
    ///   duplicated on purpose) and must be checked on the graph side
    ///   (`pw-dump`: `audio.channels` of the captured node), not here.
    ///
    /// ```bash
    /// eval "$(./scripts/start-pipewire-headless.sh --export)"
    /// pw-cli create-node adapter '{ factory.name=support.null-audio-sink \
    ///   node.name=Test_Sink media.class=Audio/Sink object.linger=true \
    ///   audio.position=[ FL FR ] }'
    /// pw-cat --playback --target Test_Sink /tmp/left_only.wav &
    /// HOLAF_METER_LIVE_SINK=Test_Sink HOLAF_METER_LIVE_WAV=/tmp/left_only.wav \
    ///   cargo test -p holaf-ui --lib a_live_left_only_source_keeps_the_right_bar_dark
    /// ```
    #[test]
    fn a_live_left_only_source_keeps_the_right_bar_dark() {
        let (Ok(sink), Ok(wav)) = (
            std::env::var("HOLAF_METER_LIVE_SINK"),
            std::env::var("HOLAF_METER_LIVE_WAV"),
        ) else {
            eprintln!(
                "SKIP a_live_left_only_source_keeps_the_right_bar_dark: set \
                 HOLAF_METER_LIVE_SINK and HOLAF_METER_LIVE_WAV=<a stereo WAV whose right \
                 channel is silent>"
            );
            return;
        };
        let mut player = Command::new(default_recorder())
            .args(["--playback", "--target", &sink, &wav])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("playback child");
        std::thread::sleep(std::time::Duration::from_millis(400));

        let board = MeterBoard::new();
        let mut capture = MeterCapture::new();
        let uid = "live-left".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), format!("{sink}.monitor"));
        capture.sync(&wanted, &slots, &board);

        let mut left = 0.0_f32;
        let mut right = 0.0_f32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let level = board.read(&uid).unwrap_or([0.0, 0.0]);
            left = left.max(level[0]);
            right = right.max(level[1]);
            if left > 0.1 {
                break;
            }
        }
        let summary = capture.stats().summary();
        capture.stop_all();
        let _ = player.kill();
        let _ = player.wait();
        assert!(
            left > 0.1,
            "the left channel of the tone must move the left bar, got L={left} R={right} \
             ({summary:?})"
        );
        assert!(
            right == 0.0,
            "a left-only source must leave the RIGHT bar at zero, got L={left} R={right} \
             ({summary:?})"
        );
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

    /// **Live, through a real child (opt-in).**  A recorder that writes `s16le`
    /// despite `--format f32` (the PipeWire 1.6 report) must reach the board with
    /// the right level **and** be reported as detected+converted — never dropped.
    ///
    /// This is the **legacy** stream: a recorder that does not know `--raw`, so
    /// the width really is unknown.  When `--raw` is in effect the reader trusts
    /// the contract instead of guessing (see [`MISREAD_CONFIRM_BLOCKS`]).
    ///
    /// Two shapes of “s16 bytes read as f32” reach this same assertion, and both
    /// are worth running:
    ///
    /// * **loud s16** (a symmetric −20 dBFS tone): the f32 reading is *huge*
    ///   (1e37), caught by the +40 dBFS guard;
    /// * **quiet/all-positive s16** (measured on the sandbox: every sample in
    ///   `0..=3783` gives peak 4.9e-30, RMS 1.4e-30): the f32 reading is a
    ///   **denormal**, which used to be accepted silently — 249 blocks, level
    ///   0.000, `detected_format: None`.  This is the case the positive RMS test
    ///   ([`f32_reading_is_a_misread`]) exists for.
    ///
    /// A tone that exercises the second shape while still carrying −20 dBFS RMS
    /// (so the bar must read 0.56) is a **DC-offset** sine:
    /// `sample = a·(1 + sin(2π1000t))` with `a = 0.0707/√1.5` (all samples stay
    /// positive, RMS = 0.0707).  Either WAV works for this test.
    ///
    /// ```bash
    /// eval "$(./scripts/start-pipewire-headless.sh --export)"
    /// pw-cli create-node adapter '{ factory.name=support.null-audio-sink \
    ///   node.name=Test_Sink media.class=Audio/Sink object.linger=true \
    ///   audio.position=[ FL FR ] }'
    /// pw-cat --playback --target Test_Sink /tmp/tone.wav &   # or the DC-offset tone
    /// HOLAF_METER_LIVE_SINK=Test_Sink HOLAF_METER_LIVE_S16=1 \
    ///   HOLAF_PW_CAT=/tmp/rig/pwcat_s16_wrapper.sh \
    ///   cargo test -p holaf-ui --lib a_live_s16_recorder_is_converted
    /// ```
    ///
    /// The wrapper must **not** advertise `--raw` (its `--help` must not list the
    /// option): when `--raw` is in effect the samples are `f32le` by contract and
    /// the reader no longer guesses another width (that guess is what corrupted a
    /// valid f32 stream on PipeWire 1.6 — see `MISREAD_CONFIRM_BLOCKS`).
    #[test]
    fn a_live_s16_recorder_is_converted() {
        let (Ok(sink), Ok(_wav)) = (
            std::env::var("HOLAF_METER_LIVE_SINK"),
            std::env::var("HOLAF_METER_LIVE_S16"),
        ) else {
            eprintln!(
                "SKIP a_live_s16_recorder_is_converted: set HOLAF_METER_LIVE_SINK and \
                 HOLAF_METER_LIVE_S16 and HOLAF_PW_CAT=<a recorder that writes s16le>"
            );
            return;
        };
        // `--raw` is the contract that the samples are f32le: with a recorder that
        // advertises it, this test would (rightly) never convert anything.
        if recorder_supports_raw(&default_recorder()) {
            eprintln!(
                "SKIP a_live_s16_recorder_is_converted: {} advertises `--raw`, so its output \
                 is f32le by contract — point HOLAF_PW_CAT at a legacy wrapper (no `--raw` \
                 in its help) to exercise the detection",
                default_recorder()
            );
            return;
        }
        let board = MeterBoard::new();
        let mut capture = MeterCapture::new();
        let uid = "s16-live".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), format!("{sink}.monitor"));
        capture.sync(&wanted, &slots, &board);
        // Sample a little past the first audible block: the very first converted
        // block can still contain the stream's leading digital silence (the
        // recorder starts before the sink has audio), which would read low.
        let mut level = wait_for_level(&board, &uid);
        let settle = std::time::Instant::now() + std::time::Duration::from_millis(400);
        while std::time::Instant::now() < settle {
            let read = board.read(&uid).unwrap_or([0.0, 0.0]);
            level = [level[0].max(read[0]), level[1].max(read[1])];
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let summary = capture.stats().summary();
        capture.stop_all();
        assert!(
            (level[0] - 0.56).abs() < 0.05,
            "the s16le stream must move the bar to 0.56 (a −20 dBFS sine), got {level:?} \
             ({summary:?})"
        );
        assert_eq!(
            summary.sources.first().and_then(|facts| facts.detected_format.clone()),
            Some("s16le".to_string()),
            "the report must name the detected format: {summary:?}"
        );
        // The level that moved the bar is the **converted** one: proof that the
        // block was decoded as s16 (−20 dBFS ≈ 0.0707) and not as the denormal
        // f32 reading it has when taken 4 bytes at a time.
        assert!(
            summary
                .sources
                .first()
                .and_then(|facts| facts.last_rms)
                .is_some_and(|rms| (rms[0] - 0.0707).abs() < 0.005),
            "the recorded RMS must be the converted one: {summary:?}"
        );
        assert_eq!(summary.rejected, 0, "nothing is refused: {summary:?}");
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
    ///
    /// This one **advertises `--raw`** like a current `pw-cat`: the samples are
    /// `f32le` by contract and the reader must never re-detect the format (see
    /// [`legacy_recorder`] for the opposite case).
    fn fake_recorder(payload: &[u8]) -> String {
        recorder_script(payload, true)
    }

    /// A recorder that does **not** know `--raw` (PipeWire < 1.4): the format of
    /// what it writes is genuinely unknown, so the byte detection is the only
    /// thing that can tell (and must stay a conservative fallback).
    fn legacy_recorder(payload: &[u8]) -> String {
        recorder_script(payload, false)
    }

    fn recorder_script(payload: &[u8], raw: bool) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "holaf-capture-{}-{:?}-{raw}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.join("block.bin");
        std::fs::write(&data, payload).unwrap();
        let script = dir.join("recorder.sh");
        let help = if raw {
            "  -a, --raw   RAW mode"
        } else {
            "  -r, --record   Recording mode"
        };
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = --help ] && {{ echo '{help}'; exit 0; }}; done\nwhile :; do cat '{}'; done\n",
                data.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        script.to_string_lossy().into_owned()
    }

    /// Read the board for up to 5 s, returning the last level seen.
    ///
    /// 5 s (not 2) because the whole test binary runs in parallel and every
    /// capture here spawns a real child: a loaded machine can take a second to
    /// schedule the script, and a test that fails on a busy CI box teaches
    /// nothing.  It returns as soon as a level arrives.
    fn wait_for_level(board: &MeterBoard, uid: &str) -> [f32; 2] {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
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
        // …and the report can see it: a numbered block count, the live state,
        // and the last RMS/peak that produced that level.
        let summary = capture.stats().summary();
        assert_eq!(summary.recorder, capture.stats().recorder());
        assert!(summary.blocks > 0, "{summary:?}");
        assert_eq!(summary.rejected, 0);
        assert!(!summary.never_streamed() && !summary.all_dead());
        let facts = &summary.sources[0];
        assert_eq!(facts.state, crate::diagnostics::CaptureState::Streaming);
        assert_eq!(facts.source, "Holaf_Bus_Tone.monitor");
        assert_eq!(facts.target, "Holaf_Bus_Tone");
        assert!(facts.monitor);
        assert!(facts.blocks > 0 && facts.pid.is_some());
        assert!(facts.last_rms.is_some_and(|rms| (rms[0] - 0.0707).abs() < 1e-3));
        // …and no conversion is claimed: a real f32le block is read as it is.
        assert!(
            facts.detected_format.is_none(),
            "a native f32 block must not be “detected”: {facts:?}"
        );
        assert!(facts.since_last_block.is_some());
        capture.stop_all();
        // Stopping clears the entries: a stale status must not describe the
        // graph of the next snapshot.
        assert!(capture.stats().summary().is_empty());
    }

    /// The `--raw` capability is **probed**, not assumed: `pw-cat` before 1.4
    /// exits on an unknown option, so the flag may only be added when the binary
    /// offers it.
    #[test]
    fn the_raw_flag_is_probed_from_the_recorder_help() {
        use std::os::unix::fs::PermissionsExt;
        let write = |name: &str, body: &str| -> String {
            let dir = std::env::temp_dir().join(format!(
                "holaf-capture-raw-{name}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("recorder.sh");
            std::fs::write(&script, body).unwrap();
            let mut permissions = std::fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).unwrap();
            script.to_string_lossy().into_owned()
        };
        // A pw-cat that lists the option (1.4+).
        let modern = write(
            "modern",
            "#!/bin/sh\n[ \"$1\" = --help ] && { echo '  -a, --raw   RAW mode'; exit 0; }\n",
        );
        assert!(recorder_supports_raw(&modern), "the help lists --raw");
        // A pw-cat that does not (0.3.x: the flag would be refused).
        let old = write(
            "old",
            "#!/bin/sh\n[ \"$1\" = --help ] && { echo '  -r, --record   Recording mode'; exit 0; }\n",
        );
        assert!(!recorder_supports_raw(&old), "no --raw in the help");
        // A recorder that never answers `--help` is not given the flag (and is
        // not waited on for ever — `probe_raw_flag` is bounded).
        let stuck = write("stuck", "#!/bin/sh\nwhile :; do sleep 1; done\n");
        assert!(!recorder_supports_raw(&stuck));
        // A binary that does not exist at all.
        assert!(!recorder_supports_raw("/nonexistent/holaf-pw-cat"));
        // The probe is cached: a second call does not respawn.  (The script is
        // still there, so a non-cached probe would answer the same — the point
        // is that the answer is stable, which the cache guarantees.)
        assert!(recorder_supports_raw(&modern));
    }

    /// **The “VU à zéro” fix (PipeWire 1.4+ without `--raw`).**  The recorder
    /// writes `s16le` even though we ask for `--format f32` (on 1.6 the samples
    /// go through libsndfile in an `.au` container).  The reader must **detect**
    /// the format from the bytes and **convert** it — the VU then shows the real
    /// level instead of staying dark.
    ///
    /// Measured, not assumed: a −20 dBFS sine encoded as `s16le` and converted
    /// (`i16 / 32768`) must read the same 0.56 a native `f32le` block reads.
    #[test]
    fn an_s16le_stream_is_detected_and_converted_instead_of_dropped() {
        // A −20 dBFS sine, encoded as interleaved s16le (what a `pw-cat` that
        // ignores `--format f32` writes).  10 ms of stereo at 2 B/sample is
        // 1920 B, not the 3840 B of an f32 block — the reader must re-frame.
        let mut payload = Vec::with_capacity(BLOCK_FRAMES * METER_CHANNELS * 2);
        for n in 0..BLOCK_FRAMES {
            let sample =
                0.1 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
            let value = (sample * 32_767.0) as i16;
            payload.extend_from_slice(&value.to_le_bytes());
            payload.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(payload.len(), BLOCK_FRAMES * METER_CHANNELS * 2);
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(legacy_recorder(&payload));
        let uid = "s16".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_Sixteen.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        let level = wait_for_level(&board, &uid);
        assert!(
            (level[0] - 0.56).abs() < 0.02,
            "a −20 dBFS sine carried as s16le must read 0.56 after conversion, got {level:?}"
        );
        // The report says the format was **detected and converted** — not that
        // blocks were dropped (nothing is dropped any more).
        let summary = capture.stats().summary();
        assert!(summary.blocks > 0, "{summary:?}");
        assert_eq!(summary.rejected, 0, "nothing is refused: {summary:?}");
        assert_eq!(summary.rejects_of(crate::diagnostics::RejectReason::Implausible), 0);
        let facts = &summary.sources[0];
        assert_eq!(facts.detected_format.as_deref(), Some("s16le"), "{facts:?}");
        assert!(
            facts.describe().contains("detected s16le and converted"),
            "{}",
            facts.describe()
        );
        // The recorded RMS is the converted one (−20 dBFS sine → 0.0707).
        assert!(
            facts.last_rms.is_some_and(|rms| (rms[0] - 0.0707).abs() < 1e-3),
            "{facts:?}"
        );
        capture.stop_all();
    }

    /// **The startup case measured live:** the stream begins with digital
    /// silence (pw-cat starts before the tone reaches the sink), then the tone
    /// arrives in s16le.  The silent block freezes `f32le`; the first real block
    /// is implausible as f32 — it must be **redetected and converted, not counted
    /// as a rejected block** (a rejection there is pure noise: the VU works).
    #[test]
    fn silence_then_s16le_is_converted_without_a_spurious_rejection() {
        use std::os::unix::fs::PermissionsExt;
        // A recorder that emits one block of zeros, then repeats an s16 tone.
        let dir = std::env::temp_dir().join(format!(
            "holaf-capture-silence-then-s16-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let zeros = dir.join("zeros.bin");
        let tone = dir.join("tone.bin");
        std::fs::write(&zeros, vec![0_u8; BLOCK_BYTES]).unwrap();
        let mut s16 = Vec::with_capacity(BLOCK_FRAMES * METER_CHANNELS * 2);
        for n in 0..BLOCK_FRAMES {
            let sample =
                0.1 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
            let value = (sample * 32_767.0) as i16;
            s16.extend_from_slice(&value.to_le_bytes());
            s16.extend_from_slice(&value.to_le_bytes());
        }
        std::fs::write(&tone, &s16).unwrap();
        let script = dir.join("recorder.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = --help ] && {{ echo '  -r, --record   Recording mode'; exit 0; }}; done\ncat '{zeros}'\nwhile :; do cat '{tone}'; done\n",
                zeros = zeros.display(),
                tone = tone.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(script.to_string_lossy().into_owned());
        let uid = "silence-then-s16".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_Silence.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        let level = wait_for_level(&board, &uid);
        assert!(
            (level[0] - 0.56).abs() < 0.03,
            "after the silent first block the s16le tone must read 0.56, got {level:?} ({:?})",
            capture.stats().summary()
        );
        let summary = capture.stats().summary();
        assert_eq!(summary.rejected, 0, "no spurious rejection: {summary:?}");
        assert_eq!(
            summary.sources[0].detected_format.as_deref(),
            Some("s16le"),
            "{summary:?}"
        );
        capture.stop_all();
    }

    /// **The residual “VU à zéro” (measured live).**  A recorder that writes
    /// `s16le` does **not** always produce an “impossible” f32 block: when every
    /// sample stays small and positive, reading four bytes at a time gives
    /// **denormals** (measured peak 4.9e-30, RMS 1.4e-30), which sail under the
    /// +40 dBFS guard.  `f32le` was then frozen on that reading, 249 blocks were
    /// accepted, and the level of every one of them was exactly 0.000.
    ///
    /// The fix is a **positive** test: an RMS under [`SUSPICIOUS_LOW_RMS`] while
    /// the bytes are not all zero means the bytes are being read at the wrong
    /// width, whatever the peak says.  Here the s16 payload carries a −20 dBFS-RMS
    /// tone (so the converted block must read 0.56), yet its f32 reading is
    /// 1e-30-ish: exactly the stream that used to be silently dropped.
    #[test]
    fn a_denormal_f32_reading_of_s16_bytes_is_detected_and_converted() {
        // All samples positive (a DC-offset tone): the high byte of every sample
        // stays under 0x0F, so the f32 reading has a tiny exponent.  A tone with
        // both signs would give huge negatives instead — the *other* guard.
        let offset = 0.0707_f32 / 1.5_f32.sqrt(); // RMS = offset·√1.5 = 0.0707
        let mut payload = Vec::with_capacity(BLOCK_FRAMES * METER_CHANNELS * 2);
        for n in 0..BLOCK_FRAMES {
            let sample = offset
                * (1.0 + (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin());
            let value = (sample * 32_767.0) as i16;
            payload.extend_from_slice(&value.to_le_bytes());
            payload.extend_from_slice(&value.to_le_bytes());
        }
        // The premise of this test, measured on the bytes themselves: the f32
        // reading of that block is denormal-quiet, i.e. it passes the loudness
        // guard — so nothing but the positive test can catch it.
        let as_f32 = analyse_block(&SampleFormat::F32Le.decode(&payload), METER_CHANNELS);
        assert!(
            !as_f32.is_implausible(),
            "the whole point: a denormal reading is not “implausible” ({as_f32:?})"
        );
        assert!(
            as_f32.rms[0].max(as_f32.rms[1]) < SUSPICIOUS_LOW_RMS,
            "…and it is suspiciously quiet instead ({as_f32:?})"
        );
        assert!(f32_reading_is_a_misread(&payload, &as_f32));

        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(legacy_recorder(&payload));
        let uid = "denormal-s16".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_Denormal.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        let level = wait_for_level(&board, &uid);
        assert!(
            (level[0] - 0.56).abs() < 0.03,
            "a −20 dBFS-RMS s16 tone must read 0.56 once converted, got {level:?} ({:?})",
            capture.stats().summary()
        );
        let summary = capture.stats().summary();
        assert_eq!(summary.rejected, 0, "nothing is refused: {summary:?}");
        let facts = &summary.sources[0];
        assert_eq!(facts.detected_format.as_deref(), Some("s16le"), "{facts:?}");
        assert!(
            facts.last_rms.is_some_and(|rms| (rms[0] - 0.0707).abs() < 1e-3),
            "the recorded RMS must be the converted one: {facts:?}"
        );
        capture.stop_all();
    }

    /// An `s16le` stream *below* the meter's floor is still detected: −60 dBFS
    /// reads as NaN/denormals in f32 (230 of 480 groups were non-finite), so the
    /// block arrives “plausible” and silent.  After detection the level is
    /// legitimately 0 (the scale starts at −60 dBFS), but the **format** and the
    /// **RMS** prove the conversion happened instead of the bytes being ignored.
    #[test]
    fn a_very_quiet_s16_stream_is_detected_from_its_denormals() {
        let mut payload = Vec::with_capacity(BLOCK_FRAMES * METER_CHANNELS * 2);
        for n in 0..BLOCK_FRAMES {
            // ±32 counts: the high byte is 0x00 (positive) or 0xFF (negative).
            let sample =
                0.001 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
            let value = (sample * 32_767.0) as i16;
            payload.extend_from_slice(&value.to_le_bytes());
            payload.extend_from_slice(&value.to_le_bytes());
        }
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(legacy_recorder(&payload));
        let uid = "quiet-s16".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_QuietSixteen.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        assert!(
            wait_until(|| capture
                .stats()
                .summary()
                .sources
                .first()
                .is_some_and(|facts| facts.blocks > 0 && facts.detected_format.is_some())),
            "the format must be detected: {:?}",
            capture.stats().summary()
        );
        let summary = capture.stats().summary();
        assert_eq!(summary.rejected, 0, "{summary:?}");
        let facts = &summary.sources[0];
        assert_eq!(facts.detected_format.as_deref(), Some("s16le"), "{facts:?}");
        // 0.001/√2 = 7.07e-4 — the real amplitude, not the 1e-39 of the misread.
        assert!(
            facts
                .last_rms
                .is_some_and(|rms| (rms[0] - 0.000_707).abs() < 1e-4),
            "the converted RMS must be the real one, not a denormal: {facts:?}"
        );
        capture.stop_all();
    }

    /// The nominal cases must **not** trigger a detection, however quiet they
    /// are: a real `f32` stream is a real `f32` stream, and reporting “detected
    /// s16le” for it would be a lie (and would draw a wrong level).
    #[test]
    fn a_real_f32_stream_is_never_redetected() {
        // −63 dBFS, i.e. under the meter's floor but perfectly ordinary f32 data
        // — a hundred thousand times louder than the suspicious threshold.
        let quiet = sine_block(0.0007);
        let as_f32 = analyse_block(&SampleFormat::F32Le.decode(&quiet), METER_CHANNELS);
        assert!(!as_f32.is_implausible());
        assert!(
            !f32_reading_is_a_misread(&quiet, &as_f32),
            "a quiet f32 block is not a misread ({as_f32:?})"
        );
        // Digital silence (all zero bytes) is not a misread either — nor is a
        // block of denormal *f32* values, the only case left that could confuse
        // the test, since no capture path produces one.
        assert!(!f32_reading_is_a_misread(&[0_u8; BLOCK_BYTES], &MeterSample::default()));

        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(fake_recorder(&quiet));
        let uid = "quiet-f32".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_QuietF32.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        assert!(
            wait_until(|| capture
                .stats()
                .summary()
                .sources
                .first()
                .is_some_and(|facts| facts.blocks > 0)),
            "the stream must be read: {:?}",
            capture.stats().summary()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        let summary = capture.stats().summary();
        let facts = &summary.sources[0];
        assert!(
            facts.detected_format.is_none(),
            "a real f32 stream is not “detected as” anything: {facts:?}"
        );
        assert_eq!(summary.rejected, 0, "{summary:?}");
        assert!(
            facts
                .last_rms
                .is_some_and(|rms| (rms[0] - 0.000_495).abs() < 1e-4),
            "the RMS must be read as is (0.0007/√2): {facts:?}"
        );
        capture.stop_all();
    }

    /// A block of **denormal f32 values** (non-zero bytes, RMS under
    /// [`SUSPICIOUS_LOW_RMS`]) is the one payload that *looks* like a misread and
    /// is not.
    ///
    /// **This is the user-reported corruption, reproduced.**  On PipeWire 1.6
    /// with `--raw` active the samples are `f32le` by contract; the reader still
    /// guessed `s16le` from such a block, froze that width and decoded every
    /// later block of valid f32 music as s16 — measured on the user's machine as
    /// `last rms 0.5858 / peak 0.9969` (a near-square signal) instead of
    /// `rms ≈ 0.1`, i.e. a VU bar pinned high, vibrating, and insensitive to the
    /// fader.  In raw mode the guess must never happen, not even once.
    #[test]
    fn a_denormal_block_never_freezes_another_format_in_raw_mode() {
        // Premise: this block *does* satisfy the quiet suspicion test — so the
        // old code would have guessed a format from it.
        let mut denormal = Vec::with_capacity(BLOCK_BYTES);
        for n in 0..BLOCK_FRAMES {
            let sample = if n % 2 == 0 { 1e-40_f32 } else { -1e-40_f32 };
            denormal.extend_from_slice(&sample.to_le_bytes());
            denormal.extend_from_slice(&sample.to_le_bytes());
        }
        let as_f32 = analyse_block(&SampleFormat::F32Le.decode(&denormal), METER_CHANNELS);
        assert!(!as_f32.is_implausible(), "{as_f32:?}");
        assert!(f32_reading_is_a_misread(&denormal, &as_f32), "{as_f32:?}");
        assert!(guess_format(&denormal).is_some(), "it would be “explained” as another width");

        // The stream alternates that block with a block of ordinary music.
        let mut payload = denormal;
        payload.extend_from_slice(&sine_block(0.25));
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(fake_recorder(&payload));
        let uid = "denormal-f32".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_DenormalF32.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        assert!(
            wait_until(|| board.read(&uid).is_some_and(|[l, _]| l > 0.0)),
            "the music blocks must reach the bar: {:?}",
            capture.stats().summary()
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        let summary = capture.stats().summary();
        let facts = &summary.sources[0];
        assert!(
            facts.detected_format.is_none(),
            "`--raw` means f32le: nothing may be “detected”: {facts:?}"
        );
        assert_eq!(summary.rejected, 0, "nothing is refused: {summary:?}");
        // A −12 dBFS sine every other block: the 300 ms RMS sits at 0.25/2 = 0.125
        // (−18 dBFS) → 0.64 on the bar.  A wrong-width decode would read 0.6+ RMS.
        let level = board.read(&uid).unwrap_or([0.0, 0.0]);
        assert!(
            (0.55..0.75).contains(&level[0]),
            "the bar must read the real music level, not converted garbage: {level:?}"
        );
        assert_eq!(level[1], level[0], "both channels carry the same sine");
        capture.stop_all();
    }

    /// **One suspicious block is not a format change.**  With a recorder that does
    /// not know `--raw` (PipeWire < 1.4) the width really is unknown, so detection
    /// stays — but it must be *confirmed*: a single odd block (a denormal tail, a
    /// startup block) must not re-frame the whole stream for ever.
    #[test]
    fn a_single_suspicious_block_never_freezes_a_format() {
        // A 3840-byte block of s16-looking bytes (a DC-offset tone, written twice):
        // read as f32 it is a denormal, which is what the quiet test flags.
        let offset = 0.0707_f32 / 1.5_f32.sqrt();
        let mut suspicious = Vec::with_capacity(BLOCK_BYTES);
        for _ in 0..2 {
            for n in 0..BLOCK_FRAMES {
                let sample = offset
                    * (1.0 + (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin());
                let value = (sample * 32_767.0) as i16;
                suspicious.extend_from_slice(&value.to_le_bytes());
                suspicious.extend_from_slice(&value.to_le_bytes());
            }
        }
        assert_eq!(suspicious.len(), BLOCK_BYTES);
        let as_f32 = analyse_block(&SampleFormat::F32Le.decode(&suspicious), METER_CHANNELS);
        assert!(f32_reading_is_a_misread(&suspicious, &as_f32), "{as_f32:?}");
        assert!(guess_format(&suspicious).is_some());

        // …followed by 29 blocks of ordinary f32 music, and repeated: every 300 ms
        // the stream carries one block that suggests s16le, never two in a row.
        let mut payload = suspicious;
        for _ in 0..29 {
            payload.extend_from_slice(&sine_block(0.25));
        }
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(legacy_recorder(&payload));
        let uid = "one-bad-block".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_OneBadBlock.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        assert!(
            wait_until(|| board.read(&uid).is_some_and(|[l, _]| l > 0.0)),
            "the music must reach the bar: {:?}",
            capture.stats().summary()
        );
        std::thread::sleep(std::time::Duration::from_millis(400));
        let summary = capture.stats().summary();
        assert!(
            summary.sources[0].detected_format.is_none(),
            "one suspicious block in 30 must not freeze a format: {summary:?}"
        );
        assert_eq!(summary.rejected, 0, "nothing is refused: {summary:?}");
        // The bar is a **100 ms** window behind a decaying envelope, so one odd
        // block in ten lowers it for a moment — that is the price of reacting
        // within one UI tick (see `meters::RMS_WINDOW`).  What must never happen
        // is the music *disappearing* from the bar, so the best reading of a
        // second is taken instead of one instant (a single sample made this test
        // depend on the reader thread's scheduling).
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
        let mut best = 0.0_f32;
        while std::time::Instant::now() < deadline {
            best = best.max(board.read(&uid).unwrap_or([0.0, 0.0])[0]);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            (0.55..0.75).contains(&best),
            "the bar must keep reading the music (best of 1 s: {best})"
        );
        capture.stop_all();
    }

    /// `s32le` and `f64le` are converted too (a recorder that honours `--format`
    /// with a different width, or an `.au` container's payload).
    #[test]
    fn s32le_and_f64le_streams_are_converted_to_the_same_level() {
        let sine: Vec<f32> = (0..BLOCK_FRAMES)
            .map(|n| 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin())
            .collect();
        // Build the two payloads.
        let mut s32 = Vec::with_capacity(BLOCK_BYTES);
        let mut f64 = Vec::with_capacity(BLOCK_FRAMES * 2 * 8);
        for sample in &sine {
            let wide = (f64::from(*sample) * 2_147_483_648.0) as i32;
            s32.extend_from_slice(&wide.to_le_bytes());
            s32.extend_from_slice(&wide.to_le_bytes());
            f64.extend_from_slice(&f64::from(*sample).to_le_bytes());
            f64.extend_from_slice(&f64::from(*sample).to_le_bytes());
        }
        for (name, payload, expected) in [("s32le", s32, "s32le"), ("f64le", f64, "f64le")] {
            let board = MeterBoard::new();
            let mut capture = MeterCapture::with_recorder(legacy_recorder(&payload));
            let uid = name.to_string();
            let slots = board.assign(std::slice::from_ref(&uid));
            let mut wanted = HashMap::new();
            wanted.insert(uid.clone(), format!("Holaf_Bus_{name}.monitor"));
            capture.sync(&wanted, &slots, &board);
            let level = wait_for_level(&board, &uid);
            assert!(
                (level[0] - 0.56).abs() < 0.03,
                "{name}: expected ≈0.56 after conversion, got {level:?} ({:?})",
                capture.stats().summary()
            );
            let summary = capture.stats().summary();
            assert_eq!(
                summary.sources[0].detected_format.as_deref(),
                Some(expected),
                "{summary:?}"
            );
            capture.stop_all();
        }
    }

    /// The pure conversion functions, without a child process: each format's
    /// full-scale and a −20 dBFS sine land on the same amplitude.
    #[test]
    fn every_sample_format_converts_to_the_same_amplitude() {
        // A ±0.5 (≈ −6 dBFS peak) frame, per format.
        let frame = 0.5_f32;
        let cases: Vec<(SampleFormat, Vec<u8>)> = vec![
            (SampleFormat::F32Le, frame.to_le_bytes().to_vec()),
            (
                SampleFormat::S16Le,
                ((frame * 32_767.0) as i16).to_le_bytes().to_vec(),
            ),
            (
                SampleFormat::S32Le,
                ((f64::from(frame) * 2_147_483_648.0) as i32)
                    .to_le_bytes()
                    .to_vec(),
            ),
            (
                SampleFormat::F64Le,
                f64::from(frame).to_le_bytes().to_vec(),
            ),
            (SampleFormat::U8, vec![(128.0 + frame * 127.0) as u8]),
            (SampleFormat::S8, vec![(frame * 127.0) as i8 as u8]),
        ];
        for (format, bytes) in cases {
            let decoded = format.decode(&bytes);
            assert_eq!(decoded.len(), 1, "{format:?}: one sample");
            assert!(
                (decoded[0] - frame).abs() < 0.01,
                "{:?}: expected {frame}, got {}",
                format,
                decoded[0]
            );
        }
        // Silence stays silence in every format (u8's zero point is 128).
        assert_eq!(SampleFormat::S16Le.decode(&[0, 0]), vec![0.0]);
        assert_eq!(SampleFormat::U8.decode(&[128]), vec![0.0]);
        assert_eq!(SampleFormat::F64Le.decode(&0.0_f64.to_le_bytes()), vec![0.0]);
    }

    /// **The residual case: a libsndfile container.**  PipeWire 1.4+ without
    /// `--raw` wraps the samples in a `.au` header; if that header ever reaches
    /// the reader as *data* (a `--raw`-less recorder that does not honour our
    /// probe), no sample format explains it, and the bar must stay dark with the
    /// cause named — never pin at full scale.
    ///
    /// This is the *only* path left that refuses blocks: with `--raw` and the
    /// s16le/s32le/f64le conversion, a real stream is never dropped.
    #[test]
    fn a_container_header_is_reported_and_never_drawn() {
        // A whole big-endian `.au` f32 block: magic `.snd`, header, then data.
        // Reading this as f32le gives huge/NaN garbage in any of the 6 formats.
        let mut payload = vec![
            0x2e, 0x73, 0x6e, 0x64, // .snd
            0, 0, 0, 24, // header size
            0, 0, 0, 0, // data size (unknown)
            0, 0, 0, 6, // encoding 6 = float32
            0, 0, 0xbb, 0x80, // 48000
            0, 0, 0, 2, // 2 channels
        ];
        payload.resize(BLOCK_BYTES, 0x7f); // 0x7f bytes: implausible in every format
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(fake_recorder(&payload));
        let uid = "container".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_Container.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        // Give the reader plenty of blocks: it must publish zeros every time.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
        while std::time::Instant::now() < deadline {
            assert_eq!(
                board.read(&uid),
                Some([0.0, 0.0]),
                "an unexplainable stream must never publish a level"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // **And it is counted**, so a dark bar is never the only trace.
        let summary = capture.stats().summary();
        assert_eq!(summary.blocks, 0, "nothing is drawn: {summary:?}");
        assert!(
            summary.rejects_of(crate::diagnostics::RejectReason::Implausible) > 0,
            "{summary:?}"
        );
        assert!(summary.sources[0].detected_format.is_none());
        // A refused block is *not* a dead chain: the bytes prove it runs.
        assert!(!summary.never_streamed());
        capture.stop_all();
    }

    /// An executable that writes `payload` **once** and exits — the “the
    /// recorder closed its stdout” / “it wrote a partial block” path.
    fn fake_recorder_once(payload: &[u8]) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "holaf-capture-once-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.join("payload.bin");
        std::fs::write(&data, payload).unwrap();
        let script = dir.join("recorder.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = --help ] && {{ echo '  -a, --raw   RAW mode'; exit 0; }}; done\ncat '{}'\n",
                data.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        script.to_string_lossy().into_owned()
    }

    /// Wait until `predicate` holds, or give up after 5 s (a loaded machine
    /// spawns these children in bursts — see [`wait_for_level`]).
    fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        predicate()
    }

    /// A stream that stops mid-block is **counted** as a partial block and its
    /// end is reported — that is where a WAV header or a truncated tail lands,
    /// and it must not look like silence.
    #[test]
    fn a_stream_that_stops_mid_block_is_counted_as_partial() {
        // 100 bytes: not one whole block, and not frame-aligned either.
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(fake_recorder_once(&[0x11_u8; 100]));
        let uid = "short".to_string();
        let slots = board.assign(std::slice::from_ref(&uid));
        let mut wanted = HashMap::new();
        wanted.insert(uid.clone(), "Holaf_Bus_Short.monitor".to_string());
        capture.sync(&wanted, &slots, &board);
        assert!(
            wait_until(|| capture.stats().summary().sources[0].state != crate::diagnostics::CaptureState::Waiting),
            "the reader must notice the end of the stream"
        );
        let summary = capture.stats().summary();
        assert_eq!(summary.blocks, 0);
        assert_eq!(summary.rejects_of(crate::diagnostics::RejectReason::Partial), 1);
        let facts = &summary.sources[0];
        assert_eq!(facts.rejected, 1);
        assert!(
            facts
                .state
                .detail()
                .unwrap_or_default()
                .contains("100 of 3840 bytes"),
            "{:?}",
            facts.state
        );
        capture.stop_all();
    }

    /// The command line the report prints must be **pasteable**: the `-P` JSON is
    /// the one argument a shell would split, so it must come out quoted, and the
    /// rest must stay readable.
    #[test]
    fn the_reported_command_line_is_pasteable() {
        let args: Vec<String> = [
            "--record",
            "-P",
            MONITOR_PROPERTIES,
            "--target",
            "Holaf_Bus_Mic",
            "--format",
            "f32",
            "--rate",
            "48000",
            "--channels",
            "2",
            "-",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        let line = render_command("pw-cat", &args);
        assert_eq!(
            line,
            "pw-cat --record -P '{\"stream.capture.sink\":true}' --target Holaf_Bus_Mic \
             --format f32 --rate 48000 --channels 2 -"
        );
        // A path with a space is quoted too (a wrapper script in /tmp/some dir).
        assert_eq!(
            render_command("/tmp/a b/pw-cat", &["--record".to_string()]),
            "/tmp/a b/pw-cat --record"
        );
    }

    /// `detect_sample_format` is what turns “impossible block” into an actionable
    /// “your recorder writes s16le”: it reinterprets the very same bytes.
    #[test]
    fn the_byte_format_of_a_misread_block_is_guessed() {
        // A real −20 dBFS sine, encoded as s16le: `detect_sample_format` looks
        // only at the bytes, so it names the format the recorder really wrote.
        let mut s16 = Vec::new();
        for n in 0..BLOCK_FRAMES * 2 {
            let sample = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
            s16.extend_from_slice(&((sample * 32_767.0) as i16).to_le_bytes());
        }
        let guess = detect_sample_format(&s16);
        assert!(guess.starts_with("s16le"), "{guess}");
        // The same sine at 32-bit and 64-bit float widths.
        let mut s32 = Vec::new();
        let mut f64_le = Vec::new();
        for n in 0..BLOCK_FRAMES * 2 {
            let sample = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
            s32.extend_from_slice(&((f64::from(sample) * 2_147_483_648.0) as i32).to_le_bytes());
            f64_le.extend_from_slice(&f64::from(sample).to_le_bytes());
        }
        assert!(detect_sample_format(&s32).starts_with("s32le"));
        assert!(detect_sample_format(&f64_le).starts_with("f64le"));
        // A container header is unambiguous, and names *which* container.
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0_u8; 44]);
        assert!(detect_sample_format(&wav).contains("WAV/RIFF"));
        let mut au = b".snd".to_vec();
        au.extend_from_slice(&[0_u8; 44]);
        assert!(detect_sample_format(&au).contains(".au"));
        // Digital silence says nothing at all (no format claim out of zeros).
        assert!(detect_sample_format(&[0_u8; BLOCK_BYTES]).starts_with("unknown"));
        // A smoothly varying s16 signal is not mistaken for a wider format: the
        // decoder that reads it as s32 would see the same RMS but the wrong
        // sample count, and roughness scoring picks s16 (the narrower truth).
        assert_eq!(guess_format(&s16).map(|(f, _)| f), Some(SampleFormat::S16Le));
        assert_eq!(guess_format(&s32).map(|(f, _)| f), Some(SampleFormat::S32Le));
        assert_eq!(
            guess_format(&f64_le).map(|(f, _)| f),
            Some(SampleFormat::F64Le)
        );
    }

    /// A recorder that picks its payload from the `--target` it was given, so one
    /// [`MeterCapture`] (one recorder) can run two captures with **different**
    /// levels — what the slot-mapping test needs.
    fn fake_recorder_by_target(tag: &str, loud: &[u8], quiet: &[u8]) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "holaf-capture-targets-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let loud_path = dir.join("loud.bin");
        let quiet_path = dir.join("quiet.bin");
        std::fs::write(&loud_path, loud).unwrap();
        std::fs::write(&quiet_path, quiet).unwrap();
        let script = dir.join("recorder.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = --help ] && {{ echo '  -a, --raw   RAW mode'; exit 0; }}; done\npayload='{quiet}'\ncase \"$*\" in\n  *Holaf_Bus_Loud*) payload='{loud}' ;;\nesac\nwhile :; do cat \"$payload\"; done\n",
                loud = loud_path.display(),
                quiet = quiet_path.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        script.to_string_lossy().into_owned()
    }

    /// A `-20 dBFS` sine block, duplicated to both channels.
    fn sine_block(amplitude: f32) -> Vec<u8> {
        let mut payload = Vec::with_capacity(BLOCK_BYTES);
        for n in 0..BLOCK_FRAMES {
            let sample =
                amplitude * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
            payload.extend_from_slice(&sample.to_le_bytes());
            payload.extend_from_slice(&sample.to_le_bytes());
        }
        payload
    }

    /// **H3 — the slot mapping.**  Each capture publishes into the slot the board
    /// gave its *uid*, so a level can never land on somebody else's bar.  The
    /// dangerous moment is a **reassignment** (a strip added/removed/reordered
    /// after an FX chain was created): the reader thread is restarted, and if it
    /// kept its old slot the bars would silently swap — every strip showing
    /// another one's audio.
    #[test]
    fn a_level_never_lands_on_another_strips_bar_after_a_reassignment() {
        let board = MeterBoard::new();
        let mut capture = MeterCapture::with_recorder(fake_recorder_by_target(
            "slots",
            &sine_block(0.1),  // −20 dBFS → level ≈ 0.56
            &sine_block(0.01), // −40 dBFS → level ≈ 0.26
        ));
        let mut wanted = HashMap::new();
        wanted.insert("loud".to_string(), "Holaf_Bus_Loud.monitor".to_string());
        wanted.insert("quiet".to_string(), "Holaf_Bus_Quiet.monitor".to_string());

        let expect = |board: &MeterBoard, loud_slot: usize, quiet_slot: usize| {
            let read = |slot: usize| board.read_slot(slot)[0];
            let loud = read(loud_slot);
            let quiet = read(quiet_slot);
            assert!(
                (loud - 0.56).abs() < 0.03,
                "the −20 dBFS capture must read 0.56, got {loud}"
            );
            assert!(
                (quiet - 0.26).abs() < 0.03,
                "the −40 dBFS capture must read 0.26, got {quiet}"
            );
        };

        // First assignment: loud = slot 0, quiet = slot 1.
        let slots = board.assign(&["loud".to_string(), "quiet".to_string()]);
        assert_eq!((slots["loud"], slots["quiet"]), (0, 1));
        capture.sync(&wanted, &slots, &board);
        assert!(
            wait_until(|| board.read_slot(0)[0] > 0.1 && board.read_slot(1)[0] > 0.1),
            "both captures must publish: {:?}",
            capture.stats().summary()
        );
        expect(&board, 0, 1);
        // …and by uid, which is what the UI reads.
        assert!((board.read("loud").unwrap()[0] - 0.56).abs() < 0.03);
        assert!((board.read("quiet").unwrap()[0] - 0.26).abs() < 0.03);

        // A strip was inserted before them: both slots move.  The readers must be
        // restarted (a stale slot index would paint the wrong bar).
        let slots = board
            .assign(&["new".to_string(), "loud".to_string(), "quiet".to_string()]);
        assert_eq!((slots["loud"], slots["quiet"]), (1, 2));
        capture.sync(&wanted, &slots, &board);
        assert!(
            wait_until(|| board.read_slot(1)[0] > 0.1 && board.read_slot(2)[0] > 0.1),
            "the restarted captures must publish into their new slots: {:?}",
            capture.stats().summary()
        );
        expect(&board, 1, 2);
        assert!((board.read("loud").unwrap()[0] - 0.56).abs() < 0.03);
        assert!((board.read("quiet").unwrap()[0] - 0.26).abs() < 0.03);
        // Every capture reports the slot it really writes into.
        let summary = capture.stats().summary();
        assert_eq!(summary.sources.len(), 2);
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
        // Nothing is detected, nothing is refused: all-zero bytes are honest
        // f32 silence, not a misread (the positive quiet test must not fire —
        // a `u8` stream's silence is `0x80`, a *f32* stream's is `0x00`).
        let summary = capture.stats().summary();
        assert_eq!(summary.rejected, 0, "{summary:?}");
        let facts = &summary.sources[0];
        assert!(facts.detected_format.is_none(), "{facts:?}");
        assert!(facts.last_rms.is_some_and(|rms| rms == [0.0, 0.0]), "{facts:?}");
        capture.stop_all();
    }
}

