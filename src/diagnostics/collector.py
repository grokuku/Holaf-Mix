"""Diagnostics collection — **pure** logic, no Qt, no globals.

Why this module exists
----------------------
When the mix "sounds wrong but looks fine", diagnosing it today means asking
the user to run a handful of shell commands (``pactl``, ``pw-cli``,
``pw-link``, ``pw-top``, ``holafctl status``) and paste the output.  This
module gathers exactly that information — plus the engine's own opinion of its
health — into one structured report, and renders it as a compact text block
the user can copy/paste in one click (see
:func:`format_report_text`).

Design contract
---------------
* **No Qt.**  The hidden diagnostics window (:mod:`src.ui.diagnostics_dialog`)
  only displays what this module returns, so everything here is unit-testable
  on a machine without PySide6 (see ``tests/test_diagnostics.py``).
* **Never raises.**  Diagnostics run precisely when something is broken, so a
  missing tool, a hung daemon, an engine that throws, an empty ``PATH``… must
  all degrade to ``"unavailable"`` + a reason inside the report instead of
  propagating.  Every external call is bounded by a short timeout
  (:data:`DEFAULT_SUBPROCESS_TIMEOUT_S`).
* **Works with both backends.**  The Rust daemon answers ``engine.status()``
  (``IpcAudioEngine``); the classic Python engine has no such method, so its
  state is reconstructed from its public-ish attributes + the strips the UI
  owns (passed through ``options["strips"]``).  Missing fields are reported as
  ``None`` and produce no finding on their own.
* **One report, two consumers.**  :func:`collect_report` returns a plain dict
  (the UI tabs read it), :func:`format_report_text` renders the same dict as a
  log-style text block, and :func:`detect_findings` is the pure rule engine
  (a list of ``{id, severity, message, hint}``) usable on its own.

Public API
----------
``collect_report(audio_engine=None, options=None) -> dict``
    Gather everything.  *options* (all optional):

    ``strips``             UI strips (``Strip`` objects or dicts) — the source
                           of truth for the Python engine's strip list, and the
                           reference for volume/config cross-checks.
    ``backend``            force ``"rust"`` / ``"python"`` (else auto-detected).
    ``config_path``        override the ``config.json`` path.
    ``app_version``        override the app version string.
    ``socket_path``        override the daemon socket path.
    ``include_graph``      collect the PipeWire graph (default ``True``).
    ``include_system``     collect the system section (default ``True``).
    ``include_logs``       collect the log tail (default ``True``).
    ``subprocess_timeout`` per-command budget in seconds (default 2.0).
    ``log_lines``          how many log lines to embed (default 200).
    ``log_lines_override`` explicit list of log lines (tests / file replay).
    ``env``                mapping used instead of ``os.environ`` for
                           subprocesses (tests: ``{"PATH": ""}``).

``detect_findings(report) -> list``
``format_report_text(report) -> str``

Report layout (keys are stable, the UI and the tests depend on them)::

    {schema, generated_at,
     environment:{app_version, backend, engine_class, engine_binary, …},
     engine:{available, reason, running, strips, routes, fx_host, pending, …},
     metering:{available, levels, sources, …},
     graph:{available, nodes, holaf_nodes, links, pw_top, …},
     system:{loadavg, memory, processes, …},
     logs:{available, lines},
     findings:[{id, severity, message, hint}],
     summary:{error, warning, info, total}}
"""

import copy
import json
import logging
import os
import platform
import re
import subprocess
import sys
from datetime import datetime, timezone
from typing import Any, Dict, List, Mapping, Optional, Sequence, Tuple

logger = logging.getLogger("Diagnostics")

# --- severities -----------------------------------------------------------
SEVERITY_ERROR = "error"
SEVERITY_WARNING = "warning"
SEVERITY_INFO = "info"
SEVERITIES = (SEVERITY_ERROR, SEVERITY_WARNING, SEVERITY_INFO)
_SEVERITY_RANK = {SEVERITY_ERROR: 0, SEVERITY_WARNING: 1, SEVERITY_INFO: 2}
#: Short label used in the text report / summary tab, per severity.
SEVERITY_TAGS = {SEVERITY_ERROR: "ERROR", SEVERITY_WARNING: "WARN", SEVERITY_INFO: "INFO"}
_SEVERITY_TAG = SEVERITY_TAGS

# --- tuning ---------------------------------------------------------------
#: Budget of a single external command.  Diagnostics run on a worker thread,
#: but a hung ``pw-dump`` (no PipeWire answering) must not pin it: every call
#: is bounded by this and reports "timeout" instead.
DEFAULT_SUBPROCESS_TIMEOUT_S = 2.0
#: How many log lines the report embeds (the Journal tab shows the same tail).
DEFAULT_LOG_LINES = 200
#: Report schema version (bumped when a key is removed/renamed).
REPORT_SCHEMA = 1
#: Prefix of every node the mix owns in the PipeWire graph.
HOLAF_NODE_PREFIX = "Holaf_"
#: Naming convention of the *classic* Python engine — and of the stale Rust
#: binaries that predate `Holaf_Bus_<Label>`.  Seeing it while the Rust backend
#: is active means the `holafd` binary is out of date.
LEGACY_STRIP_PREFIX = "Holaf_Strip_"
#: Below this quantum, a physical device is very likely to crackle (the
#: symptom that triggered this whole diagnostics window).
MIN_HEALTHY_QUANTUM = 256
#: ERR values in `pw-top` above this become an *error* instead of a warning.
PWT_ERROR_THRESHOLD = 100
#: Marker used by every "section turned off" reason (live auto-refresh).
#: A disabled section must not produce a finding — it was simply not collected.
DISABLED = "disabled"
#: `Holaf_*` nodes in these classes are the mix's own graph objects.
_AUDIO_CLASSES = ("Audio/Sink", "Audio/Source", "Stream/Input/Audio",
                  "Stream/Output/Audio")


# =========================================================================
# small helpers
# =========================================================================
def _now_iso() -> str:
    """UTC timestamp, second precision (stable for tests/goldens)."""
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def _positive_float(value: Any, default: float) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return default
    return number if number > 0 else default


def _positive_int(value: Any, default: int) -> int:
    try:
        number = int(value)
    except (TypeError, ValueError):
        return default
    return number if number > 0 else default


def _as_dict(value: Any) -> Dict[str, Any]:
    return value if isinstance(value, dict) else {}


def _as_list(value: Any) -> List[Any]:
    if isinstance(value, (list, tuple)):
        return list(value)
    return []


def _text(value: Any) -> Optional[str]:
    """*value* as a non-empty string, else ``None`` (keeps reports tidy)."""
    if value is None:
        return None
    text = str(value).strip()
    return text or None


def _run(
    command: Sequence[str],
    timeout: float = DEFAULT_SUBPROCESS_TIMEOUT_S,
    env: Optional[Mapping[str, str]] = None,
) -> Tuple[Optional[str], Optional[str]]:
    """Run *command*; return ``(stdout, None)`` or ``(None, reason)``.

    Never raises.  ``reason`` is a short human-readable string suitable for the
    report ("not found (is pw-dump installed?)", "timeout after 2s", the first
    stderr line, …).  ``env`` lets a test simulate an empty ``PATH``.
    """
    argv = [str(part) for part in command]
    try:
        result = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
            env=None if env is None else dict(env),
        )
    except FileNotFoundError:
        return None, "not found (is %s installed?)" % argv[0]
    except subprocess.TimeoutExpired:
        return None, "timeout after %gs" % timeout
    except (OSError, ValueError) as exc:  # EACCES, bad env, closed stdin…
        return None, "failed to run (%s)" % exc
    if result.returncode != 0:
        detail = ""
        for stream in (result.stderr, result.stdout):
            for line in (stream or "").splitlines():
                if line.strip():
                    detail = line.strip()
                    break
            if detail:
                break
        return None, detail or "exit code %d" % result.returncode
    return result.stdout, None


def _safe_call(func: Any, *args: Any, **kwargs: Any) -> Tuple[Any, Optional[str]]:
    """Call *func*; return ``(result, None)`` or ``(None, reason)``."""
    if not callable(func):
        return None, "not supported by this engine"
    try:
        return func(*args, **kwargs), None
    except Exception as exc:  # noqa: BLE001 - diagnostics must never raise
        logger.debug("diagnostics: %s failed: %s", getattr(func, "__name__", func), exc)
        return None, "%s: %s" % (exc.__class__.__name__, exc)


# =========================================================================
# environment
# =========================================================================
def _detect_backend(audio_engine: Any, options: Mapping[str, Any]) -> str:
    """``"rust"`` (IPC daemon) / ``"python"`` (classic) / ``"unknown"``.

    Duck-typed on purpose: the module path is checked first, then the class
    name, then the presence of the daemon-only ``status()`` method — so a test
    double can pretend to be either backend without importing the real one.
    """
    forced = _text(options.get("backend"))
    if forced:
        forced = forced.lower()
        return forced if forced in ("rust", "python") else "unknown"
    if audio_engine is None:
        return "unknown"
    module = (getattr(type(audio_engine), "__module__", "") or "").lower()
    name = (getattr(type(audio_engine), "__name__", "") or "").lower()
    if "ipc" in module or "ipc" in name:
        return "rust"
    if callable(getattr(audio_engine, "status", None)):
        return "rust"
    return "python"


def _app_version(options: Mapping[str, Any]) -> Optional[str]:
    override = _text(options.get("app_version"))
    if override:
        return override
    try:
        from src import __version__  # local import: keeps the module import cheap

        return _text(__version__)
    except Exception:  # noqa: BLE001 - version is cosmetic
        return None


def _config_path(options: Mapping[str, Any]) -> Optional[str]:
    override = _text(options.get("config_path"))
    if override:
        return override
    try:
        from src.config import settings

        return _text(getattr(settings, "CONFIG_FILE", None))
    except Exception:  # noqa: BLE001
        return None


