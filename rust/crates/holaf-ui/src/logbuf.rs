//! In-memory log ring buffer + the "Journal" tab of the diagnostics window (B7).
//!
//! Why it exists: a desktop-launched app has no terminal, so `env_logger`'s
//! output — the only place where a failure explains itself — is invisible.  The
//! Qt build solved this with `src/diagnostics/logbuffer.py` (a
//! `logging.Handler` keeping the last N lines) and a "Journal" tab; this is the
//! same thing in Rust: a bounded `VecDeque` of formatted lines, fed by
//! [`TeeLogger`] and shown by [`crate::dialogs`].
//!
//! Design notes
//! ------------
//! * the buffer is **bounded** ([`MAX_LINES`]): a mixer left running for a day
//!   must not grow without limit, and the oldest line is the least useful one;
//! * **order is preserved** (a `VecDeque`, pushed under one mutex, so two threads
//!   logging at once cannot reorder each other's lines *within* one push);
//! * the formatting is a **pure function** ([`format_line`]) so the exact text —
//!   which is what a user pastes into a bug report — is testable without a
//!   logger, a clock or a global;
//! * writing to the buffer can never fail the caller: a poisoned mutex (a panic
//!   in another thread while logging) yields an empty snapshot instead of a
//!   second panic *inside logging*, which would be the worst possible place.
//!
//! The log is not part of [`crate::diagnostics::Report`]: the report is a pure
//! snapshot of the engine, while the journal is a mutable process-global.  They
//! live in the same window because that is where a user looks, not because they
//! share a data model.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many lines the diagnostics window keeps (the Python build used 500 too).
pub const MAX_LINES: usize = 500;

/// A bounded FIFO of formatted log lines.
///
/// Pure (no globals, no clock) so the ring semantics — bound, order, eviction —
/// are unit-testable; the process-wide instance lives in [`global`].
#[derive(Debug)]
pub struct LogBuffer {
    lines: VecDeque<String>,
    capacity: usize,
}

