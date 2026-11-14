//! Coalescing primitives for the engine commands the UI emits.
//!
//! A fader drag emits one `Volume` action **per frame** (60/s) but the engine
//! must not be called that often: the Python UI throttled the same thing with a
//! 50 ms `QTimer` (`_check_and_send_volume`, "20 Hz limit"), and the task's
//! contract is 25 Hz.  [`Throttled`] is the rule in one place, as data:
//!
//! * the **first** value of a burst goes out immediately (no lag at the start of
//!   a gesture);
//! * everything inside the following `interval` is stashed, keeping only the
//!   **last** value;
//! * when the interval elapses, the last stashed value is sent once.
//!
//! The clock is injected (`Instant` is a parameter everywhere), so the tests
//! assert the exact send pattern without sleeping — and a loaded machine cannot
//! change the behaviour.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Per-key trailing throttle: at most one send per `interval`, always the last
/// value.
#[derive(Debug, Clone)]
pub struct Throttled<T> {
    interval: Duration,
    last_sent: HashMap<String, Instant>,
    pending: HashMap<String, T>,
}

impl<T> Throttled<T> {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_sent: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }
}

impl<T: Clone> Throttled<T> {
    /// Record a value for `key`.
    ///
    /// Returns `Some(value)` when it must be sent right now (the key was idle
    /// long enough), `None` when it was coalesced — [`Self::take_due`] then
    /// yields it later.
    pub fn record(&mut self, now: Instant, key: &str, value: T) -> Option<T> {
        match self.last_sent.get(key) {
            Some(last) if now.saturating_duration_since(*last) < self.interval => {
                // Inside the window: keep only the latest value.
                self.pending.insert(key.to_string(), value);
                None
            }
            _ => {
                self.last_sent.insert(key.to_string(), now);
                self.pending.remove(key);
                Some(value)
            }
        }
    }

    /// The values whose window elapsed (one entry per key at most).
    pub fn take_due(&mut self, now: Instant) -> Vec<(String, T)> {
        let due: Vec<String> = self
            .pending
            .keys()
            .filter(|key| {
                self.last_sent
                    .get(*key)
                    .is_none_or(|last| now.saturating_duration_since(*last) >= self.interval)
            })
            .cloned()
            .collect();
        let mut out = Vec::with_capacity(due.len());
        for key in due {
            if let Some(value) = self.pending.remove(&key) {
                self.last_sent.insert(key.clone(), now);
                out.push((key, value));
            }
        }
        // Deterministic order: HashMap iteration is not, and a test (or a log)
        // must not depend on the allocator.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// The stashed value of a key, without sending it (tests/diagnostics).
    pub fn peek(&self, key: &str) -> Option<&T> {
        self.pending.get(key)
    }

    /// Earliest instant at which [`Self::take_due`] can return something (for
    /// `request_repaint_after`).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .keys()
            .filter_map(|key| self.last_sent.get(key).map(|last| *last + self.interval))
            .min()
    }

    /// Drop everything (engine gone, strip deleted): nothing stale is sent.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.last_sent.clear();
    }

    /// Forget one key (strip deleted).
    pub fn forget(&mut self, key: &str) {
        self.pending.remove(key);
        self.last_sent.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_value_goes_out_immediately_then_only_the_last() {
        let now = Instant::now();
        let mut t = Throttled::new(Duration::from_millis(40));
        assert_eq!(t.record(now, "in-mic", 0.1_f32), Some(0.1));
        // Same frame + the next ones: coalesced, last value wins.
        assert_eq!(t.record(now, "in-mic", 0.2), None);
        assert_eq!(
            t.record(now + Duration::from_millis(10), "in-mic", 0.3),
            None
        );
        assert_eq!(
            t.record(now + Duration::from_millis(30), "in-mic", 0.4),
            None
        );
        assert_eq!(t.peek("in-mic"), Some(&0.4));
        // Not due yet.
        assert!(t.take_due(now + Duration::from_millis(39)).is_empty());
        // Due: exactly one send, with the last value.
        let due = t.take_due(now + Duration::from_millis(40));
        assert_eq!(due, vec![("in-mic".to_string(), 0.4)]);
        assert!(!t.is_pending());
        assert!(t.take_due(now + Duration::from_millis(100)).is_empty());
    }

    #[test]
    fn keys_are_independent_and_ordered() {
        let now = Instant::now();
        let mut t = Throttled::new(Duration::from_millis(40));
        assert_eq!(t.record(now, "b", 1), Some(1));
        assert_eq!(t.record(now, "a", 2), Some(2));
        assert_eq!(t.record(now, "a", 3), None);
        assert_eq!(t.record(now, "b", 4), None);
        let due = t.take_due(now + Duration::from_millis(50));
        assert_eq!(
            due,
            vec![("a".to_string(), 3), ("b".to_string(), 4)],
            "sorted by key, so a test/log is deterministic"
        );
    }

    #[test]
    fn a_new_gesture_after_the_window_is_sent_at_once() {
        let now = Instant::now();
        let mut t = Throttled::new(Duration::from_millis(40));
        t.record(now, "k", 1);
        assert_eq!(t.record(now + Duration::from_millis(41), "k", 2), Some(2));
        assert!(!t.is_pending());
    }

    #[test]
    fn deadline_and_forget() {
        let now = Instant::now();
        let mut t = Throttled::new(Duration::from_millis(40));
        assert_eq!(t.next_deadline(), None);
        t.record(now, "k", 1);
        t.record(now, "k", 2);
        assert_eq!(t.next_deadline(), Some(now + Duration::from_millis(40)));
        assert_eq!(t.pending_count(), 1);
        t.forget("k");
        assert_eq!(t.next_deadline(), None);
        assert!(!t.is_pending());
        // …and the next record is immediate again.
        assert_eq!(t.record(now, "k", 3), Some(3));
        t.clear();
        assert!(!t.is_pending());
    }
}
