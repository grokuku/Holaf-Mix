"""Tests for the sounddevice-backed `MeteringEngine` — no audio device needed.

Only the *bookkeeping* is exercised here (level staleness, retry backoff, device
probing): opening real capture streams requires a live PulseAudio/PipeWire
server, which the end-to-end check covers separately.

Run with:

    python -m unittest discover -s tests -v
    # or, from the repo root:
    python tests/test_metering.py
"""

import logging
import os
import sys
import time
import unittest
from unittest import mock

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if REPO_ROOT not in sys.path:
    sys.path.insert(0, REPO_ROOT)

from src.backend import metering  # noqa: E402
from src.backend.metering import MeteringEngine  # noqa: E402

logging.disable(logging.CRITICAL)


def engine_without_probe() -> MeteringEngine:
    """A `MeteringEngine` that never touches PortAudio (device probe stubbed)."""
    with mock.patch.object(MeteringEngine, "_find_pulse_device", return_value=0):
        return MeteringEngine()


class LevelStalenessTest(unittest.TestCase):
    def test_fresh_levels_are_returned_verbatim(self):
        engine = engine_without_probe()
        try:
            engine.levels["a"] = (0.5, 0.25)
            engine._level_ts["a"] = time.monotonic()
            self.assertEqual(engine.get_levels(), {"a": (0.5, 0.25)})
        finally:
            engine.stop_all()

    def test_stale_levels_are_reported_as_silence(self):
        # A source that suspends (idle strip, unplugged device) stops calling
        # the audio callback: the bar must fall back to zero instead of staying
        # frozen at its last value.
        engine = engine_without_probe()
        try:
            engine.levels["a"] = (0.9, 0.9)
            engine._level_ts["a"] = time.monotonic() - (metering.LEVEL_STALE_TIMEOUT_S + 0.5)
            self.assertEqual(engine.get_levels(), {"a": (0.0, 0.0)})
        finally:
            engine.stop_all()

    def test_streams_stop_removes_levels(self):
        engine = engine_without_probe()
        engine.levels["a"] = (0.5, 0.5)
        engine._level_ts["a"] = time.monotonic()
        engine.stop_monitoring("a")
        self.assertNotIn("a", engine.levels)
        self.assertNotIn("a", engine._level_ts)
        self.assertEqual(engine.get_levels(), {})


class RetryBackoffTest(unittest.TestCase):
    def test_failed_open_backs_off_exponentially_up_to_the_cap(self):
        engine = engine_without_probe()
        try:
            # Simulate a strip whose stream could not be opened: record the
            # delay the worker would schedule and check it doubles until the cap.
            delays = []
            for _ in range(8):
                failures = engine._retry_state.get("a", [0, 0.0])[0] + 1
                delay = min(
                    metering.RETRY_MAX_DELAY_S,
                    metering.RETRY_BASE_DELAY_S * (2 ** (failures - 1)),
                )
                engine._retry_state["a"] = [failures, time.monotonic() + delay]
                delays.append(delay)
            self.assertEqual(delays[0], metering.RETRY_BASE_DELAY_S)
            self.assertTrue(all(b >= a for a, b in zip(delays, delays[1:])))
            self.assertEqual(delays[-1], metering.RETRY_MAX_DELAY_S)
        finally:
            engine.stop_all()

    def test_retry_pending_skips_strips_still_in_backoff(self):
        engine = engine_without_probe()
        started = []
        try:
            engine.start_monitoring = lambda uid, src: started.append((uid, src))
            engine.pending_retries = {"a": "src-a", "b": "src-b"}
            # `a` is in its backoff window, `b` is due.
            engine._retry_state = {"a": [3, time.monotonic() + 60.0]}
            engine.retry_pending()
            self.assertEqual(started, [("b", "src-b")])
            # Not due entries stay queued exactly once (no thread pile-up).
            self.assertEqual(list(engine.pending_retries), ["a"])
        finally:
            engine.stop_all()

    def test_creation_lock_timeout_leaves_the_strip_pending(self):
        engine = engine_without_probe()
        try:
            engine.pending_retries.clear()
            engine.creation_lock.acquire()  # pretend another worker is stalled
            with mock.patch.object(metering, "CREATION_LOCK_TIMEOUT_S", 0.05):
                engine._worker_start_stream("a", "src-a")  # must not block
            self.assertEqual(engine.pending_retries, {"a": "src-a"})
        finally:
            engine.creation_lock.release()
            engine.stop_all()


