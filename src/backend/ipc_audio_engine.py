"""IpcAudioEngine — Python client for the Rust `holafd` NDJSON daemon (J4, robustesse J5).

This module exposes the **same public surface** as
:mod:`src.backend.audio_engine.AudioEngine` (the class the UI talks to), but
instead of driving PipeWire directly it sends NDJSON orders over a Unix socket
to the Rust daemon (`holafd`), which owns the audio graph.

Design contract
---------------
* Pure transport + thin mapping.  The daemon is the source of truth: it runs
  every snapshot through the `holaf-core` migrations and rebuilds the graph
  from the applied state.
* One fresh Unix-socket connection per request.  This mirrors `holafctl` and
  keeps concurrent UI worker threads safe (the daemon serialises requests on a
  single mutex, like the Python engine's ``RLock``).
* Methods never block the UI thread: `main_window.py` already dispatches them
  through `_run_in_background`/`BackendWorker`.
* Errors surface as :class:`IpcEngineError` with a stable machine-readable
  ``code`` and the failing ``request_id`` when known.  Lightweight mutators
  (volume, mute, …) log and swallow errors to stay forgiving like
  `AudioEngine`; `start_engine` re-raises so a missing daemon produces a
  clear startup failure in `main.py`.
* Bounded retry: lightweight mutators retry once on a transient transport
  failure (socket timeout / dropped connection) — the retry opens a fresh
  connection, which is cheap because every request already uses its own
  socket.  Heavy, state-replacing orders (`engine.apply_snapshot`) never
  retry.  Every retry is logged.
* Explicit timeouts: every `recv` runs on a socket configured with
  `settimeout` (``REQUEST_TIMEOUT_S`` = 5 s by default), so a hung daemon can
  never block the UI thread pool forever; a timeout surfaces as
  :class:`IpcEngineError` with code ``timeout``.

Wire format (holaf-ipc, spec §4)::

    request:  {"id":1,"method":"strip.set_volume","params":{"uid":…,"value":…}}
    response: {"id":1,"ok":true,"result":{…}}
    response: {"id":1,"ok":false,"error":{"code":"unknown_strip","message":…}}
    event:    {"event":"state_changed","params":{…}}

Metering (VU bars) stays Python-local; `get_meter_levels` is a stub returning
``{}`` until the optional native metering lands (J5+, spec §6).
"""

import errno
import json
import logging
import os
import socket
import threading
from typing import Any, Dict, List, Optional

from src.models.strip_model import Strip

logger = logging.getLogger("IpcAudioEngine")

# Default round-trip budget for a single NDJSON request/response.
#
# This is the socket timeout applied to *every* connect/send/recv below
# (see IpcAudioEngine._connect and _request_once) — no recv ever blocks
# longer than this.  The UI always talks to the engine from its worker-thread
# pool (`_run_in_background`/BackendWorker), so a 5 s ceiling keeps a hung
# daemon from freezing the GUI while still leaving slow PipeWire rebuilds
# (snapshot apply, app.move) enough room.
REQUEST_TIMEOUT_S = 5.0

# Extra attempts granted to lightweight mutators after the first failure
# (``retries=1`` => at most 2 tries in total).  Heavy orders such as
# engine.apply_snapshot pass ``retries=0`` and never auto-retry.
DEFAULT_RETRIES = 1

# Transport failures worth one bounded retry.  They mean "the daemon may
# simply have been busy / the socket dropped" and a fresh connection has a
# real chance of succeeding.  ``daemon_not_running`` and ``parse_error`` are
# deliberately excluded (a retry cannot fix them) and so are daemon-reported
# application errors (``ok:false``) which never reach this set.
RETRYABLE_CODES = frozenset(
    {"timeout", "connection_error", "connection_closed"}
)

# --- IPC method names (mirror holaf-ipc constants) -------------------------
METHOD_APPLY_SNAPSHOT = "engine.apply_snapshot"
METHOD_STOP = "engine.stop"
METHOD_STATUS = "engine.status"
METHOD_STRIP_SET_VOLUME = "strip.set_volume"
METHOD_STRIP_SET_MUTE = "strip.set_mute"
METHOD_STRIP_SET_MONO = "strip.set_mono"
METHOD_FX_SET_ACTIVE = "fx.set_active"
METHOD_FX_SET_PARAMS = "fx.set_params"
METHOD_ROUTING_SET = "routing.set"
METHOD_ROUTING_LIST = "routing.list"
METHOD_SYSTEM_SET_DEFAULT_SINK = "system.set_default_sink"
METHOD_DEVICE_LIST = "device.list"
METHOD_APP_LIST = "app.list"
METHOD_APP_MOVE = "app.move"