def _socket_path(audio_engine: Any, options: Mapping[str, Any]) -> Optional[str]:
    """Socket the app talks to: the engine's own path first, then the default.

    The engine knows the truth (``IpcAudioEngine.socket_path`` was resolved
    from ``$HOLAF_SOCKET`` at startup); ``default_socket_path()`` is only the
    fallback so the report still shows something with the classic engine.
    """
    override = _text(options.get("socket_path"))
    if override:
        return override
    engine_path = _text(getattr(audio_engine, "socket_path", None))
    if engine_path:
        return engine_path
    try:
        from src.backend.ipc_audio_engine import default_socket_path

        return _text(default_socket_path())
    except Exception:  # noqa: BLE001
        return None


def _engine_binary() -> Optional[str]:
    """Path of the `holafd` binary the app would launch (best effort)."""
    try:
        from src.backend.rust_launcher import _find_holafd_binary

        return _text(_find_holafd_binary())
    except Exception:  # noqa: BLE001
        return None


def _pipewire_version(timeout: float, env: Optional[Mapping[str, str]]) -> Tuple[Optional[str], Optional[str]]:
    """PipeWire version via ``pw-cli --version`` (works without a daemon).

    Falls back to ``pkg-config --modversion libpipewire-0.3`` (the *compiled*
    version, which is what the pw-* tools link against anyway).
    """
    out, reason = _run(["pw-cli", "--version"], timeout, env)
    if out:
        match = re.search(r"(?:Linked|Compiled) with libpipewire\s+([0-9][\w.\-]*)", out)
        if match:
            return match.group(1), None
        match = re.search(r"(\d+\.\d+\.\d+)", out)
        if match:
            return match.group(1), None
        reason = "unrecognised output of `pw-cli --version`"
    out, pkg_reason = _run(["pkg-config", "--modversion", "libpipewire-0.3"], timeout, env)
    if out:
        return out.strip(), None
    return None, reason or pkg_reason


def _collect_environment(
    audio_engine: Any,
    backend: str,
    options: Mapping[str, Any],
    timeout: float,
    env: Optional[Mapping[str, str]],
) -> Dict[str, Any]:
    pipewire_version, pipewire_reason = _pipewire_version(timeout, env)
    binary = _engine_binary()
    binary_mtime = None
    if binary:
        try:
            binary_mtime = datetime.fromtimestamp(
                os.path.getmtime(binary), tz=timezone.utc
            ).replace(microsecond=0).isoformat()
        except OSError:
            binary_mtime = None

    unavailable: Dict[str, str] = {}
    if pipewire_reason and not pipewire_version:
        unavailable["pipewire_version"] = pipewire_reason

    try:
        uname = platform.uname()
        os_label = "%s %s (%s)" % (uname.system or "?", uname.release or "?", uname.machine or "?")
        kernel = uname.release or None
    except Exception:  # noqa: BLE001 - platform.uname is very unlikely to fail
        os_label, kernel = None, None
        unavailable["os"] = "platform.uname() failed"

    environment = {
        "app_version": _app_version(options),
        "backend": backend,
        "engine_class": type(audio_engine).__name__ if audio_engine is not None else None,
        "engine_module": getattr(type(audio_engine), "__module__", None) if audio_engine else None,
        "engine_binary": binary,
        "engine_binary_mtime": binary_mtime,
        # Set by main.create_audio_engine() when the Rust path degraded: the
        # report must say *why* the app is running the classic engine.
        "rust_fallback_reason": _text(getattr(audio_engine, "_rust_fallback_reason", None)),
        "config_path": _config_path(options),
        "app_pid": os.getpid(),
        "os": os_label,
        "kernel": kernel,
        "python": "%s (%s)" % (platform.python_version(), sys.executable or "?"),
        "pipewire_version": pipewire_version,
        "socket_path": _socket_path(audio_engine, options),
        "frozen": bool(getattr(sys, "frozen", False)),
        "unavailable": unavailable,
    }
    return environment


# =========================================================================
# engine state
# =========================================================================
def _strip_plain(strip: Any) -> Dict[str, Any]:
    """Normalise a ``Strip`` model object **or** a dict into a plain dict."""
    if isinstance(strip, dict):
        source = strip
    else:
        to_dict = getattr(strip, "to_dict", None)
        if callable(to_dict):
            value, _reason = _safe_call(to_dict)
            source = value if isinstance(value, dict) else {}
        else:
            source = {
                key: getattr(strip, key, None)
                for key in ("uid", "label", "kind", "mode", "volume", "mute",
                            "is_mono", "device_name", "effects", "routes",
                            "is_default")
            }
    return dict(source)


def _active_effects(effects: Any) -> List[str]:
    """Effect keys whose ``active`` flag is truthy, in mapping order."""
    active: List[str] = []
    for key, value in (_as_dict(effects)).items():
        if isinstance(value, dict):
            if value.get("active"):
                active.append(str(key))
        elif value:  # legacy bool form
            active.append(str(key))
    return active


def _normalise_strip(raw: Any, node_name: Optional[str] = None) -> Dict[str, Any]:
    strip = _strip_plain(raw)
    effects = _as_dict(strip.get("effects"))
    return {
        "uid": _text(strip.get("uid")),
        "label": _text(strip.get("label")),
        "kind": _text(strip.get("kind")),
        "mode": _text(strip.get("mode")),
        "node_name": _text(strip.get("node_name")) or _text(node_name),
        "volume": strip.get("volume"),
        "mute": strip.get("mute"),
        "is_mono": strip.get("is_mono"),
        "device_name": _text(strip.get("device_name")),
        "effects": _active_effects(effects),
        "routes": [str(route) for route in _as_list(strip.get("routes"))],
        "is_default": strip.get("is_default"),
    }


def _normalise_pending(raw: Any) -> List[Dict[str, str]]:
    pending: List[Dict[str, str]] = []
    for item in _as_list(raw):
        if isinstance(item, dict):
            pending.append(
                {"key": _text(item.get("key")) or "?",
                 "message": _text(item.get("message")) or ""}
            )
        else:
            pending.append({"key": "?", "message": _text(item) or ""})
    return pending


def _normalise_fx_host(raw: Any) -> Dict[str, Any]:
    host = _as_dict(raw)
    chains = _as_list(host.get("chains"))
    return {
        "mode": _text(host.get("mode")),
        "pid": host.get("pid"),
        "respawns": host.get("respawns"),
        "chains": [str(chain) for chain in chains],
        "alive": host.get("alive"),
    }


def _engine_from_status(status: Dict[str, Any]) -> Dict[str, Any]:
    """Normalise the daemon's ``engine.status`` payload (Rust backend)."""
    strips = [
        _normalise_strip(strip)
        for strip in _as_list(status.get("strips"))
        if isinstance(strip, dict)
    ]
    fx_host = _normalise_fx_host(status.get("fx_host"))
    # The daemon reports the mode it is running in; "external" means a
    # dedicated `holafd --fx-host` child owns the chains (the healthy shape).
    if fx_host["mode"] and fx_host["pid"] is not None:
        fx_host["alive"] = fx_host["alive"] if fx_host["alive"] is not None else True
    return {
        "available": True,
        "reason": None,
        "source": "engine.status()",
        "running": status.get("running"),
        "default_sink": _text(status.get("default_sink")),
        "degraded": bool(status.get("degraded")),
        "pending": _normalise_pending(status.get("pending")),
        "strips": strips,
        "routes": _as_list(status.get("routes")),
        "apps": _as_list(status.get("apps")),
        "fx_host": fx_host,
        "fx_chains": _as_list(status.get("fx_chains")),
        "meter_sources": {
            str(uid): _text(name)
            for uid, name in _as_dict(status.get("meter_sources")).items()
        },
        "mainloop_pumps": status.get("mainloop_pumps"),
        "holaf_nodes": [str(name) for name in _as_list(status.get("holaf_nodes"))],
    }


def _engine_from_python(audio_engine: Any, options: Mapping[str, Any]) -> Dict[str, Any]:
    """Reconstruct a status-like dict from the classic Python engine.

    The Python engine owns the graph directly and keeps no ``status()``: its
    state lives in ``node_registry`` / ``name_cache`` / ``fx_node_ids`` and the
    strip list belongs to the UI (``options["strips"]``).  Everything missing
    stays ``None`` — the findings rule engine only fires on facts it has.
    """
    registry = _as_dict(getattr(audio_engine, "node_registry", None))
    name_cache = _as_dict(getattr(audio_engine, "name_cache", None))
    fx_nodes = _as_dict(getattr(audio_engine, "fx_node_ids", None))
    strips_raw = _as_list(options.get("strips"))

    strips: List[Dict[str, Any]] = []
    for raw in strips_raw:
        strip = _normalise_strip(raw)
        uid = strip.get("uid")
        node_id = registry.get(uid) if uid else None
        if node_id is not None:
            strip["node_name"] = _text(name_cache.get(node_id)) or strip.get("node_name")
        if not strip.get("node_name") and uid:
            # The classic engine still names its virtual buses after the uid.
            strip["node_name"] = "%s%s" % (LEGACY_STRIP_PREFIX, uid)
        strips.append(strip)

    fx_host_process = getattr(audio_engine, "fx_host_process", None)
    host_pid = getattr(fx_host_process, "pid", None)
    host_alive: Optional[bool] = None
    if fx_host_process is not None:
        poll = getattr(fx_host_process, "poll", None)
        if callable(poll):
            alive, _reason = _safe_call(poll)
            host_alive = alive is None

    # An object exposing none of the engine's attributes cannot be described:
    # say so instead of pretending it is a healthy empty engine.
    if not strips and not any(
        hasattr(audio_engine, attr)
        for attr in ("node_registry", "created_nodes", "metering", "fx_node_ids")
    ):
        return {
            "available": False,
            "reason": "not a known audio engine (no status(), no node_registry)"
                      " — pass options['strips'] to describe it",
            "source": "Python engine attributes",
            "strips": [],
            "pending": [],
            "routes": [],
            "fx_host": _normalise_fx_host(None),
            "meter_sources": {},
            "holaf_nodes": [],
        }

    routes: List[Any] = []
    for strip in strips:
        for target in _as_list(strip.get("routes")):
            routes.append({"source_uid": strip.get("uid"), "target_uid": target})

    # Which source each VU stream records: `_stream_sources` (uid → name) is
    # the authoritative map in `MeteringEngine`; `active_streams` only holds
    # the open streams, without their source name.
    metering = getattr(audio_engine, "metering", None)
    active_sources: Dict[str, str] = {}
    for attr in ("_stream_sources", "stream_sources", "active_streams"):
        candidate = _as_dict(getattr(metering, attr, None))
        if candidate:
            active_sources = {str(uid): _text(value) for uid, value in candidate.items()}
            break

    return {
        "available": True,
        "reason": None,
        "source": "Python engine attributes",
        "running": bool(registry) if registry else None,
        "default_sink": _text(options.get("default_sink")),
        "degraded": None,
        "pending": [],
        "strips": strips,
        "routes": routes,
        "apps": [],
        # The Python engine hosts its chains in a persistent `pw-cli` child:
        # external by construction, with no respawn bookkeeping.
        "fx_host": {
            "mode": "external" if fx_host_process is not None else None,
            "pid": host_pid,
            "respawns": 0 if fx_host_process is not None else None,
            "chains": sorted(str(uid) for uid in fx_nodes),
            "alive": host_alive,
        },
        "fx_chains": [
            {"uid": str(uid), "chain": True} for uid in fx_nodes
        ],
        "meter_sources": active_sources,
        "mainloop_pumps": None,
        "holaf_nodes": [],
    }