class DeviceProbeTest(unittest.TestCase):
    def test_falls_back_to_the_default_input_device(self):
        class Pair:
            """Mimics sounddevice's unsized `_InputOutputPair`."""

            def __getitem__(self, index):
                return 3 if index == 0 else -1

        fake_default = mock.Mock(device=Pair())
        with mock.patch.object(metering.sd, "query_devices", return_value=[]), mock.patch.object(
            metering.sd, "default", fake_default
        ):
            self.assertEqual(MeteringEngine()._find_pulse_device(), 3)

    def test_returns_none_when_no_input_device_exists(self):
        class Pair:
            def __getitem__(self, index):
                return -1

        fake_default = mock.Mock(device=Pair())
        with mock.patch.object(metering.sd, "query_devices", return_value=[]), mock.patch.object(
            metering.sd, "default", fake_default
        ):
            # `len(sd.default.device)` would raise TypeError on the real
            # sounddevice pair: the probe must stay exception-free and report
            # "no device" so the caller retries later.
            self.assertIsNone(MeteringEngine()._find_pulse_device())

    def test_start_monitoring_reprobes_when_the_stack_was_not_ready(self):
        # The app may start before pipewire-pulse is up: the initial probe finds
        # no device, so every monitoring request must retry the probe instead of
        # silently giving up for the process lifetime.
        probe = mock.Mock(return_value=None)
        engine = MeteringEngine()
        engine.pulse_device_index = None
        with mock.patch.object(MeteringEngine, "_find_pulse_device", probe):
            engine.start_monitoring("a", "src-a")
            probe.assert_called_once()
        self.assertEqual(engine.pending_retries, {})  # not monitored, not queued


class MeterRateTest(unittest.TestCase):
    """The VU streams must ask for the *graph's* rate, not PortAudio's default.

    Leaving it to PortAudio made every meter a permanent resampler (44 100 Hz
    streams on a 48 000 Hz graph — 7 of them in a reported session), which is
    pure CPU/buffer churn on the same USB interface that was already xrunning.
    """

    def test_clock_rate_is_parsed_from_pw_metadata(self):
        output = "\n".join([
            'update: id:0 key:\'log.level\' value:\'2\' type:\'\'',
            'update: id:0 key:\'clock.rate\' value:\'48000\' type:\'\'',
            'update: id:0 key:\'clock.quantum\' value:\'1024\' type:\'\'',
        ])
        done = mock.Mock(stdout=output, returncode=0)
        with mock.patch.object(metering.subprocess, "run", return_value=done):
            self.assertEqual(metering.graph_sample_rate(), 48000)

    def test_a_missing_or_broken_probe_is_not_fatal(self):
        with mock.patch.object(
            metering.subprocess, "run", side_effect=OSError("pw-metadata not found")
        ):
            self.assertIsNone(metering.graph_sample_rate())
        done = mock.Mock(stdout="no metadata here", returncode=0)
        with mock.patch.object(metering.subprocess, "run", return_value=done):
            self.assertIsNone(metering.graph_sample_rate())

    def test_an_absurd_rate_is_rejected(self):
        out = "update: id:0 key:'clock.rate' value:'7' type:''"
        done = mock.Mock(stdout=out, returncode=0)
        with mock.patch.object(metering.subprocess, "run", return_value=done):
            self.assertIsNone(metering.graph_sample_rate())

    def test_the_engine_asks_for_the_graph_rate(self):
        engine = engine_without_probe()
        with mock.patch.object(metering, "graph_sample_rate", return_value=48000) as probe:
            self.assertEqual(engine._meter_rate(), 48000)
            # Resolved once per engine, not on every strip.
            self.assertEqual(engine._meter_rate(), 48000)
        self.assertEqual(probe.call_count, 1)

    def test_the_fallback_is_used_when_pipewire_does_not_answer(self):
        engine = engine_without_probe()
        with mock.patch.object(metering, "graph_sample_rate", return_value=None):
            self.assertEqual(engine._meter_rate(), metering.METER_RATE_FALLBACK)

    def test_opening_falls_back_to_the_portaudio_default(self):
        """A rejected rate must never cost us the stream."""
        engine = engine_without_probe()
        calls = []

        def fake_input_stream(**kwargs):
            calls.append(kwargs)
            if "samplerate" in kwargs:
                raise RuntimeError("Invalid sample rate")
            return mock.Mock(name="stream")

        with mock.patch.object(metering, "graph_sample_rate", return_value=48000), \
                mock.patch.object(metering.sd, "InputStream", side_effect=fake_input_stream):
            stream = engine._open_stream(lambda *a: None)
        self.assertIsNotNone(stream)
        self.assertEqual(calls[0].get("samplerate"), 48000)
        self.assertNotIn("samplerate", calls[1])
        self.assertEqual(calls[1]["blocksize"], 2048)


if __name__ == "__main__":
    unittest.main(verbosity=2)