impl LogBuffer {
    /// An empty buffer keeping at most `capacity` lines (`capacity` is clamped to
    /// at least 1: a zero-capacity buffer would silently swallow everything).
    pub const fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            capacity: if capacity == 0 { 1 } else { capacity },
        }
    }

    /// Append one line, dropping the oldest when the buffer is full.
    pub fn push(&mut self, line: String) {
        while self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// The lines, oldest first.
    pub fn lines(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

/// The process-wide buffer, filled by [`TeeLogger`] and read by the diagnostics
/// window.  `Mutex` (not a lock-free ring) on purpose: logging is not a hot path
/// (a handful of events per UI action) and a lock keeps the order obvious.
static GLOBAL: Mutex<LogBuffer> = Mutex::new(LogBuffer::new(MAX_LINES));

/// The process-wide buffer (exposed for the tests of the ring semantics).
pub fn global() -> &'static Mutex<LogBuffer> {
    &GLOBAL
}

/// Append one line to the process-wide buffer.
pub fn push(line: String) {
    // `lock()` fails only when another thread panicked *while holding it*; in
    // that case the log is the least of the problems and dropping the line is
    // still better than panicking inside a logger.
    if let Ok(mut buffer) = GLOBAL.lock() {
        buffer.push(line);
    }
}

/// The buffered lines, oldest first (empty when the mutex is poisoned).
pub fn snapshot() -> Vec<String> {
    match GLOBAL.lock() {
        Ok(buffer) => buffer.lines(),
        Err(poisoned) => poisoned.into_inner().lines(),
    }
}

/// How many lines are buffered.
pub fn len() -> usize {
    GLOBAL.lock().map_or(0, |buffer| buffer.len())
}

/// Drop every buffered line (used by the tests).
pub fn clear() {
    if let Ok(mut buffer) = GLOBAL.lock() {
        buffer.clear();
    }
}

/// Serialises the tests that read *and* write the process-wide buffer.
///
/// `GLOBAL` is one instance for the whole process, so two tests pushing/clearing
/// it in parallel would fight (a `clear()` in one test erases the line the other
/// is about to paint).  Every such test takes this guard first.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Format one log record the way the Journal tab shows it:
/// `HH:MM:SS LEVEL target: message`.
///
/// `unix` is the record's timestamp in seconds since the epoch, so the function
/// stays pure and the test can pin an exact string.  The time is **UTC** on
/// purpose: converting to local time needs a tz database, and a journal offset by
/// the wrong zone is worse than one clearly marked.
pub fn format_line(level: log::Level, target: &str, message: &str, unix: u64) -> String {
    let seconds_of_day = unix % 86_400;
    format!(
        "{:02}:{:02}:{:02} {:5} {target}: {message}",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
        level.as_str()
    )
}

/// Seconds since the Unix epoch (0 when the clock is before it — never panics).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Install the process logger: `env_logger` on stderr **plus** the ring buffer
/// behind the diagnostics window's "Journal" tab.
///
/// Teeing instead of two loggers: the `log` crate accepts a single global sink,
/// and `env_logger` is what gives us `RUST_LOG`/`HOLAF_LOG` filtering and a
/// terminal/journald copy.  The buffer therefore mirrors exactly what the filter
/// accepts — no more, no less.
///
/// `default_filter` is used when the environment says nothing (`"info"` for the
/// app, `"warn"` for the headless snapshot binary).
///
/// `set_boxed_logger` fails only when another logger was installed first (a test
/// harness).  Saying so on stderr beats a silent app: the Journal tab is the
/// only user-visible log, so a missing one must be explained.
pub fn install(default_filter: &str) {
    let inner =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
            .build();
    // The filter lives in the `log` crate: without this line every `log::debug!`
    // would still be formatted before `enabled()` is consulted.
    log::set_max_level(inner.filter());
    if log::set_boxed_logger(Box::new(TeeLogger::new(inner))).is_err() {
        eprintln!("holaf: a logger was already installed - the Journal tab stays empty");
    }
}

/// A `log::Log` that writes to an **inner** logger (normally `env_logger`) and
/// mirrors every accepted record into the ring buffer.
///
/// Teeing instead of replacing: `env_logger` keeps its filtering, its
/// `RUST_LOG`/`HOLAF_LOG` handling and its stderr format (so `journalctl` and a
/// terminal still work), and only the extra copy lands in memory.  The filter is
/// askable from the inner logger, which is what makes `enabled()` cheap.
pub struct TeeLogger<L: log::Log> {
    inner: L,
}

impl<L: log::Log> TeeLogger<L> {
    pub fn new(inner: L) -> Self {
        Self { inner }
    }
}

impl<L: log::Log> log::Log for TeeLogger<L> {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        // Only records the inner logger would emit: the buffer must show exactly
        // what the user's `RUST_LOG` asks for, not more.
        if !self.inner.enabled(record.metadata()) {
            return;
        }
        push(format_line(
            record.level(),
            record.target(),
            &record.args().to_string(),
            now_unix(),
        ));
        self.inner.log(record);
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_is_bounded_and_evicts_the_oldest_line() {
        let mut buffer = LogBuffer::new(3);
        for index in 0..5 {
            buffer.push(format!("line {index}"));
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.lines(), vec!["line 2", "line 3", "line 4"]);
    }

    #[test]
    fn the_order_is_the_push_order() {
        let mut buffer = LogBuffer::new(MAX_LINES);
        for index in 0..MAX_LINES {
            buffer.push(format!("line {index}"));
        }
        // A full buffer keeps everything, oldest first — exactly what a reader
        // scrolling to the bottom expects.
        let lines = buffer.lines();
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines.first().unwrap(), "line 0");
        assert_eq!(lines.last().unwrap(), &format!("line {}", MAX_LINES - 1));
    }

    #[test]
    fn a_zero_capacity_buffer_still_keeps_the_last_line() {
        let mut buffer = LogBuffer::new(0);
        buffer.push("only".to_string());
        assert_eq!(buffer.lines(), vec!["only"]);
        buffer.clear();
        assert!(buffer.is_empty());
    }

    #[test]
    fn the_line_format_is_stable_and_readable() {
        // 01:00:00 UTC = 3600 s after the epoch.
        assert_eq!(
            format_line(log::Level::Info, "holaf_ui::app", "loaded 7 strips", 3_600),
            "01:00:00 INFO  holaf_ui::app: loaded 7 strips"
        );
        assert_eq!(
            format_line(log::Level::Warn, "holaf_ui::midi", "no port", 86_400 + 59),
            "00:00:59 WARN  holaf_ui::midi: no port"
        );
        // The level is padded to 5 and left-aligned, so the *target* always
        // starts at the same column — which is the only reason to pad at all.
        assert_eq!(
            format_line(log::Level::Error, "t", "boom", 0),
            "00:00:00 ERROR t: boom"
        );
        let infos: Vec<usize> = [
            log::Level::Info,
            log::Level::Warn,
            log::Level::Error,
            log::Level::Debug,
        ]
        .iter()
        .map(|level| format_line(*level, "t", "m", 0).find("t: m").unwrap())
        .collect();
        assert!(infos.iter().all(|column| *column == infos[0]), "{infos:?}");
    }

    /// The global buffer is what the diagnostics window reads: pushing is visible
    /// in a snapshot, and the snapshot cannot alias the buffer (a caller that
    /// mutates the returned `Vec` must not corrupt the log).
    #[test]
    fn the_global_buffer_is_readable_and_copy_on_read() {
        let _guard = test_lock();
        clear();
        push("first".to_string());
        push("second".to_string());
        let lines = snapshot();
        assert!(lines.len() >= 2, "{lines:?}");
        assert_eq!(&lines[lines.len() - 2..], ["first", "second"]);
        assert_eq!(len(), lines.len());
        // Mutating the snapshot must not affect the buffer.
        let mut copy = snapshot();
        copy.clear();
        assert_eq!(len(), lines.len());
        clear();
        assert_eq!(len(), 0);
    }

    /// A record the inner logger filters out must not reach the buffer —
    /// otherwise `RUST_LOG=warn` would still fill the Journal with `DEBUG` noise.
    #[test]
    fn the_tee_logger_only_mirrors_what_the_inner_logger_accepts() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Minimal inner logger: `enabled` only at WARN and above, and it counts
        /// what it received.
        struct WarnOnly(AtomicUsize);

        impl log::Log for WarnOnly {
            fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
                metadata.level() <= log::Level::Warn
            }
            fn log(&self, _record: &log::Record<'_>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn flush(&self) {}
        }

        let _guard = test_lock();
        let tee = TeeLogger::new(WarnOnly(AtomicUsize::new(0)));

        clear();
        let arguments = format_args!("message");
        let info = log::Record::builder()
            .args(arguments)
            .level(log::Level::Info)
            .target("holaf_ui::tests")
            .build();
        log::Log::log(&tee, &info);
        assert_eq!(
            len(),
            0,
            "an Info record is filtered out by the inner logger"
        );

        let arguments = format_args!("message");
        let warn = log::Record::builder()
            .args(arguments)
            .level(log::Level::Warn)
            .target("holaf_ui::tests")
            .build();
        log::Log::log(&tee, &warn);
        assert_eq!(len(), 1, "a Warn record is mirrored");
        let lines = snapshot();
        assert!(
            lines[0].contains("WARN  holaf_ui::tests: message"),
            "{lines:?}"
        );

        let metadata = log::Metadata::builder()
            .level(log::Level::Error)
            .target("holaf_ui::tests")
            .build();
        assert!(
            log::Log::enabled(&tee, &metadata),
            "the inner filter decides"
        );
        clear();
    }
}
