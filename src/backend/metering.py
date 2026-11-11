import sounddevice as sd
import numpy as np
import os
import logging
import threading
import time
from typing import Dict, Tuple, Optional

logger = logging.getLogger("MeteringEngine")

# How long a worker waits for the (process-wide, env-var-protected) stream
# creation section.  Opening a capture stream on an *idle* source can stall for
# a long time inside PulseAudio/PortAudio (measured: ~30 s on a suspended
# null-sink monitor), so the section is never held open-ended.
CREATION_LOCK_TIMEOUT_S = 5.0

# Per-strip backoff after a failed open: doubles from the base up to the cap,
# so a source that cannot be opened yet (strip idle, audio stack still coming
# up) is retried a couple of times per minute instead of on every sweep.
RETRY_BASE_DELAY_S = 2.0
RETRY_MAX_DELAY_S = 30.0

# A capture stream that has not delivered a single frame since it was opened is
# considered wedged (PipeWire/pipewire-pulse can leave a stream created on an
# *idle* source permanently suspended: it exists, is linked, but never runs).
# After this delay it is closed and reopened, so the VU meter starts working on
# its own as soon as the strip produces sound.
STREAM_NO_DATA_TIMEOUT_S = 3.0

# A level older than this is reported as silence.  The audio callback runs
# every `blocksize`/rate (~43 ms at 2048/48 k), but PortAudio stops calling it
# as soon as the server suspends the source (an idle null-sink, a removed
# device): without this guard a VU bar would stay frozen at its last value
# instead of falling back to zero.
LEVEL_STALE_TIMEOUT_S = 0.25