def _collect_engine(audio_engine: Any, options: Mapping[str, Any], backend: str) -> Dict[str, Any]:
    if audio_engine is None:
        return {
            "available": False,
            "reason": "no audio engine attached",
            "source": None,
            "strips": [],
            "pending": [],
            "routes": [],
            "fx_host": _normalise_fx_host(None),
            "meter_sources": {},
            "holaf_nodes": [],
        }

    status_method = getattr(audio_engine, "status", None)
    if backend == "rust" or callable(status_method):
        status, reason = _safe_call(status_method)
        if isinstance(status, dict):
            return _engine_from_status(status)
        state = _engine_from_python(audio_engine, options)
        state["available"] = False
        state["reason"] = "engine.status() failed — %s" % (reason or "empty answer")
        state["source"] = "engine.status()"
        return state

    state = _engine_from_python(audio_engine, options)
    if not state["strips"]:
        state["reason"] = "the strip list was not provided (options['strips'])"
    return state


# =========================================================================
# metering
# =========================================================================
def _collect_metering(
    audio_engine: Any, engine_state: Mapping[str, Any]
) -> Dict[str, Any]:
    """Current VU levels + which sources are listened to.

    ``get_meter_levels()`` is polled by the UI at 25 Hz and always returns the
    cached level dict (it never blocks on IPC), so calling it once here is
    safe.  A test double may raise: the section then reports why.
    """
    metering: Dict[str, Any] = {
        "available": False,
        "reason": None,
        "levels": {},
        "sources": {},
        "loudest": None,
    }
    if audio_engine is None:
        metering["reason"] = "no audio engine attached"
        return metering

    levels, reason = _safe_call(getattr(audio_engine, "get_meter_levels", None))
    if isinstance(levels, dict):
        metering["available"] = True
        normalised: Dict[str, List[float]] = {}
        for uid, pair in levels.items():
            values = _as_list(pair)
            try:
                left = float(values[0]) if values else 0.0
                right = float(values[1]) if len(values) > 1 else left
            except (TypeError, ValueError):
                continue
            normalised[str(uid)] = [round(left, 4), round(right, 4)]
        metering["levels"] = normalised
        if normalised:
            loudest = max(normalised.items(), key=lambda item: max(item[1]))
            metering["loudest"] = {"uid": loudest[0], "left": loudest[1][0], "right": loudest[1][1]}
    else:
        metering["reason"] = reason or "get_meter_levels() not supported"

    metering["sources"] = {
        str(uid): _text(name)
        for uid, name in _as_dict(engine_state.get("meter_sources")).items()
    }
    return metering


# =========================================================================
# PipeWire graph
# =========================================================================
def _props_quantum_rate(props: Mapping[str, Any]) -> Tuple[Optional[int], Optional[int]]:
    """Quantum/rate from node props (``node.latency`` = ``1024/48000``).

    This is the *configured* latency of the node, not the live value `pw-top`
    shows — but it is available with plain ``pw-dump``, i.e. on every PipeWire
    version, which matters because ``pw-top -b`` only exists from 0.3.66.
    """
    quantum: Optional[int] = None
    rate: Optional[int] = None
    latency = props.get("node.latency")
    if isinstance(latency, str) and "/" in latency:
        num, _, den = latency.partition("/")
        try:
            quantum = int(num)
            rate = int(den)
        except ValueError:
            quantum, rate = None, None
    node_rate = props.get("node.rate")
    if isinstance(node_rate, str) and "/" in node_rate:
        _num, _, den = node_rate.partition("/")
        try:
            rate = int(den)
        except ValueError:
            pass
    return quantum, rate


def _params_quantum_rate(info: Mapping[str, Any]) -> Tuple[Optional[int], Optional[int]]:
    """Quantum/rate from the node's ``Latency``/``ProcessLatency`` params.

    A *running* node publishes ``ProcessLatency`` (``{quantum, rate, ns}``) and
    ``Latency`` (``{min_quantum, min_rate, ..., max_quantum, max_rate, ...}``,
    with a direction).  This is the only way to learn the real quantum from
    ``pw-dump`` alone, i.e. on PipeWire < 0.3.66 where ``pw-top -b`` does not
    exist.  Values are ``[value]`` or ``[id, value]`` depending on the
    PipeWire version — both shapes are accepted.
    """
    params = _as_dict(info.get("params"))
    for name in ("ProcessLatency", "Latency"):
        for entry in _as_list(params.get(name)):
            value: Any = entry
            if isinstance(entry, (list, tuple)) and len(entry) >= 2:
                value = entry[1]
            if not isinstance(value, dict):
                continue
            quantum = value.get("quantum")
            if quantum is None:
                quantum = value.get("min_quantum", value.get("max_quantum"))
            rate = value.get("rate")
            if rate is None:
                rate = value.get("min_rate", value.get("max_rate"))
            try:
                parsed_quantum = int(float(quantum)) if quantum is not None else None
            except (TypeError, ValueError):
                parsed_quantum = None
            try:
                parsed_rate = int(float(rate)) if rate is not None else None
            except (TypeError, ValueError):
                parsed_rate = None
            if parsed_quantum or parsed_rate:
                return parsed_quantum, parsed_rate
    return None, None


def parse_pw_dump(data: Any) -> Dict[str, Any]:
    """Extract nodes + links from a parsed ``pw-dump`` document (pure).

    Only what the diagnostics need is kept: node identity / class / state /
    configured quantum, and links between named nodes.  Ids are resolved to
    names because a report full of bare ids is unreadable, and the ids change
    on every restart.
    """
    nodes: List[Dict[str, Any]] = []
    links: List[Dict[str, Any]] = []
    if not isinstance(data, list):
        return {"nodes": nodes, "links": links, "link_count": 0}

    names: Dict[Any, str] = {}
    for obj in data:
        if not isinstance(obj, dict):
            continue
        if obj.get("type") != "PipeWire:Interface:Node":
            continue
        info = _as_dict(obj.get("info"))
        props = _as_dict(info.get("props"))
        name = _text(props.get("node.name"))
        if not name:
            continue
        quantum, rate = _props_quantum_rate(props)
        if quantum is None and rate is None:
            # `node.latency` only exists on configured nodes; a running one
            # publishes its live quantum through the Latency params instead.
            quantum, rate = _params_quantum_rate(info)
        node = {
            "id": obj.get("id"),
            "name": name,
            "media_class": _text(props.get("media.class")),
            "description": _text(props.get("node.description")) or _text(props.get("node.nick")),
            "state": _text(info.get("state")),
            "quantum": quantum,
            "rate": rate,
            "ports_in": info.get("n-input-ports"),
            "ports_out": info.get("n-output-ports"),
            "is_holaf": name.startswith(HOLAF_NODE_PREFIX),
            "is_monitor": ".monitor" in name.lower(),
            "error": _text(info.get("error")),
        }
        names[obj.get("id")] = name
        nodes.append(node)

    link_ports: Dict[Any, Dict[str, str]] = {}
    for obj in data:
        if not isinstance(obj, dict):
            continue
        if obj.get("type") != "PipeWire:Interface:Port":
            continue
        info = _as_dict(obj.get("info"))
        props = _as_dict(info.get("props"))
        port_name = _text(props.get("port.name")) or _text(props.get("port.id"))
        if port_name:
            link_ports[obj.get("id")] = {"name": port_name, "direction": _text(props.get("port.direction"))}

    for obj in data:
        if not isinstance(obj, dict):
            continue
        if obj.get("type") != "PipeWire:Interface:Link":
            continue
        info = _as_dict(obj.get("info"))
        out_id = info.get("output-node-id")
        in_id = info.get("input-node-id")
        out_out = link_ports.get(info.get("output-port-id"), {})
        in_out = link_ports.get(info.get("input-port-id"), {})
        links.append(
            {
                "output": names.get(out_id),
                "output_port": out_out.get("name"),
                "input": names.get(in_id),
                "input_port": in_out.get("name"),
                "state": _text(info.get("state")),
                "is_holaf": bool(
                    (names.get(out_id) or "").startswith(HOLAF_NODE_PREFIX)
                    or (names.get(in_id) or "").startswith(HOLAF_NODE_PREFIX)
                ),
            }
        )

    return {"nodes": nodes, "links": links, "link_count": len(links)}


_PW_TOP_ROW = re.compile(
    r"^\s*(?P<state>[A-Za-z])\s+(?P<id>\d+)\s+(?P<quantum>\d+)\s+(?P<rate>\d+)\s+"
    r"(?P<wait>\S+)\s+(?P<busy>\S+)\s+(?P<wq>\S+)\s+(?P<bq>\S+)\s+(?P<err>\d+)\s+"
    r"(?P<rest>.+?)\s*$"
)