# Canonical effect ordering (mirrors FX_PLUGIN_MAP in audio_engine.py and
# EffectKey::all() in holaf-core).  The Strip model keeps all five effects
# uniformly regardless of strip kind.
EFFECT_ORDER = ("gate", "noise_cancel", "eq", "tube", "compressor")


class _SocketLineReader:
    """A small newline-delimited reader over a raw blocking socket.

    NDJSON uses one JSON document per line.  This reads with ``recv`` into an
    internal buffer so that bytes past the first newline (a coalesced event +
    response) stay buffered for the next :meth:`readline` call.
    Returns ``None`` on EOF instead of raising, so the caller can decide how
    to phrase a truncated-connection error.
    """

    def __init__(self, sock: socket.socket):
        self._sock = sock
        self._buffer = b""

    def readline(self) -> Optional[str]:
        while b"\n" not in self._buffer:
            chunk = self._sock.recv(4096)
            if not chunk:
                return None
            self._buffer += chunk
        line, self._buffer = self._buffer.split(b"\n", 1)
        return line.decode("utf-8", errors="replace")


class IpcEngineError(Exception):
    """A failed IPC exchange with the daemon.

    Attributes:
        code: Stable machine-readable error code echoed from the daemon
            (``unknown_strip``, ``not_implemented``, …) or a local transport
            code (``daemon_not_running``, ``timeout``, ``connection_error``,
            ``connection_closed``, ``parse_error``).
        message: Human-readable description (already included in str()).
        request_id: The IPC request id this error belongs to, or ``None``
            when the failure happened before an id could be attributed.
    """

    def __init__(
        self,
        code: str,
        message: str,
        request_id: Optional[int] = None,
    ):
        super().__init__(f"[{code}] {message}")
        self.code = code
        self.message = message
        self.request_id = request_id


def default_socket_path() -> str:
    """Resolve the daemon socket the same way `holafd`/`holafctl` do:
    ``$HOLAF_SOCKET``, else ``/tmp/holaf-<USER>.sock``.
    """
    env = os.environ.get("HOLAF_SOCKET")
    if env:
        return env
    user = os.environ.get("USER") or "holaf"
    return os.path.join("/tmp", f"holaf-{user}.sock")


def _strip_to_rust_dict(strip: Strip) -> Dict[str, Any]:
    """Project a Python :class:`Strip` onto the Rust daemon's JSON schema.

    The daemon's ``Strip`` (holaf-core) models: uid, label, kind, volume,
    mute, is_mono, routes, is_default, device_name and effects.  UI-only
    fields (mode, assigned_apps, MIDI mappings) are deliberately dropped —
    they are not modelled by the daemon yet and would be ignored by serde
    anyway.  Effect keys are already snake_case in the Python model, so the
    conversion is mostly a whitelist projection + effect normalisation
    (legacy boolean entries are upgraded to ``{active, params}``).
    """
    raw_effects = getattr(strip, "effects", None) or {}
    effects: Dict[str, Dict[str, Any]] = {}
    for key, data in raw_effects.items():
        if isinstance(data, dict):
            effects[key] = {
                "active": bool(data.get("active", False)),
                "params": dict(data.get("params", {}) or {}),
            }
        else:
            # Legacy boolean effect entry (old config format).
            effects[key] = {"active": bool(data), "params": {}}

    return {
        "uid": strip.uid,
        "label": strip.label,
        "kind": strip.kind,  # StripType.INPUT / OUTPUT => "input"/"output"
        "volume": float(strip.volume),
        "mute": bool(strip.mute),
        "is_mono": bool(strip.is_mono),
        "routes": list(strip.routes),
        "is_default": bool(strip.is_default),
        "device_name": strip.device_name,
        "effects": effects,
    }