class MeteringEngine:
    def __init__(self):
        self.active_streams: Dict[str, sd.InputStream] = {}
        self.levels: Dict[str, Tuple[float, float]] = {}
        self.pending_retries: Dict[str, str] = {} # UID -> Source Name
        # Failure bookkeeping for the retry backoff (uid -> [failures, next_ts]).
        self._retry_state: Dict[str, list] = {}
        # Monotonic timestamp of the last audio callback per strip (staleness).
        self._level_ts: Dict[str, float] = {}
        # Frames received per strip, open timestamp and source name (wedged
        # stream watchdog: a stream that never fires must be reopened).
        self._frame_counts: Dict[str, int] = {}
        self._stream_opened_at: Dict[str, float] = {}
        self._stream_sources: Dict[str, str] = {}
        
        # LOCK 1: Protects shared data (levels, registries). Used frequently by UI.
        self.data_lock = threading.Lock()
        
        # LOCK 2: Serializes stream creation to prevent os.environ collisions.
        # Used only by worker threads.
        self.creation_lock = threading.Lock()
        
        self.pulse_device_index = self._find_pulse_device()

    def _find_pulse_device(self) -> Optional[int]:
        """Index of the PortAudio input device that speaks the Pulse protocol.

        Returns ``None`` when the audio stack is not reachable yet (e.g. the
        application started before ``pipewire-pulse``): callers retry later
        instead of giving up for the process lifetime.
        """
        try:
            devices = sd.query_devices()
            for i, dev in enumerate(devices):
                if dev['name'] == 'pulse' and dev['max_input_channels'] > 0:
                    return i
            for i, dev in enumerate(devices):
                if dev['name'] == 'default' and dev['max_input_channels'] > 0:
                    return i
            # Fallback: PortAudio's default input device.
            # NOTE: `sd.default.device` is an unsized `_InputOutputPair` (it
            # supports indexing but NOT `len()`), and its entries are -1 when
            # no host API has a default device — hence the explicit checks.
            try:
                default_in = sd.default.device[0]
            except (TypeError, IndexError):
                return None
            if isinstance(default_in, int) and default_in >= 0:
                return default_in
            return None
        except Exception as e:
            logger.error(f"Error finding pulse device: {e}")
            return None

    def start_monitoring(self, strip_uid: str, source_name: str):
        """
        Non-blocking call. Spawns a thread to attempt stream connection.
        """
        if self.pulse_device_index is None:
            # The audio stack may have come up after we were constructed (app
            # started before pipewire-pulse / PulseAudio).  Re-probe here:
            # `retry_pending()` calls this method again on every sweep, so
            # metering recovers on its own instead of staying dead until the
            # next app start.
            self.pulse_device_index = self._find_pulse_device()
            if self.pulse_device_index is None:
                return

        # Stop existing if any (synchronous cleanup is fast)
        self.stop_monitoring(strip_uid)

        # Launch the connection attempt in a separate thread to avoid freezing UI/Main logic
        # if ALSA hangs or timeouts.
        t = threading.Thread(target=self._worker_start_stream, args=(strip_uid, source_name))
        t.daemon = True
        t.start()

    def _worker_start_stream(self, strip_uid: str, source_name: str):
        """
        The heavy lifting happens here.
        CRITICAL: We must ensure os.environ is not overwritten by another thread
        during the stream initialization.
        """
        def callback(indata, frames, time_info, status):
            if status: pass 
            
            if indata.shape[1] >= 2:
                rms = np.sqrt(np.mean(indata**2, axis=0))
                l_vol = min(1.0, rms[0] * 5)
                r_vol = min(1.0, rms[1] * 5)
            else:
                val = np.sqrt(np.mean(indata**2))
                l_vol = r_vol = min(1.0, val * 5)

            with self.data_lock:
                self.levels[strip_uid] = (l_vol, r_vol)
                self._level_ts[strip_uid] = time.monotonic()
                self._frame_counts[strip_uid] = self._frame_counts.get(strip_uid, 0) + frames

        stream = None
        success = False

        # --- CRITICAL SECTION: STREAM CREATION ---
        # We hold the creation_lock to ensure 'PULSE_SOURCE' belongs to US
        # for the entire duration of the stream initialization.
        # NOTE: os.environ is process-wide mutable state. The creation_lock
        # serializes *our* workers, but any *other* thread in the process
        # that reads os.environ between our set/del may see a transient
        # wrong PULSE_SOURCE. The lock minimizes the window but cannot
        # eliminate it. A proper fix would use pulsectl or the PortAudio
        # host API directly to bypass env-var-based source selection.
        #
        # The acquisition is bounded: opening a capture stream on an *idle*
        # source can stall for tens of seconds inside PulseAudio/PortAudio
        # (measured ~30 s on a suspended null-sink monitor), and a stalled
        # worker must not queue every other strip behind it.
        if not self.creation_lock.acquire(timeout=CREATION_LOCK_TIMEOUT_S):
            logger.debug(
                "stream creation busy for '%s' (%s) — retrying later",
                strip_uid,
                source_name,
            )
            with self.data_lock:
                self.pending_retries[strip_uid] = source_name
            return
        try:
            # 1. Set Target
            os.environ["PULSE_SOURCE"] = source_name

            # 2. Initialize Stream (This reads the Env)
            # This might block for a short while, but since we are in a background thread
            # and NOT holding data_lock, the UI remains responsive.
            stream = sd.InputStream(
                device=self.pulse_device_index,
                channels=2,
                blocksize=2048,
                latency='high', 
                callback=callback
            )

            # 3. Start Stream
            stream.start()
            success = True

        except Exception as exc:
            logger.debug(
                "stream open failed for '%s' (%s): %s", source_name, strip_uid, exc
            )
            success = False
        finally:
            # 4. Cleanup Env immediately
            if "PULSE_SOURCE" in os.environ:
                del os.environ["PULSE_SOURCE"]
            self.creation_lock.release()

        # --- UPDATE REGISTRY ---
        with self.data_lock:
            if success and stream:
                self.active_streams[strip_uid] = stream
                self.pending_retries.pop(strip_uid, None)
                self._retry_state.pop(strip_uid, None)
                self._frame_counts[strip_uid] = 0
                self._stream_opened_at[strip_uid] = time.monotonic()
                self._stream_sources[strip_uid] = source_name
            else:
                # Schedule retry, with an exponential backoff (see
                # retry_pending) because a failed open can have burned tens of
                # seconds inside the audio stack.
                failures = self._retry_state.get(strip_uid, [0, 0.0])[0] + 1
                delay = min(
                    RETRY_MAX_DELAY_S, RETRY_BASE_DELAY_S * (2 ** (failures - 1))
                )
                self._retry_state[strip_uid] = [failures, time.monotonic() + delay]
                self.pending_retries[strip_uid] = source_name

    def retry_pending(self):
        """
        Called periodically. Re-triggers threaded attempts.

        Strips whose stream could not be opened back off exponentially
        (``RETRY_BASE_DELAY_S`` … ``RETRY_MAX_DELAY_S``): a failed open can
        stall for tens of seconds, so retrying on every sweep would only pile
        worker threads up behind it.
        """
        now = time.monotonic()
        with self.data_lock:
            items_to_retry = [
                (uid, source)
                for uid, source in self.pending_retries.items()
                if self._retry_state.get(uid, [0, 0.0])[1] <= now
            ]
            # Clear them; _worker_start_stream re-adds on failure.
            # This way, ALL due pending items retry in the same cycle instead of
            # waiting 2.5s per item (50 cycles * 50ms).
            for uid, _source in items_to_retry:
                del self.pending_retries[uid]

        # Watchdog: a stream opened while its source was idle can stay suspended
        # forever (no callback ever fires).  Reopen it so metering recovers on
        # its own once the strip has sound.
        wedged: List[Tuple[str, str]] = []
        now_ts = time.monotonic()
        with self.data_lock:
            for uid in list(self.active_streams):
                if self._frame_counts.get(uid, 0) > 0:
                    continue
                opened = self._stream_opened_at.get(uid, now_ts)
                if now_ts - opened > STREAM_NO_DATA_TIMEOUT_S:
                    wedged.append((uid, self._stream_sources.get(uid, "")))
        for uid, source in wedged:
            logger.debug("stream for '%s' delivered no audio — reopening", uid)
            # `stop_monitoring` drops the bookkeeping for this strip, then the
            # target is re-queued: `start_monitoring` reopens it on the next
            # sweep (no backoff, it is not a failure).
            self.stop_monitoring(uid)
            if source:
                with self.data_lock:
                    self.pending_retries[uid] = source
                    self._retry_state.pop(uid, None)

        if not items_to_retry: return

        for uid, source in items_to_retry:
            self.start_monitoring(uid, source)

    def stop_monitoring(self, strip_uid: str):
        stream = None
        with self.data_lock:
            if strip_uid in self.active_streams:
                stream = self.active_streams[strip_uid]
                del self.active_streams[strip_uid]
            
            if strip_uid in self.pending_retries:
                del self.pending_retries[strip_uid]
            self._retry_state.pop(strip_uid, None)
            
            if strip_uid in self.levels:
                del self.levels[strip_uid]
            self._level_ts.pop(strip_uid, None)
            self._frame_counts.pop(strip_uid, None)
            self._stream_opened_at.pop(strip_uid, None)
            self._stream_sources.pop(strip_uid, None)
        
        # Close outside lock to avoid deadlocks
        if stream:
            try:
                stream.stop()
                stream.close()
            except Exception:
                pass

    def stop_all(self):
        with self.data_lock:
            keys = list(self.active_streams.keys())
        for uid in keys:
            self.stop_monitoring(uid)
        
        with self.data_lock:
            self.pending_retries.clear()
            self._retry_state.clear()

    def get_levels(self) -> Dict[str, Tuple[float, float]]:
        """Snapshot of the current levels, stale streams reported as silence.

        Returns ``{uid: (left, right)}`` with values in 0.0-1.0 (RMS scaled by 5,
        clipped) — the exact shape `main_window._update_meters` consumes.
        """
        now = time.monotonic()
        with self.data_lock:
            return {
                uid: (
                    level
                    if now - self._level_ts.get(uid, 0.0) <= LEVEL_STALE_TIMEOUT_S
                    else (0.0, 0.0)
                )
                for uid, level in self.levels.items()
            }