def parse_pw_top(text: str) -> Dict[str, Any]:
    """Parse ``pw-top -b -n 2`` output into per-node rows (pure).

    The batch mode prints one block per iteration and repeats the header;
    later blocks carry real measurements, so rows are keyed by node id and the
    last occurrence wins.  ``pw-top`` older than 0.3.66 has no ``-b`` at all —
    that is the caller's problem, this function only parses text.
    """
    rows: Dict[Any, Dict[str, Any]] = {}
    errors_total = 0
    format_type = None
    for line in (text or "").splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("S "):
            continue
        if stripped.lower().startswith(("error", "can't", "failed")):
            continue
        match = _PW_TOP_ROW.match(line)
        if not match:
            continue
        rest = match.group("rest").split()
        # `rest` is "<format type> <channels> <rate> <node name…>"; keep the
        # node name only (it is what the user can match against pw-dump).
        name = " ".join(rest[3:]) if len(rest) > 3 else " ".join(rest)
        try:
            err = int(match.group("err"))
        except ValueError:
            err = 0
        errors_total += err
        if len(rest) >= 1:
            format_type = rest[0]
        node_id = int(match.group("id"))
        rows[node_id] = {
            "id": node_id,
            "state": match.group("state").upper(),
            "quantum": int(match.group("quantum")),
            "rate": int(match.group("rate")),
            "wait": match.group("wait"),
            "busy": match.group("busy"),
            "err": err,
            "format": format_type,
            "name": name or None,
        }
    ordered = [rows[key] for key in sorted(rows)]
    return {"rows": ordered, "errors": errors_total, "node_count": len(ordered)}


def _collect_graph(
    options: Mapping[str, Any],
    timeout: float,
    engine_state: Mapping[str, Any],
    env: Optional[Mapping[str, str]],
) -> Dict[str, Any]:
    """Nodes / links / quantum of the live PipeWire graph (best effort)."""
    graph: Dict[str, Any] = {
        "available": False,
        "reason": None,
        "source": "pw-dump",
        "nodes": [],
        "holaf_nodes": [],
        "links": [],
        "link_count": 0,
        "pw_top": {"available": False, "reason": None, "rows": [], "errors": 0},
        "device_nodes": [],
    }

    dump_text, dump_reason = _run(["pw-dump"], timeout, env)
    parsed: Dict[str, Any] = {"nodes": [], "links": [], "link_count": 0}
    if dump_text:
        try:
            parsed = parse_pw_dump(json.loads(dump_text))
            graph["available"] = True
        except (json.JSONDecodeError, TypeError) as exc:
            graph["reason"] = "could not parse pw-dump output (%s)" % exc
    else:
        graph["reason"] = dump_reason or "pw-dump produced no output"

    nodes = parsed["nodes"]
    graph["nodes"] = nodes
    holaf = [node for node in nodes if node["is_holaf"]]
    graph["holaf_nodes"] = holaf
    device_nodes = [
        node
        for node in nodes
        if not node["is_holaf"]
        and not node["is_monitor"]
        and (node["media_class"] or "") in ("Audio/Sink", "Audio/Source")
    ]
    graph["device_nodes"] = device_nodes

    # Links: only the ones that touch the mix (a busy session has hundreds of
    # links between unrelated apps, which would bury the useful lines).
    interesting = {node["name"] for node in holaf + device_nodes}
    referenced = {
        strip.get("device_name")
        for strip in _as_list(engine_state.get("strips"))
        if strip.get("device_name")
    }
    interesting |= referenced
    selected = [
        link
        for link in parsed["links"]
        if link["is_holaf"]
        or link["output"] in interesting
        or link["input"] in interesting
    ]
    graph["links"] = selected
    graph["link_count"] = parsed["link_count"]
    graph["interesting_links"] = len(selected)

    # Live quantum/errors from pw-top (batch mode: two iterations, the second
    # one carries real values — `-n 1` prints zeros only).
    top_text, top_reason = _run(["pw-top", "-b", "-n", "2"], timeout, env)
    if top_text:
        top = parse_pw_top(top_text)
        graph["pw_top"] = {
            "available": bool(top["rows"]),
            "reason": None if top["rows"] else "pw-top printed no rows",
            "rows": top["rows"],
            "errors": top["errors"],
        }
    else:
        reason = top_reason or "pw-top produced no output"
        if top_reason and "invalid option" in top_reason:
            reason = "pw-top has no batch mode (needs PipeWire >= 0.3.66)"
        graph["pw_top"] = {"available": False, "reason": reason, "rows": [], "errors": 0}

    # Fold the live quantum into the node list when pw-top knows the node.
    by_name = {
        row["name"]: row
        for row in graph["pw_top"]["rows"]
        if row.get("name")
    }
    for node in nodes:
        row = by_name.get(node["name"])
        if row:
            node["live_quantum"] = row["quantum"]
            node["live_rate"] = row["rate"]
            if row.get("err"):
                node["live_err"] = row["err"]
            if node.get("state") is None:
                node["state"] = row["state"]

    return graph


# =========================================================================
# system
# =========================================================================
_CLK_TCK = None


def _clock_ticks() -> int:
    """``sysconf(_SC_CLK_TCK)`` (100 almost everywhere) — cached."""
    global _CLK_TCK
    if _CLK_TCK is None:
        try:
            _CLK_TCK = int(os.sysconf("SC_CLK_TCK"))
        except (AttributeError, ValueError, OSError):
            _CLK_TCK = 100
    return _CLK_TCK


def _read_proc_stat(pid: int) -> Optional[Dict[str, Any]]:
    """Parse ``/proc/<pid>/stat`` (field indices follow proc(5))."""
    try:
        with open("/proc/%d/stat" % int(pid), "r", encoding="utf-8", errors="replace") as handle:
            raw = handle.read()
    except (OSError, ValueError, TypeError):
        return None
    # The comm field is parenthesised and may contain spaces/parentheses:
    # split after the LAST ')' so the numeric fields line up.
    _, _, tail = raw.rpartition(")")
    fields = tail.split()
    if len(fields) < 22:
        return None
    try:
        return {
            "state": fields[0],
            "ppid": int(fields[1]),
            "utime": int(fields[11]),
            "stime": int(fields[12]),
            "starttime": int(fields[19]),
        }
    except (ValueError, IndexError):
        return None


