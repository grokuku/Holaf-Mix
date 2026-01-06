import sounddevice as sd
import numpy as np
import os
import logging
import threading
import time
from typing import Dict, Tuple, Optional

logger = logging.getLogger("MeteringEngine")

class MeteringEngine:
    def __init__(self):
        self.active_streams: Dict[str, sd.InputStream] = {}
        self.levels: Dict[str, Tuple[float, float]] = {}
        self.pending_retries: Dict[str, str] = {} # UID -> Source Name
        
        # LOCK 1: Protects shared data (levels, registries). Used frequently by UI.
        self.data_lock = threading.Lock()
        
        # LOCK 2: Serializes stream creation to prevent os.environ collisions.
        # Used only by worker threads.
        self.creation_lock = threading.Lock()
        
        self.pulse_device_index = self._find_pulse_device()

    def _find_pulse_device(self) -> Optional[int]:
        try:
            devices = sd.query_devices()
            for i, dev in enumerate(devices):
                if dev['name'] == 'pulse' and dev['max_input_channels'] > 0:
                    return i
            for i, dev in enumerate(devices):
                if dev['name'] == 'default' and dev['max_input_channels'] > 0:
                    return i
            # Fallback
            if len(sd.default.device) > 0:
                 return sd.default.device[0]
            return None
        except Exception as e:
            logger.error(f"Error finding pulse device: {e}")
            return None

    def start_monitoring(self, strip_uid: str, source_name: str):
        """
        Non-blocking call. Spawns a thread to attempt stream connection.
        """
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

        stream = None
        success = False

        # --- CRITICAL SECTION: STREAM CREATION ---
        # We hold the creation_lock to ensure 'PULSE_SOURCE' belongs to US 
        # for the entire duration of the stream initialization.
        with self.creation_lock:
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
                
            except Exception:
                success = False
            finally:
                # 4. Cleanup Env immediately
                if "PULSE_SOURCE" in os.environ:
                    del os.environ["PULSE_SOURCE"]

        # --- UPDATE REGISTRY ---
        with self.data_lock:
            if success and stream:
                self.active_streams[strip_uid] = stream
                if strip_uid in self.pending_retries:
                    del self.pending_retries[strip_uid]
            else:
                # Schedule retry
                self.pending_retries[strip_uid] = source_name

    def retry_pending(self):
        """
        Called periodically. Re-triggers threaded attempts.
        """
        with self.data_lock:
            items_to_retry = list(self.pending_retries.items())
        
        if not items_to_retry: return

        # Retry logic: gently retry one item
        uid, source = items_to_retry[0]
        self.start_monitoring(uid, source)

    def stop_monitoring(self, strip_uid: str):
        stream = None
        with self.data_lock:
            if strip_uid in self.active_streams:
                stream = self.active_streams[strip_uid]
                del self.active_streams[strip_uid]
            
            if strip_uid in self.pending_retries:
                del self.pending_retries[strip_uid]
            
            if strip_uid in self.levels:
                del self.levels[strip_uid]
        
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

    def get_levels(self) -> Dict[str, Tuple[float, float]]:
        with self.data_lock:
            return self.levels.copy()