class IpcAudioEngine:
    """IPC replacement for `AudioEngine` backed by the Rust `holafd` daemon.

    Public method names/signatures mirror :class:`src.backend.audio_engine
    .AudioEngine` so `main_window.py` needs no changes.  Additional read
    helpers (`get_audio_nodes`, `get_sink_inputs`, `move_sink_input`) expose
    the daemon's device/app IPC methods; device/app discovery in the UI is
    still served by `pipewire_utils` until the mapping milestone.
    """

    def __init__(self, socket_path: Optional[str] = None):
        # Guards request-id allocation only; each request uses a fresh
        # connection so no socket-level lock is required.
        self.socket_path = socket_path or default_socket_path()
        self.timeout: float = REQUEST_TIMEOUT_S
        self._seq_lock = threading.Lock()
        self._seq = 0
        # Shallow copy of the last applied strips.  Kept so `reload_strip`
        # can fall back to re-applying a full snapshot (the only "rebuild"
        # primitive in the J4 protocol) when a hot FX update cannot reach the
        # daemon.  Strip objects are shared with the caller, so their live
        # state is always current.
        self._strips: List[Strip] = []

    # ------------------------------------------------------------------ IPC
    def _connect(self, timeout: Optional[float] = None) -> socket.socket:
        """Open a fresh Unix-socket connection to the daemon.

        Raises :class:`IpcEngineError` with a clear ``daemon_not_running``
        code when no daemon is listening at ``self.socket_path`` (socket file
        missing or stale).
        """
        path = self.socket_path
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.settimeout(self.timeout if timeout is None else timeout)
            sock.connect(path)
            return sock
        except OSError as exc:
            sock.close()
            if exc.errno in (errno.ENOENT, errno.ECONNREFUSED):
                raise IpcEngineError(
                    "daemon_not_running",
                    f"daemon not running — launch holafd (socket '{path}')",
                ) from exc
            raise IpcEngineError(
                "connection_error",
                f"cannot connect to daemon at '{path}': {exc}",
            ) from exc

    def _next_request_id(self) -> int:
        """Allocate the next monotonically increasing request id (thread-safe)."""
        with self._seq_lock:
            self._seq += 1
            return self._seq

    def _request(
        self,
        method: str,
        params: Optional[Dict[str, Any]] = None,
        timeout: Optional[float] = None,
        retries: int = DEFAULT_RETRIES,
    ) -> Dict[str, Any]:
        """Send one NDJSON request and return the daemon's ``result`` dict.

        Events pushed by the daemon between the request and its response are
        ignored (they carry no request id).  On ``ok: false`` a
        :class:`IpcEngineError` is raised with the daemon's error code,
        message and request id.

        ``retries`` bounds the number of *extra* attempts after the first
        failure (``retries=1`` => at most two tries).  Only transient
        transport failures (:data:`RETRYABLE_CODES`) are retried, and each
        retry opens a fresh connection (every request already uses its own
        socket, so this is just "try again").  Heavy, state-replacing orders
        such as ``engine.apply_snapshot`` pass ``retries=0``: re-running them
        automatically could double-apply a snapshot, so callers own that
        decision.  Every retry is logged.

        Raises:
            IpcEngineError: on transport failure, malformed daemon output or
                a daemon-reported ``ok:false`` error.  Always carries the
                request id of the attempt that failed.
        """
        params = params or {}
        # "retries" extra attempts on top of the initial one.
        for attempt in range(1, retries + 2):
            request_id = self._next_request_id()
            try:
                return self._request_once(request_id, method, params, timeout)
            except IpcEngineError as exc:
                can_retry = exc.code in RETRYABLE_CODES and attempt <= retries
                if not can_retry:
                    raise
                logger.warning(
                    "request '%s' (id %s) failed with '%s' — retrying "
                    "(%d/%d)",
                    method,
                    request_id,
                    exc.code,
                    attempt,
                    retries,
                )
        # Unreachable: the last attempt either returns or raises.
        raise AssertionError("unreachable: retry loop exhausted")

    def _request_once(
        self,
        request_id: int,
        method: str,
        params: Dict[str, Any],
        timeout: Optional[float],
    ) -> Dict[str, Any]:
        """One full request/response exchange (no retry logic).

        Every socket operation runs under an explicit ``settimeout``
        (``self.timeout`` unless ``timeout`` overrides it), so a recv can
        never block indefinitely.  Raises :class:`IpcEngineError` carrying
        ``request_id`` on any failure.
        """
        effective_timeout = self.timeout if timeout is None else timeout
        payload = (
            json.dumps({"id": request_id, "method": method, "params": params})
            + "\n"  # NDJSON: every request must be a single, newline-terminated line.
        ).encode("utf-8")

        sock = None
        try:
            sock = self._connect(timeout)
            # Belt-and-suspenders: _connect already applied the timeout, but
            # keep the guarantee here too so a later refactor cannot leave a
            # blocking recv behind.
            sock.settimeout(effective_timeout)
            sock.sendall(payload)
            # Line-oriented reads: NDJSON, one JSON value per line.  Raw recv
            # with an internal buffer (no socket.makefile) so behaviour is
            # identical from any thread and leftover bytes from a multi-line
            # recv (events coalesced with the response) are not lost.
            reader = _SocketLineReader(sock)
            while True:
                line = reader.readline()
                if line is None:
                    raise IpcEngineError(
                        "connection_closed",
                        f"daemon closed the connection while answering "
                        f"'{method}' (is it still running?)",
                        request_id=request_id,
                    )
                try:
                    message = json.loads(line)
                except ValueError as exc:
                    raise IpcEngineError(
                        "parse_error",
                        f"malformed line from daemon: {line!r} ({exc})",
                        request_id=request_id,
                    ) from exc
                if "event" in message:
                    # Push notification (state_changed, …) — not our
                    # response; skip it and keep reading.
                    continue
                if message.get("id") != request_id:
                    # Response for an unknown request (stale) — ignore.
                    continue
                if message.get("ok"):
                    result = message.get("result")
                    return result if isinstance(result, dict) else {}
                error = message.get("error") or {}
                raise IpcEngineError(
                    str(error.get("code") or "error"),
                    str(error.get("message") or "daemon reported an error"),
                    request_id=request_id,
                )
        except socket.timeout:
            raise IpcEngineError(
                "timeout",
                f"no response from the daemon for '{method}' within "
                f"{effective_timeout:.1f}s",
                request_id=request_id,
            )
        except IpcEngineError as exc:
            # _connect / the loop above may have raised before request_id was
            # attached (e.g. daemon_not_running during connect).  Re-raise a
            # copy carrying this request's id so the caller can correlate.
            if exc.request_id is not None:
                raise
            raise IpcEngineError(exc.code, exc.message, request_id=request_id)
        except OSError as exc:
            raise IpcEngineError(
                "connection_error",
                f"error talking to the daemon for '{method}': {exc}",
                request_id=request_id,
            )
        finally:
            if sock is not None:
                sock.close()

    # ------------------------------------------------- AudioEngine surface
    def start_engine(self, strips: List[Strip]):
        """Replace the whole engine state (equivalent to ``start_engine``).

        Strips are converted to the Rust schema and the daemon runs every
        entry through the `holaf-core` migrations.  Errors propagate so a
        missing/unresponsive daemon surfaces as a clear startup failure.
        """
        self._strips = list(strips)
        payload = {"strips": [_strip_to_rust_dict(s) for s in strips]}
        # Snapshot replacement is heavy and state-replacing: never auto-retry
        # (a re-sent snapshot could double-build the graph).
        result = self._request(METHOD_APPLY_SNAPSHOT, payload, retries=0)
        applied = int((result or {}).get("applied", len(strips)))
        logger.info("start_engine: applied snapshot (%d strips)", applied)

    def shutdown(self):
        """Stop the engine and tear down the audio graph (best effort)."""
        try:
            # engine.stop is a teardown order — no auto-retry.
            self._request(METHOD_STOP, retries=0)
        except IpcEngineError as exc:
            # App-quit path: never crash because the daemon is gone.
            logger.debug("shutdown: %s", exc)

    def get_meter_levels(self) -> Dict[str, Any]:
        """STUB — returns ``{}`` (no VU meters) until J5.

        The rewrite spec §6 keeps metering Python-local (sounddevice) for now:
        the daemon does not expose per-strip peak levels until the optional
        native metering lands.  main_window's meter timer polls this method
        and simply finds no widgets to update.
        """
        # TODO(J5): either keep a local MeteringEngine fed by
        # engine.status/fx names, or read native meter.get (spec §6).
        return {}

    def set_volume(self, strip_uid: str, volume: float):
        self._safe_request(
            METHOD_STRIP_SET_VOLUME,
            {"uid": strip_uid, "value": float(volume)},
            "set_volume",
        )

    def set_mute(self, strip_uid: str, muted: bool):
        self._safe_request(
            METHOD_STRIP_SET_MUTE,
            {"uid": strip_uid, "muted": bool(muted)},
            "set_mute",
        )

    def set_mono(self, strip_uid: str, enabled: bool):
        self._safe_request(
            METHOD_STRIP_SET_MONO,
            {"uid": strip_uid, "mono": bool(enabled)},
            "set_mono",
        )

    def update_routing(self, source_uid: str, target_uid: str, active: bool):
        self._safe_request(
            METHOD_ROUTING_SET,
            {
                "source_uid": source_uid,
                "target_uid": target_uid,
                "active": bool(active),
            },
            "update_routing",
        )

    def set_system_default(self, strip_uid: str):
        self._safe_request(
            METHOD_SYSTEM_SET_DEFAULT_SINK,
            {"uid": strip_uid},
            "set_system_default",
        )

    def update_fx_params(self, strip: Strip) -> bool:
        """Hot-apply a strip's current FX state via ``fx.set_active`` +
        ``fx.set_params``.  Returns ``True`` when the daemon acknowledged the
        full push, ``False`` when a full reload/restart is needed instead
        (mirrors `AudioEngine.update_fx_params`).
        """
        try:
            self._push_fx_state(strip)
            return True
        except IpcEngineError as exc:
            logger.warning("update_fx_params(%s): %s", strip.uid, exc)
            return False

    def reload_strip(self, strip: Strip) -> bool:
        """Rebuild one strip's FX state on the daemon.

        The J4 protocol has no per-strip rebuild order — the daemon owns the
        graph and rebuilds chains internally when it receives FX intents.  We
        first push the strip's current FX state; if that cannot reach the
        daemon (e.g. it lost state and replies ``unknown_strip``), we fall
        back to re-applying the full snapshot from the last ``start_engine``
        call, which fully resyncs the engine.  Returns ``True`` on success so
        `main_window._apply_fx_change` skips its own full-restart fallback.
        """
        try:
            self._push_fx_state(strip)
            return True
        except IpcEngineError as exc:
            logger.warning("reload_strip(%s): FX push failed (%s); resyncing snapshot", strip.uid, exc)
        return self._resync_snapshot()

    # ----------------------------------------------------- device/app reads
    def status(self) -> Dict[str, Any]:
        """Query the daemon state (running flag, strips, routes, …)."""
        return self._request(METHOD_STATUS)

    def get_audio_nodes(self) -> Dict[str, Any]:
        """List devices known to the daemon (``device.list``).

        Returns the daemon's payload as-is for now — the Python
        `pipewire_utils.get_audio_nodes` node dict (id/name/description/…)
        mapping lands with the device milestone.
        """
        return self._request(METHOD_DEVICE_LIST)

    def get_sink_inputs(self) -> Dict[str, Any]:
        """List applications playing audio (``app.list``) as-is."""
        return self._request(METHOD_APP_LIST)

    def move_sink_input(self, app_id: Any, sink_name: str) -> bool:
        """Move an application to another sink (``app.move``)."""
        try:
            self._request(
                METHOD_APP_MOVE,
                {"app_id": str(app_id), "sink_name": sink_name},
            )
            return True
        except IpcEngineError as exc:
            logger.warning("move_sink_input: %s", exc)
            return False

    # ------------------------------------------------------------- internal
    def _safe_request(self, method: str, params: Dict[str, Any], op: str):
        """Send a lightweight order; log-and-swallow like `AudioEngine`, which
        never raises for missing nodes (it just early-returns)."""
        try:
            self._request(method, params)
        except IpcEngineError as exc:
            logger.warning("%s: %s", op, exc)

    def _push_fx_state(self, strip: Strip):
        """Push the strip's whole FX state as a sequence of IPC intents.

        Order matters for a real backend: activate the effect first (so the
        chain/plugin exists), then push its control values.  Inactive effects
        only need ``fx.set_active false`` — the daemon merges the neutral
        bypass values itself when it builds the graph.
        """
        raw_effects = getattr(strip, "effects", None) or {}
        for key in EFFECT_ORDER:
            data = raw_effects.get(key)
            if isinstance(data, dict):
                active = bool(data.get("active", False))
                params = data.get("params", {}) or {}
            else:
                active = bool(data)
                params = {}
            self._request(
                METHOD_FX_SET_ACTIVE,
                {"uid": strip.uid, "effect": key, "active": active},
            )
            if active and params:
                self._request(
                    METHOD_FX_SET_PARAMS,
                    {"uid": strip.uid, "effect": key, "params": dict(params)},
                )

    def _resync_snapshot(self) -> bool:
        """Re-apply the last known strips to the daemon (full resync)."""
        if not self._strips:
            logger.warning("_resync_snapshot: no cached strips; cannot resync")
            return False
        try:
            self.start_engine(self._strips)
            return True
        except IpcEngineError as exc:
            logger.warning("_resync_snapshot: %s", exc)
            return False