def _read_rss_mb(pid: int) -> Optional[float]:
    """Resident set size in MB from ``/proc/<pid>/status`` (kB there)."""
    try:
        with open("/proc/%d/status" % int(pid), "r", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                if line.startswith("VmRSS:"):
                    parts = line.split()
                    return round(int(parts[1]) / 1024.0, 1)
    except (OSError, ValueError, IndexError, TypeError):
        return None
    return None


def _read_cmdline(pid: int) -> List[str]:
    try:
        with open("/proc/%d/cmdline" % int(pid), "rb") as handle:
            raw = handle.read()
    except (OSError, ValueError, TypeError):
        return []
    return [part.decode("utf-8", "replace") for part in raw.split(b"\0") if part]


def _uptime_s() -> Optional[float]:
    try:
        with open("/proc/uptime", "r", encoding="utf-8") as handle:
            return float(handle.read().split()[0])
    except (OSError, ValueError, IndexError):
        return None


def scan_holaf_processes() -> List[Dict[str, Any]]:
    """Every live ``holafd`` process, as ``/proc`` sees it.

    A ``/proc`` scan (rather than a pid from the launcher) is deliberate: it
    also catches daemons the app did **not** spawn — the orphans left by a
    previous session, which are the usual cause of "the UI is connected to a
    daemon that no longer owns the graph".
    """
    processes: List[Dict[str, Any]] = []
    try:
        entries = os.listdir("/proc")
    except OSError:
        return processes
    uptime = _uptime_s()
    ticks = _clock_ticks()
    for entry in entries:
        if not entry.isdigit():
            continue
        pid = int(entry)
        argv = _read_cmdline(pid)
        if not argv:
            continue
        # Match the basename so a daemon launched with an absolute path
        # (`/projects/Holaf-Mix/dist/holafd`) is found too.
        if os.path.basename(argv[0]) != "holafd":
            continue
        stat = _read_proc_stat(pid) or {}
        cpu_percent = None
        if stat and uptime:
            elapsed = uptime - (stat["starttime"] / float(ticks))
            if elapsed > 0:
                cpu_seconds = (stat["utime"] + stat["stime"]) / float(ticks)
                cpu_percent = round(100.0 * cpu_seconds / elapsed, 1)
        role = "fx-host" if "--fx-host" in argv[1:] else "daemon"
        socket_path = None
        for index, arg in enumerate(argv):
            if arg == "--socket" and index + 1 < len(argv):
                socket_path = argv[index + 1]
                break
        processes.append(
            {
                "pid": pid,
                "ppid": stat.get("ppid"),
                "state": stat.get("state"),
                "role": role,
                "socket_path": socket_path,
                "cmdline": " ".join(argv),
                # Average CPU over the process lifetime: a single /proc read
                # cannot give an instantaneous rate, and sleeping 100 ms per
                # process inside the report is not worth it.
                "cpu_percent_avg": cpu_percent,
                "rss_mb": _read_rss_mb(pid),
            }
        )
    processes.sort(key=lambda item: (item["role"] != "daemon", item["pid"]))
    return processes


def _read_meminfo() -> Dict[str, Any]:
    info: Dict[str, Any] = {}
    try:
        with open("/proc/meminfo", "r", encoding="utf-8") as handle:
            for line in handle:
                key, _, rest = line.partition(":")
                parts = rest.split()
                if not parts:
                    continue
                try:
                    value_kb = int(parts[0])
                except ValueError:
                    continue
                info[key.strip()] = value_kb
    except OSError:
        return {}
    total = info.get("MemTotal")
    available = info.get("MemAvailable", info.get("MemFree"))
    memory: Dict[str, Any] = {}
    if total:
        memory["total_mb"] = round(total / 1024.0, 1)
    if available is not None:
        memory["available_mb"] = round(available / 1024.0, 1)
    if total and available is not None:
        memory["used_mb"] = round((total - available) / 1024.0, 1)
        memory["used_percent"] = round(100.0 * (total - available) / total, 1)
    if info.get("SwapTotal"):
        memory["swap_total_mb"] = round(info["SwapTotal"] / 1024.0, 1)
        memory["swap_free_mb"] = round(info.get("SwapFree", 0) / 1024.0, 1)
    return memory


def _collect_system(
    options: Mapping[str, Any], engine_state: Mapping[str, Any]
) -> Dict[str, Any]:
    if options.get("include_system") is False:
        return {"available": False, "reason": DISABLED}
    system: Dict[str, Any] = {"available": True, "reason": None}
    try:
        system["loadavg"] = [round(value, 2) for value in os.getloadavg()]
    except (OSError, AttributeError):
        system["loadavg"] = None
    system["cpu_count"] = os.cpu_count()
    system["memory"] = _read_meminfo()
    system["uptime_s"] = _uptime_s()

    processes = scan_holaf_processes()
    system["processes"] = processes
    system["daemon_count"] = sum(1 for proc in processes if proc["role"] == "daemon")
    system["fx_host_count"] = sum(1 for proc in processes if proc["role"] == "fx-host")

    # The app itself: nice to correlate a CPU spike with the report.
    app_stat = _read_proc_stat(os.getpid()) or {}
    system["app"] = {
        "pid": os.getpid(),
        "rss_mb": _read_rss_mb(os.getpid()),
        "threads": _thread_count(os.getpid()),
        "cpu_percent_avg": _lifetime_cpu_percent(app_stat),
    }
    system["engine_pid"] = engine_state.get("fx_host", {}).get("pid")
    return system


def _lifetime_cpu_percent(stat: Mapping[str, Any]) -> Optional[float]:
    uptime = _uptime_s()
    if not stat or not uptime:
        return None
    try:
        elapsed = uptime - (stat["starttime"] / float(_clock_ticks()))
        if elapsed <= 0:
            return None
        return round(100.0 * ((stat["utime"] + stat["stime"]) / float(_clock_ticks())) / elapsed, 1)
    except (KeyError, TypeError, ValueError, ZeroDivisionError):
        return None


def _thread_count(pid: int) -> Optional[int]:
    try:
        with open("/proc/%d/status" % int(pid), "r", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                if line.startswith("Threads:"):
                    return int(line.split()[1])
    except (OSError, ValueError, IndexError):
        return None
    return None


# =========================================================================
# logs
# =========================================================================
def _collect_logs(options: Mapping[str, Any]) -> Dict[str, Any]:
    override = options.get("log_lines_override")
    if isinstance(override, (list, tuple)):
        lines = [str(line) for line in override]
        return {"available": True, "source": "provided", "lines": lines}
    if options.get("include_logs") is False:
        return {"available": False, "reason": DISABLED, "lines": []}
    limit = _positive_int(options.get("log_lines"), DEFAULT_LOG_LINES)
    try:
        from src.diagnostics.logbuffer import get_log_lines

        lines = get_log_lines(limit)
    except Exception as exc:  # noqa: BLE001 - the log buffer is optional
        return {"available": False, "reason": str(exc), "lines": []}
    if not lines:
        return {
            "available": False,
            "reason": "no in-memory log handler installed (see main.py)",
            "lines": [],
        }
    return {"available": True, "source": "in-memory ring buffer", "lines": lines}


# =========================================================================
# findings (pure rule engine)
# =========================================================================
def _finding(fid: str, severity: str, message: str, hint: Optional[str] = None) -> Dict[str, Any]:
    return {"id": fid, "severity": severity, "message": message, "hint": hint}


def _summarise_pending(pending: Sequence[Mapping[str, Any]], limit: int = 3) -> str:
    """First *limit* pending items, then ``… +N more``.

    A degraded daemon can report ten items with multi-line messages; pasting
    all of them into the one-line finding made the summary unreadable (the full
    list is still in the Engine tab / ENGINE section).
    """
    items = [item for item in pending if isinstance(item, Mapping)]
    parts = []
    for item in items[:limit]:
        message = " ".join(str(item.get("message") or "").split())
        if len(message) > 140:
            message = message[:137] + "…"
        parts.append("%s (%s)" % (item.get("key"), message))
    remaining = len(items) - len(parts)
    if remaining > 0:
        parts.append("… +%d more (see the ENGINE section)" % remaining)
    return "; ".join(parts)


def detect_findings(report: Mapping[str, Any]) -> List[Dict[str, Any]]:
    """Run every automatic check on *report*; return sorted findings.

    Sorted by severity (error → warning → info) then by id, so the same broken
    state always prints in the same order (the text report is meant to be
    diffable / pasteable).
    """
    environment = _as_dict(report.get("environment"))
    engine = _as_dict(report.get("engine"))
    metering = _as_dict(report.get("metering"))
    graph = _as_dict(report.get("graph"))
    system = _as_dict(report.get("system"))
    logs = _as_dict(report.get("logs"))
    backend = environment.get("backend")
    findings: List[Dict[str, Any]] = []

    # --- 1. the engine itself ---------------------------------------------
    if not engine.get("available"):
        if backend == "rust":
            findings.append(
                _finding(
                    "daemon_unreachable", SEVERITY_ERROR,
                    "the Rust daemon could not be queried: %s" % (engine.get("reason") or "unknown reason"),
                    "run `holafctl status` (socket: %s) and check the holafd log"
                    % (environment.get("socket_path") or "?"),
                )
            )
        else:
            findings.append(
                _finding(
                    "engine_unavailable", SEVERITY_WARNING,
                    "the audio engine state is unavailable: %s" % (engine.get("reason") or "unknown reason"),
                    "restart Holaf-Mix; if it persists, attach the report to a bug report",
                )
            )
    elif engine.get("running") is False:
        findings.append(
            _finding(
                "engine_not_running", SEVERITY_WARNING,
                "the engine reports running=false: the graph is not built",
                "start playback or re-apply the configuration (add/remove a strip)",
            )
        )

    fallback_reason = environment.get("rust_fallback_reason")
    if fallback_reason:
        findings.append(
            _finding(
                "backend_fallback", SEVERITY_WARNING,
                "the Rust backend was unavailable, the app fell back to the classic Python engine: %s"
                % fallback_reason,
                "rebuild/reinstall holafd (scripts/build_rust.sh) to get the Rust backend back",
            )
        )

    # --- 2. graph health as the engine sees it ----------------------------
    pending = _as_list(engine.get("pending"))
    if engine.get("degraded"):
        detail = _summarise_pending(pending) or "no details reported"
        findings.append(
            _finding(
                "graph_degraded", SEVERITY_ERROR,
                "the engine reports a DEGRADED graph — %s" % detail,
                "a device/route/chain could not be built; re-plug the device, then "
                "re-apply the configuration. This is the first thing to fix.",
            )
        )
    elif pending:
        # A daemon that does not set the flag but still lists retries: the
        # pending items are the real information, report them anyway.
        findings.append(
            _finding(
                "graph_pending", SEVERITY_WARNING,
                "the engine is still retrying incomplete graph work: %s"
                % _summarise_pending(pending),
                "usually a device that is not enumerated yet; re-check in a few seconds",
            )
        )

    # --- 3. FX host --------------------------------------------------------
    fx_host = _as_dict(engine.get("fx_host"))
    mode = (fx_host.get("mode") or "").lower()
    if mode == "inprocess":
        findings.append(
            _finding(
                "fx_host_inprocess", SEVERITY_WARNING,
                "the FX chains are hosted in the daemon process (mode=inprocess) instead of a "
                "dedicated --fx-host child",
                "the in-process mode is the legacy path and is the known cause of crackling/"
                "chopping chains; restart holafd so it can spawn its FX host",
            )
        )
    elif mode == "external" and fx_host.get("alive") is False:
        findings.append(
            _finding(
                "fx_host_down", SEVERITY_ERROR,
                "the FX host process is gone (pid %s) — chains are not running"
                % (fx_host.get("pid") or "?"),
                "the daemon respawns it on demand; if respawns keeps climbing, check the logs",
            )
        )
    respawns = fx_host.get("respawns")
    if isinstance(respawns, (int, float)) and respawns > 0:
        severity = SEVERITY_ERROR if respawns >= 3 else SEVERITY_WARNING
        findings.append(
            _finding(
                "fx_host_respawns", severity,
                "the FX host has been respawned %d time(s) — it is crashing" % respawns,
                "look for a LADSPA plugin error or a memlock limit in the logs (Journal tab)",
            )
        )

    pumps = engine.get("mainloop_pumps")
    chains = _as_list(fx_host.get("chains"))
    # `idle_pumps` only stays at 0 when the daemon never ran an idle pass: a
    # healthy daemon (with or without chains) bumps it immediately, so a 0 here
    # means its PipeWire mainloop is not being serviced — the modules it hosts
    # (filter-chains) then starve and the audio chops.
    if (
        backend == "rust"
        and engine.get("available")
        and engine.get("running")
        and pumps is not None
        and not pumps
    ):
        if chains:
            findings.append(
                _finding(
                    "mainloop_pumps_stalled", SEVERITY_WARNING,
                    "mainloop_pumps=0 while %d FX chain(s) are declared: the daemon is not "
                    "servicing its PipeWire mainloop" % len(chains),
                    "chains starve and the audio chops; restart the daemon",
                )
            )
        else:
            # No chain to starve: either the daemon has not run an idle pass
            # yet, or it is a build that does not maintain the counter.
            findings.append(
                _finding(
                    "mainloop_pumps_zero", SEVERITY_INFO,
                    "mainloop_pumps=0 and no FX chain declared: the daemon has not serviced "
                    "an idle pass yet (or this build does not maintain the counter)",
                    "not fatal; if the FX chains crackle, treat it as a dead mainloop",
                )
            )

    # --- 4. node naming (stale binary detection) --------------------------
    strips = [strip for strip in _as_list(engine.get("strips")) if isinstance(strip, dict)]
    legacy_strips = [
        strip for strip in strips
        if (strip.get("node_name") or "").startswith(LEGACY_STRIP_PREFIX)
    ]
    legacy_nodes = [
        node.get("name") for node in _as_list(graph.get("nodes"))
        if isinstance(node, dict) and (node.get("name") or "").startswith(LEGACY_STRIP_PREFIX)
    ]
    if backend == "rust" and (legacy_strips or legacy_nodes):
        names = sorted({strip.get("node_name") for strip in legacy_strips} | set(legacy_nodes))
        findings.append(
            _finding(
                "legacy_bus_names", SEVERITY_WARNING,
                "legacy `Holaf_Strip_<uid>` bus names on a Rust backend: %s" % ", ".join(names),
                "bus names follow the label (`Holaf_Bus_<Label>`) since J5 — the holafd binary "
                "in use is stale, rebuild it (scripts/build_rust.sh) and restart",
            )
        )

    if (
        backend == "rust"
        and strips
        and all(not strip.get("node_name") for strip in strips)
    ):
        findings.append(
            _finding(
                "strip_bus_unnamed", SEVERITY_WARNING,
                "the daemon does not publish `node_name` for any of its %d strips"
                % len(strips),
                "that field came with the label-based bus names (J5): the holafd binary in "
                "use is stale — rebuild it (scripts/build_rust.sh) and restart",
            )
        )

    # --- 5. strips vs live graph -----------------------------------------
    if graph.get("available"):
        graph_names = {node.get("name") for node in _as_list(graph.get("nodes")) if isinstance(node, dict)}
        device_names = graph_names
        for strip in strips:
            label = strip.get("label") or strip.get("uid") or "?"
            node_name = strip.get("node_name")
            if node_name and node_name not in graph_names:
                findings.append(
                    _finding(
                        "strip_bus_missing", SEVERITY_ERROR,
                        "strip '%s' declares bus '%s' but that node is absent from the PipeWire graph"
                        % (label, node_name),
                        "the bus was not created (daemon error, or the strip was removed from the "
                        "graph): re-apply the configuration",
                    )
                )
            device = strip.get("device_name")
            if device and device not in device_names:
                findings.append(
                    _finding(
                        "device_missing", SEVERITY_ERROR,
                        "strip '%s' references device '%s' which is not in the graph"
                        % (label, device),
                        "the device is unplugged/renamed; re-select it on the strip",
                    )
                )

    # --- 6. quantum / errors (the crackle signature) ----------------------
    small_quantum = []
    for node in _as_list(graph.get("device_nodes")):
        if not isinstance(node, dict):
            continue
        quantum = node.get("live_quantum") or node.get("quantum")
        if isinstance(quantum, int) and 0 < quantum < MIN_HEALTHY_QUANTUM:
            small_quantum.append("%s (quantum %d)" % (node.get("name"), quantum))
    if small_quantum:
        findings.append(
            _finding(
                "small_quantum", SEVERITY_WARNING,
                "quantum below %d on physical device(s): %s"
                % (MIN_HEALTHY_QUANTUM, ", ".join(small_quantum)),
                "a very small quantum overruns easily on a busy CPU (crackling): raise it "
                "(default.clock.quantum) or lower the sample rate",
            )
        )

    pw_top = _as_dict(graph.get("pw_top"))
    top_errors = pw_top.get("errors")
    if isinstance(top_errors, int) and top_errors > 0:
        severity = SEVERITY_ERROR if top_errors >= PWT_ERROR_THRESHOLD else SEVERITY_WARNING
        worst = sorted(
            (row for row in _as_list(pw_top.get("rows")) if isinstance(row, dict) and row.get("err")),
            key=lambda row: row.get("err") or 0,
            reverse=True,
        )[:3]
        detail = ", ".join("%s (ERR %d)" % (row.get("name"), row.get("err")) for row in worst)
        findings.append(
            _finding(
                "pwt_errors", severity,
                "%d processing error(s) reported by pw-top: %s" % (top_errors, detail),
                "xruns: the graph misses its deadline (CPU load, quantum, a hungry LADSPA chain)",
            )
        )

    if not graph.get("available") and backend in ("rust", "python") \
            and (graph.get("reason") or "") != DISABLED:
        findings.append(
            _finding(
                "pipewire_graph_unavailable", SEVERITY_ERROR,
                "the PipeWire graph could not be read: %s" % (graph.get("reason") or "unknown reason"),
                "is PipeWire running for this user? (scripts/start-pipewire-headless.sh in a "
                "container, `systemctl --user status pipewire` on a desktop)",
            )
        )

    top_reason = pw_top.get("reason")
    if top_reason and graph.get("available"):
        findings.append(
            _finding(
                "pw_top_unavailable", SEVERITY_INFO,
                "live quantum/error statistics unavailable: %s" % top_reason,
                "not fatal — quantum/rate still come from pw-dump",
            )
        )

    # --- 7. volumes / configuration drift --------------------------------
    ui_strips = {
        str(plain.get("uid")): plain
        for plain in (_strip_plain(raw) for raw in _as_list(report.get("_ui_strips")))
        if plain.get("uid")
    }
    unity = []
    for strip in strips:
        uid = strip.get("uid")
        volume = strip.get("volume")
        if uid in ui_strips and isinstance(volume, (int, float)):
            ui_volume = ui_strips[uid].get("volume")
            try:
                if ui_volume is not None and abs(float(ui_volume) - float(volume)) > 1e-3:
                    findings.append(
                        _finding(
                            "volume_mismatch", SEVERITY_WARNING,
                            "strip '%s': the engine volume is %.3f but the configuration says %.3f"
                            % (strip.get("label") or uid, float(volume), float(ui_volume)),
                            "the volume was not applied to the graph — move the fader once to re-apply it",
                        )
                    )
            except (TypeError, ValueError):
                pass
        if isinstance(volume, (int, float)) and abs(float(volume) - 1.0) < 1e-6:
            unity.append(strip.get("label") or uid or "?")
    if len(unity) >= 3 and len(unity) == len(strips):
        findings.append(
            _finding(
                "volume_unity", SEVERITY_INFO,
                "every strip sits at exactly 1.000: %s" % ", ".join(sorted(unity)),
                "expected if you never touched the faders; otherwise the volumes did not reach "
                "the graph",
            )
        )

    # --- 8. FX state vs chains ------------------------------------------
    chain_entries = [
        entry for entry in _as_list(engine.get("fx_chains")) if isinstance(entry, dict)
    ]
    chain_uids = {entry.get("uid") for entry in chain_entries if entry.get("chain")}
    if not chain_entries and chains:
        chain_uids = set(chains)
    # Only judge strips against the chains when the engine actually reports
    # chain bookkeeping: an engine that says nothing must not look broken.
    chains_reported = bool(chain_entries) or bool(chains)
    for strip in strips:
        effects = _as_list(strip.get("effects"))
        uid = strip.get("uid")
        if effects and chains_reported and uid not in chain_uids:
            findings.append(
                _finding(
                    "fx_chain_missing", SEVERITY_WARNING,
                    "strip '%s' has active effect(s) %s but no FX chain in the graph"
                    % (strip.get("label") or uid, ", ".join(effects)),
                    "toggle one of the effects off/on to rebuild the chain",
                )
            )
        elif not effects and uid in chain_uids:
            findings.append(
                _finding(
                    "fx_chain_leftover", SEVERITY_INFO,
                    "strip '%s' has an FX chain but no active effect (leftover chain)"
                    % (strip.get("label") or uid),
                    "harmless; it disappears when the strip is reloaded",
                )
            )

    # --- 9. metering -----------------------------------------------------
    if not metering.get("available") and (metering.get("reason") or "") != DISABLED:
        findings.append(
            _finding(
                "metering_unavailable", SEVERITY_WARNING,
                "VU metering is unavailable: %s" % (metering.get("reason") or "unknown reason"),
                "the meters are decorative here, but a real capture problem also breaks them: "
                "check that the PulseAudio bridge / sources exist",
            )
        )
    elif engine.get("running") and strips and not _as_dict(metering.get("sources")):
        findings.append(
            _finding(
                "metering_no_source", SEVERITY_WARNING,
                "the engine is running and strips exist, but no metering source is registered",
                "meter sources change with the FX chain lifecycle; wait 2 s (the engine "
                "re-syncs) then refresh this window",
            )
        )

    # --- 10. daemon processes -------------------------------------------
    daemon_count = system.get("daemon_count")
    if isinstance(daemon_count, int) and daemon_count > 1:
        pids = ", ".join(
            str(proc.get("pid")) for proc in _as_list(system.get("processes"))
            if isinstance(proc, dict) and proc.get("role") == "daemon"
        )
        findings.append(
            _finding(
                "multiple_daemons", SEVERITY_WARNING,
                "%d holafd daemons are alive (%s)" % (daemon_count, pids),
                "orphans from a previous session: the UI may be talking to the wrong one. "
                "Kill the extra pids (`kill <pid>`), never the one on the socket shown above",
            )
        )
    orphans = [
        proc for proc in _as_list(system.get("processes"))
        if isinstance(proc, dict) and proc.get("role") == "daemon" and proc.get("ppid") == 1
    ]
    if orphans:
        findings.append(
            _finding(
                "orphan_daemon", SEVERITY_WARNING,
                "holafd pid(s) %s have no parent (adopted by init): their app died without "
                "stopping them" % ", ".join(str(proc.get("pid")) for proc in orphans),
                "they still own `Holaf_*` nodes; `kill <pid>` them before restarting the app",
            )
        )

    # --- 11. log tail ----------------------------------------------------
    if logs.get("available"):
        error_lines = [
            line for line in _as_list(logs.get("lines"))
            if " ERROR " in line or line.strip().startswith("ERROR")
        ]
        if error_lines:
            findings.append(
                _finding(
                    "log_errors", SEVERITY_WARNING,
                    "%d ERROR line(s) in the log tail (last one: %s)"
                    % (len(error_lines), error_lines[-1][:160]),
                    "the Journal tab has the full tail — copy the whole report for a bug thread",
                )
            )

    findings.sort(key=lambda item: (_SEVERITY_RANK.get(item["severity"], 9), item["id"]))
    return findings


def _summarize(findings: Sequence[Mapping[str, Any]]) -> Dict[str, int]:
    summary = {SEVERITY_ERROR: 0, SEVERITY_WARNING: 0, SEVERITY_INFO: 0}
    for finding in findings:
        severity = finding.get("severity")
        if severity in summary:
            summary[severity] += 1
    summary["total"] = len(findings)
    return summary


# =========================================================================
# public entry points
# =========================================================================
def collect_report(audio_engine: Any = None, options: Optional[Mapping[str, Any]] = None) -> Dict[str, Any]:
    """Collect the whole diagnostic report (see the module docstring).

    Never raises: every section is collected inside its own guard, so a broken
    engine, a missing tool or an unreadable ``/proc`` degrades to
    ``{"available": False, "reason": …}`` in that section only.
    """
    opts: Dict[str, Any] = dict(options or {})
    timeout = _positive_float(opts.get("subprocess_timeout"), DEFAULT_SUBPROCESS_TIMEOUT_S)
    env = opts.get("env")
    env = dict(env) if isinstance(env, Mapping) else None

    report: Dict[str, Any] = {
        "schema": REPORT_SCHEMA,
        "generated_at": _now_iso(),
    }

    try:
        backend = _detect_backend(audio_engine, opts)
    except Exception:  # noqa: BLE001 - defensive: never abort the report
        backend = "unknown"

    try:
        report["environment"] = _collect_environment(audio_engine, backend, opts, timeout, env)
    except Exception as exc:  # noqa: BLE001
        report["environment"] = {
            "backend": backend,
            "unavailable": {"environment": "%s: %s" % (exc.__class__.__name__, exc)},
        }

    try:
        report["engine"] = _collect_engine(audio_engine, opts, backend)
    except Exception as exc:  # noqa: BLE001
        report["engine"] = {
            "available": False,
            "reason": "engine state collection failed (%s: %s)" % (exc.__class__.__name__, exc),
            "strips": [],
            "pending": [],
            "routes": [],
            "fx_host": _normalise_fx_host(None),
            "meter_sources": {},
            "holaf_nodes": [],
        }

    try:
        report["metering"] = _collect_metering(audio_engine, report["engine"])
    except Exception as exc:  # noqa: BLE001
        report["metering"] = {
            "available": False,
            "reason": "%s: %s" % (exc.__class__.__name__, exc),
            "levels": {},
            "sources": {},
        }

    if opts.get("include_graph") is False:
        report["graph"] = {"available": False, "reason": DISABLED, "nodes": [],
                           "holaf_nodes": [], "links": [], "link_count": 0,
                           "pw_top": {"available": False, "reason": DISABLED, "rows": [], "errors": 0}}
    else:
        try:
            report["graph"] = _collect_graph(opts, timeout, report["engine"], env)
        except Exception as exc:  # noqa: BLE001
            report["graph"] = {
                "available": False,
                "reason": "%s: %s" % (exc.__class__.__name__, exc),
                "nodes": [], "holaf_nodes": [], "links": [], "link_count": 0,
                "pw_top": {"available": False, "reason": "unavailable", "rows": [], "errors": 0},
            }

    try:
        report["system"] = _collect_system(opts, report["engine"])
    except Exception as exc:  # noqa: BLE001
        report["system"] = {"available": False,
                            "reason": "%s: %s" % (exc.__class__.__name__, exc)}

    try:
        report["logs"] = _collect_logs(opts)
    except Exception as exc:  # noqa: BLE001
        report["logs"] = {"available": False, "reason": str(exc), "lines": []}

    # Findings may need the UI-side strips for the volume/config cross-check;
    # keep them in the report under a private key so detect_findings() stays a
    # pure function of the report (and format_report_text ignores it).
    report["_ui_strips"] = _as_list(opts.get("strips"))

    findings = detect_findings(report)
    report["findings"] = findings
    report["summary"] = _summarize(findings)
    return report


def merge_report(
    base: Optional[Mapping[str, Any]],
    update: Optional[Mapping[str, Any]],
    sections: Sequence[str] = ("engine", "metering"),
) -> Dict[str, Any]:
    """Overlay the *live* sections of *update* onto *base* (auto-refresh).

    The window's 2 s auto-refresh deliberately collects only the cheap, live
    parts (engine + metering: no ``pw-dump``, no ``/proc`` scan, no logs), so
    the expensive sections of the last full collection must be kept — a report
    with an empty "Graph" tab would be worse than no refresh at all.

    Findings and the summary are recomputed on the merged report, so a rule
    that needs the (kept) graph still fires.  Pure data, no Qt: usable from a
    unittest.
    """
    if not isinstance(base, Mapping):
        merged: Dict[str, Any] = dict(update) if isinstance(update, Mapping) else {}
        findings = detect_findings(merged)
        merged["findings"] = findings
        merged["summary"] = _summarize(findings)
        return merged
    merged = copy.deepcopy(dict(base))
    if isinstance(update, Mapping):
        for name in sections:
            if name in update:
                merged[name] = update[name]
        if update.get("generated_at"):
            merged["generated_at"] = update["generated_at"]
        # The strip list the UI handed us is live data too.
        if "_ui_strips" in update:
            merged["_ui_strips"] = update["_ui_strips"]
    findings = detect_findings(merged)
    merged["findings"] = findings
    merged["summary"] = _summarize(findings)
    return merged


def severity_label(severity: Optional[str]) -> str:
    """Short uppercase label for *severity* (report + summary tab), safe on junk."""
    return SEVERITY_TAGS.get(severity or "", (severity or "?").upper())


# =========================================================================
# text rendering
# =========================================================================
# Every section is rendered once, into a list of plain-text lines, and then:
#   * `format_sections()` exposes them separately (the diagnostics window
#     shows one section per tab — same text as the report, no duplicate
#     formatting code to keep in sync), and
#   * `format_report_text()` concatenates them for the copy button.
def _fmt_number(value: Any, digits: int = 2) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return ("%%.%df" % digits) % value
    if value is None:
        return "?"
    return str(value)


def _fmt_bool(value: Any) -> str:
    if value is None:
        return "?"
    return "true" if value else "false"


def _fmt_cpu(proc: Mapping[str, Any]) -> str:
    cpu = proc.get("cpu_percent_avg")
    rss = proc.get("rss_mb")
    parts = []
    if isinstance(cpu, (int, float)):
        parts.append("cpu %s%%" % _fmt_number(cpu, 1))
    if isinstance(rss, (int, float)):
        parts.append("rss %s MB" % _fmt_number(rss, 1))
    return " ".join(parts)


def _render_header(report: Mapping[str, Any]) -> List[str]:
    environment = _as_dict(report.get("environment"))
    engine_class = environment.get("engine_class")
    return [
        "=" * 78,
        "Holaf-Mix — diagnostic report",
        "generated %s" % (report.get("generated_at") or "?"),
        "app %s · backend %s%s"
        % (
            environment.get("app_version") or "?",
            environment.get("backend") or "?",
            " (%s)" % engine_class if engine_class else "",
        ),
        "=" * 78,
        "",
    ]


def _render_summary(report: Mapping[str, Any]) -> List[str]:
    findings = [item for item in _as_list(report.get("findings")) if isinstance(item, dict)]
    summary = _as_dict(report.get("summary"))
    lines = [
        "FINDINGS — %d error(s), %d warning(s), %d info"
        % (
            summary.get(SEVERITY_ERROR, 0),
            summary.get(SEVERITY_WARNING, 0),
            summary.get(SEVERITY_INFO, 0),
        )
    ]
    if not findings:
        lines.append("  (none — everything looks healthy)")
    for finding in findings:
        lines.append(
            "  [%-7s] %s — %s"
            % (
                _SEVERITY_TAG.get(finding.get("severity"), "?"),
                finding.get("id") or "?",
                finding.get("message") or "",
            )
        )
        if finding.get("hint"):
            lines.append("            → %s" % finding["hint"])
    lines.append("")
    return lines


def _render_environment(report: Mapping[str, Any]) -> List[str]:
    environment = _as_dict(report.get("environment"))
    lines = ["ENVIRONMENT"]
    lines.append("  backend        : %s" % (environment.get("backend") or "?"))
    lines.append(
        "  engine         : %s (%s)"
        % (environment.get("engine_class") or "?", environment.get("engine_module") or "?")
    )
    lines.append("  app version    : %s" % (environment.get("app_version") or "?"))
    lines.append("  config         : %s" % (environment.get("config_path") or "?"))
    lines.append("  socket         : %s" % (environment.get("socket_path") or "?"))
    lines.append(
        "  holafd binary  : %s%s"
        % (
            environment.get("engine_binary") or "?",
            " (built %s)" % environment["engine_binary_mtime"]
            if environment.get("engine_binary_mtime")
            else "",
        )
    )
    lines.append("  PipeWire       : %s" % (environment.get("pipewire_version") or "?"))
    lines.append("  OS / kernel    : %s" % (environment.get("os") or "?"))
    lines.append("  Python         : %s" % (environment.get("python") or "?"))
    lines.append(
        "  app pid        : %s%s"
        % (
            environment.get("app_pid") or "?",
            " (frozen)" if environment.get("frozen") else "",
        )
    )
    if environment.get("rust_fallback_reason"):
        lines.append("  fallback reason: %s" % environment["rust_fallback_reason"])
    for key, reason in _as_dict(environment.get("unavailable")).items():
        lines.append("  ! %s unavailable: %s" % (key, reason))
    lines.append("")
    return lines


def _render_engine(report: Mapping[str, Any]) -> List[str]:
    engine = _as_dict(report.get("engine"))
    lines = ["ENGINE (%s)" % (engine.get("source") or "?")]
    if not engine.get("available"):
        lines.append("  unavailable: %s" % (engine.get("reason") or "unknown reason"))
        lines.append("")
        return lines

    lines.append("  running        : %s" % _fmt_bool(engine.get("running")))
    lines.append("  default sink   : %s" % (engine.get("default_sink") or "?"))
    lines.append("  degraded       : %s" % _fmt_bool(engine.get("degraded")))
    fx_host = _as_dict(engine.get("fx_host"))
    lines.append(
        "  fx host        : mode=%s pid=%s respawns=%s chains=%s"
        % (
            fx_host.get("mode") or "?",
            fx_host.get("pid") if fx_host.get("pid") is not None else "?",
            fx_host.get("respawns") if fx_host.get("respawns") is not None else "?",
            len(_as_list(fx_host.get("chains"))),
        )
    )
    if engine.get("mainloop_pumps") is not None:
        lines.append("  mainloop pumps : %s" % engine.get("mainloop_pumps"))
    pending = _as_list(engine.get("pending"))
    if pending:
        lines.append("  pending:")
        for item in pending:
            lines.append("    - %s: %s" % (item.get("key"), item.get("message")))

    strips = [strip for strip in _as_list(engine.get("strips")) if isinstance(strip, dict)]
    lines.append("  strips (%d):" % len(strips))
    if not strips:
        lines.append("    (none)")
    for strip in strips:
        lines.append(
            "    - %-20s %-22s bus=%-24s vol=%s mute=%s mono=%s fx=[%s]"
            % (
                (strip.get("label") or "?")[:20],
                "%s/%s" % (strip.get("kind") or "?", strip.get("mode") or "?"),
                strip.get("node_name") or "-",
                _fmt_number(strip.get("volume"), 3),
                _fmt_bool(strip.get("mute")),
                _fmt_bool(strip.get("is_mono")),
                ",".join(_as_list(strip.get("effects"))),
            )
        )
        if strip.get("device_name"):
            lines.append("      device: %s" % strip["device_name"])

    routes = _as_list(engine.get("routes"))
    lines.append("  routes (%d):" % len(routes))
    for route in routes[:40]:
        if isinstance(route, dict):
            lines.append(
                "    - %s → %s" % (route.get("source_uid") or "?", route.get("target_uid") or "?")
            )
        else:
            lines.append("    - %s" % route)
    if len(routes) > 40:
        lines.append("    … %d more" % (len(routes) - 40))

    node_names = _as_list(engine.get("holaf_nodes"))
    if node_names:
        lines.append("  holaf nodes (daemon view): %s" % ", ".join(str(name) for name in node_names))
    lines.append("")
    return lines


def _render_metering(report: Mapping[str, Any]) -> List[str]:
    metering = _as_dict(report.get("metering"))
    lines = ["METERING"]
    if not metering.get("available"):
        lines.append("  unavailable: %s" % (metering.get("reason") or "unknown reason"))
        lines.append("")
        return lines
    levels = _as_dict(metering.get("levels"))
    lines.append("  strips metered : %d" % len(levels))
    for uid, pair in list(levels.items())[:40]:
        values = _as_list(pair)
        lines.append(
            "    - %-24s L=%s R=%s"
            % (
                uid,
                _fmt_number(values[0] if values else 0, 3),
                _fmt_number(values[1] if len(values) > 1 else 0, 3),
            )
        )
    sources = _as_dict(metering.get("sources"))
    if sources:
        lines.append("  sources:")
        for uid, name in list(sources.items())[:40]:
            lines.append("    - %-24s %s" % (uid, name or "-"))
    lines.append("")
    return lines


def _render_graph(report: Mapping[str, Any]) -> List[str]:
    graph = _as_dict(report.get("graph"))
    lines = ["PIPEWIRE GRAPH (%s)" % (graph.get("source") or "?")]
    if not graph.get("available"):
        lines.append("  unavailable: %s" % (graph.get("reason") or "unknown reason"))
        lines.append("")
        return lines

    holaf_nodes = [node for node in _as_list(graph.get("holaf_nodes")) if isinstance(node, dict)]
    lines.append("  Holaf nodes (%d):" % len(holaf_nodes))
    if not holaf_nodes:
        lines.append("    (none — the mix owns no node in the graph)")
    for node in holaf_nodes:
        lines.append(
            "    - id=%-5s %-34s %-18s %-10s quantum=%s rate=%s in/out=%s/%s"
            % (
                node.get("id"),
                (node.get("name") or "")[:34],
                node.get("media_class") or "?",
                node.get("state") or "?",
                node.get("live_quantum") or node.get("quantum") or "?",
                node.get("live_rate") or node.get("rate") or "?",
                node.get("ports_in") if node.get("ports_in") is not None else "?",
                node.get("ports_out") if node.get("ports_out") is not None else "?",
            )
        )

    device_nodes = [node for node in _as_list(graph.get("device_nodes")) if isinstance(node, dict)]
    lines.append("  Devices (%d):" % len(device_nodes))
    for node in device_nodes[:25]:
        lines.append(
            "    - id=%-5s %-34s %-18s %-10s quantum=%s"
            % (
                node.get("id"),
                (node.get("name") or "")[:34],
                node.get("media_class") or "?",
                node.get("state") or "?",
                node.get("live_quantum") or node.get("quantum") or "?",
            )
        )
    if len(device_nodes) > 25:
        lines.append("    … %d more" % (len(device_nodes) - 25))

    links = [link for link in _as_list(graph.get("links")) if isinstance(link, dict)]
    lines.append("  Links (%d shown / %d in graph):" % (len(links), graph.get("link_count") or 0))
    for link in links[:60]:
        lines.append(
            "    - %s:%s → %s:%s  [%s]"
            % (
                link.get("output") or "?",
                link.get("output_port") or "?",
                link.get("input") or "?",
                link.get("input_port") or "?",
                link.get("state") or "?",
            )
        )
    if len(links) > 60:
        lines.append("    … %d more" % (len(links) - 60))

    pw_top = _as_dict(graph.get("pw_top"))
    if pw_top.get("available"):
        lines.append(
            "  pw-top: ERR total=%s over %d node(s)"
            % (pw_top.get("errors"), len(_as_list(pw_top.get("rows"))))
        )
        for row in _as_list(pw_top.get("rows"))[:20]:
            if not isinstance(row, dict):
                continue
            lines.append(
                "    - %-1s id=%-5s quant=%-5s rate=%-6s wait=%-8s busy=%-8s err=%-4s %s"
                % (
                    row.get("state") or "?",
                    row.get("id"),
                    row.get("quantum"),
                    row.get("rate"),
                    row.get("wait"),
                    row.get("busy"),
                    row.get("err"),
                    (row.get("name") or "")[:40],
                )
            )
    else:
        lines.append("  pw-top: unavailable (%s)" % (pw_top.get("reason") or "?"))
    lines.append("")
    return lines


def _render_system(report: Mapping[str, Any]) -> List[str]:
    system = _as_dict(report.get("system"))
    lines = ["SYSTEM"]
    if not system.get("available"):
        lines.append("  unavailable: %s" % (system.get("reason") or "unknown reason"))
        lines.append("")
        return lines

    loadavg = system.get("loadavg")
    lines.append(
        "  load average   : %s (cpus: %s)"
        % (
            " ".join(_fmt_number(value, 2) for value in loadavg)
            if isinstance(loadavg, (list, tuple))
            else "?",
            system.get("cpu_count") or "?",
        )
    )
    memory = _as_dict(system.get("memory"))
    if memory:
        lines.append(
            "  memory         : used %s MB / %s MB (%s%%), available %s MB"
            % (
                memory.get("used_mb"),
                memory.get("total_mb"),
                _fmt_number(memory.get("used_percent"), 1),
                memory.get("available_mb"),
            )
        )
    app = _as_dict(system.get("app"))
    lines.append(
        "  app process    : pid %s, threads %s, rss %s MB, cpu(avg) %s%%"
        % (app.get("pid"), app.get("threads"), app.get("rss_mb"), app.get("cpu_percent_avg"))
    )
    processes = [proc for proc in _as_list(system.get("processes")) if isinstance(proc, dict)]
    lines.append(
        "  holafd         : %d daemon(s), %d fx-host(s)"
        % (system.get("daemon_count") or 0, system.get("fx_host_count") or 0)
    )
    for proc in processes:
        lines.append(
            "    - pid %-6s ppid %-6s %-8s %-28s %s"
            % (
                proc.get("pid"),
                proc.get("ppid"),
                proc.get("role") or "?",
                _fmt_cpu(proc),
                (proc.get("cmdline") or "")[:60],
            )
        )
    lines.append("")
    return lines


def _render_logs(report: Mapping[str, Any]) -> List[str]:
    logs = _as_dict(report.get("logs"))
    lines = ["JOURNAL (last %d line(s))" % len(_as_list(logs.get("lines")))]
    if not logs.get("available"):
        lines.append("  unavailable: %s" % (logs.get("reason") or "unknown reason"))
    else:
        for line in _as_list(logs.get("lines")):
            lines.append("  %s" % line)
    lines.append("")
    return lines


#: Section order of the text report.  The diagnostics window uses the same
#: order for its tabs, minus the ones it merges (ENVIRONMENT lives in the
#: "System" tab, METERING in the "Engine" tab).
SECTION_ORDER = ("summary", "environment", "engine", "metering", "graph", "system", "logs")

_RENDERERS = {
    "summary": _render_summary,
    "environment": _render_environment,
    "engine": _render_engine,
    "metering": _render_metering,
    "graph": _render_graph,
    "system": _render_system,
    "logs": _render_logs,
}


def _render_section(report: Mapping[str, Any], name: str) -> List[str]:
    renderer = _RENDERERS.get(name)
    if renderer is None:
        return []
    try:
        return renderer(report)
    except Exception as exc:  # noqa: BLE001 - never break the whole report
        return ["%s: rendering failed (%s: %s)" % (name.upper(), exc.__class__.__name__, exc), ""]


def format_sections(report: Optional[Mapping[str, Any]]) -> Dict[str, str]:
    """Render every section to text, keyed by section name.

    One section per diagnostics tab (``summary``, ``environment``, ``engine``,
    ``metering``, ``graph``, ``system``, ``logs``), so the window and the
    copyable report can never drift apart.
    """
    if not isinstance(report, Mapping):
        return {name: "" for name in SECTION_ORDER}
    return {
        name: "\n".join(_render_section(report, name)).rstrip("\n")
        for name in SECTION_ORDER
    }


def format_report_text(report: Optional[Mapping[str, Any]]) -> str:
    """Render a report dict as the compact block the user pastes.

    Layout: findings first (that is the answer), then the sections in the same
    order as the window's tabs.  No JSON: the target reader is a human (or a
    human pasting into a chat), so alignment and short lines matter more than
    machine parsing.
    """
    if not isinstance(report, Mapping):
        return "Holaf-Mix diagnostics: no report available.\n"
    lines = _render_header(report)
    for name in SECTION_ORDER:
        lines.extend(_render_section(report, name))
    lines.append("=" * 78)
    return "\n".join(lines) + "\n"
