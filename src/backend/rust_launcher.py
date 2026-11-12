"""rust_launcher — ensure/stop the Rust `holafd` daemon (J4).

Functions:
    daemon_socket_path()      Resolve the socket (env or default, like holafd).
    ensure_daemon_running()   Start `holafd` if it is not already answering on
                              the socket.  No-op when it is.  Never called at
                              import time.  Returns a :class:`DaemonHandle`
                              telling the caller whether the daemon was
                              spawned *by us* (so only the spawning process
                              should stop it on exit).
    stop_daemon()             Terminate the daemon this module launched.

Ownership
----------
``stop_daemon()`` only ever terminates a process this module spawned (tracked
in ``_process``) — a daemon that was already answering on the socket when the
app started (launched manually by the user or by a previous session) is left
alone.  ``DaemonHandle.spawned_by_us`` exposes the same ownership decision as
a return value of ``ensure_daemon_running()`` so the application can branch
its exit teardown explicitly.

The daemon binary is located via (in order): ``$HOLAFD_BIN``, the
PyInstaller bundle (frozen app: next to ``sys.executable``, then
``sys._MEIPASS`` / ``_internal``), the repo's ``rust/target/release/holafd``
then ``rust/target/debug/holafd``, then the ``PATH`` (``shutil.which``).  Its
stdout/stderr are appended to ``<socket>.log`` so Rust ``env_logger`` output is
easy to inspect after a crash.

Frozen (PyInstaller) bundles
----------------------------
Onefile bundles extract their payload — including an embedded ``holafd`` —
into ``sys._MEIPASS`` at every start.  The extracted copy may lose its
executable bit (the archive can be unpacked without mode preservation),
so a candidate found there is ``chmod +x``-ed on the fly before use; the same
helper also covers `onedir` bundles, where ``sys._MEIPASS`` is the
``_internal/`` directory.  The distributed tarball additionally places
``holafd`` right next to the ``holaf-mix`` executable, which is the first
frozen candidate (that copy is never extracted nor copied per launch).

Backend choice
--------------
Rust mode is the application default (see ``main.create_audio_engine``), so the
launcher starts the **live PipeWire backend**: ``holafd --backend pipewire``.
``$HOLAFD_BACKEND`` remains an override for debugging and tests (``fake`` for a
no-audio in-memory engine, see ``docs/rust-rewrite-status.md``).
"""

import logging
import os
import shutil
import socket
import subprocess
import sys
import threading
import time
from typing import List, Optional, Tuple

from .ipc_audio_engine import default_socket_path

logger = logging.getLogger("RustLauncher")


class DaemonHandle:
    """Result of :func:`ensure_daemon_running`.

    Attributes:
        ok: True when a `holafd` daemon answers on the socket (either one we
            just spawned or one that was already running); False when no
            binary was found or the daemon failed to come up.
        spawned_by_us: True **only** when this process actually launched the
            daemon during the call.  False when a daemon was already
            answering on the socket (launched manually by the user or by an
            earlier process) — in that case the caller must NOT stop it on
            exit.
        reason: Human-readable explanation when ``ok`` is False ("holafd
            binary not found", "holafd exited early (code 1)", "timed out
            waiting for holafd", …).  Empty on success.  This is what the app
            shows to the user when it degrades to the Python engine, so it
            must stay actionable and free of secrets.

    ``bool(handle)`` mirrors ``ok`` so existing ``if not
    ensure_daemon_running():`` checks keep working while new callers can still
    read ``handle.spawned_by_us`` to decide who owns the teardown.
    """

    __slots__ = ("ok", "spawned_by_us", "reason")

    def __init__(self, ok: bool, spawned_by_us: bool, reason: str = ""):
        self.ok = ok
        self.spawned_by_us = spawned_by_us
        self.reason = reason

    def __bool__(self) -> bool:
        return self.ok

    def __repr__(self) -> str:
        return (
            f"DaemonHandle(ok={self.ok!r}, spawned_by_us={self.spawned_by_us!r}, "
            f"reason={self.reason!r})"
        )

# How long a single liveness probe may block.
PROBE_TIMEOUT_S = 0.5
# Polling cadence / overall budget while waiting for the daemon to start.
STARTUP_POLL_INTERVAL_S = 0.1
STARTUP_MAX_WAIT_S = 2.0
# How long stop_daemon lets a graceful terminate() finish before kill().
STOP_GRACE_S = 3.0

# Backend the daemon runs by default.  Rust mode is the application default, so
# the live PipeWire backend is what users get; `fake` stays available through
# $HOLAFD_BACKEND for debugging and tests (no audio needed).
DEFAULT_BACKEND = "pipewire"

# The subprocess this module launched (None until ensure_daemon_running runs).
_process: Optional[subprocess.Popen] = None
_launched_socket: Optional[str] = None

# UI worker threads may call ensure_daemon_running concurrently from a thread
# pool.  A single lock serialises the locate+spawn+wait sequence so two callers
# can never double-spawn, and ``_spawning`` records "a launch attempt is in
# flight" for diagnostics (and lets stop_daemon wait its turn).
_launch_lock = threading.Lock()
_spawning = False


def _is_spawning() -> bool:
    """True while a launch attempt is in progress (informational)."""
    return _spawning


def daemon_socket_path() -> str:
    """Resolve the daemon socket: ``$HOLAF_SOCKET`` else ``/tmp/holaf-$USER.sock``."""
    return default_socket_path()


def _daemon_responding(path: str, timeout: float = PROBE_TIMEOUT_S) -> bool:
    """Quick liveness probe: can we open a connection to the socket?"""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.settimeout(timeout)
        sock.connect(path)
        return True
    except OSError:
        return False
    finally:
        sock.close()


def _is_executable(path: str) -> bool:
    return os.path.isfile(path) and os.access(path, os.X_OK)


def _ensure_executable(path: str) -> bool:
    """Return True when ``path`` is an executable file, chmod +x-ing it first.

    PyInstaller onefile bundles extract their payload (including an embedded
    ``holafd``) into ``sys._MEIPASS`` on every launch.  Depending on how the
    archive was produced/unpacked the extracted copy may not carry the
    executable bit, so we restore it here instead of failing to spawn the
    daemon.  Any filesystem error is swallowed (the caller then just skips
    this candidate).
    """
    if not os.path.isfile(path):
        return False
    if os.access(path, os.X_OK):
        return True
    try:
        mode = os.stat(path).st_mode
        os.chmod(path, mode | 0o111)
        logger.info("restored the executable bit on %s", path)
    except OSError as exc:
        logger.warning("cannot make %s executable: %s", path, exc)
        return False
    return os.access(path, os.X_OK)


def _frozen_candidate_dirs() -> List[str]:
    """Directories to probe for a bundled ``holafd`` in a frozen build.

    * the application directory (``sys.executable``'s folder) — where the
      distribution tarball places ``holafd``, and what PyInstaller uses for
      ``onedir``'s top level;
    * ``sys._MEIPASS`` — the onefile extraction dir, also `onedir`'s
      ``_internal/`` in PyInstaller ≥ 6;
    * ``<app dir>/_internal`` — explicit fallback for onedir layouts where
      ``sys._MEIPASS`` is unavailable (older/newer bootloaders).
    """
    dirs = []
    exe_dir = os.path.dirname(os.path.abspath(sys.executable))
    dirs.append(exe_dir)
    meipass = getattr(sys, "_MEIPASS", None)
    if meipass:
        dirs.append(os.path.abspath(meipass))
    dirs.append(os.path.join(exe_dir, "_internal"))
    # De-duplicate while keeping order.
    seen = set()
    ordered = []
    for d in dirs:
        if d not in seen:
            seen.add(d)
            ordered.append(d)
    return ordered


def _find_holafd_binary() -> Optional[str]:
    """Locate the `holafd` executable or return None.

    Search order: ``$HOLAFD_BIN`` → frozen bundle (app dir, ``sys._MEIPASS``,
    ``_internal/``) → ``rust/target/{release,debug}`` (dev checkout) →
    ``$PATH``.  The resolved path is logged together with the rule that matched,
    so a startup failure is diagnosable from the log alone.
    """
    env_bin = os.environ.get("HOLAFD_BIN")
    if env_bin:
        # Strict: an explicit path must already be executable.  We never
        # chmod a file the user named — a typo would otherwise turn an
        # arbitrary file into an executed "daemon".
        if _is_executable(env_bin):
            logger.info("holafd resolved from $HOLAFD_BIN: %s", env_bin)
            return env_bin
        logger.warning(
            "HOLAFD_BIN=%s is not an executable file; ignoring", env_bin
        )

    # Frozen (PyInstaller) bundle: holafd ships *inside* the app directory or
    # is extracted into _MEIPASS.  Checked before the dev tree on purpose — a
    # packaged app must use its own pinned daemon, never a leftover build.
    if getattr(sys, "frozen", False):
        for directory in _frozen_candidate_dirs():
            candidate = os.path.join(directory, "holafd")
            if _ensure_executable(candidate):
                logger.info(
                    "holafd resolved from the PyInstaller bundle: %s", candidate
                )
                return candidate
        logger.warning(
            "frozen app: no bundled holafd next to %s nor in %s — "
            "falling back to the repo/PATH lookup",
            os.path.abspath(sys.executable),
            getattr(sys, "_MEIPASS", "<no _MEIPASS>"),
        )

    # <repo-root>/rust/target/<profile>/holafd  (this file lives at
    # <root>/src/backend/rust_launcher.py, so root is 3 dirs up).
    root = os.path.dirname(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    )
    target = os.path.join(root, "rust", "target")
    for profile in ("release", "debug"):
        candidate = os.path.join(target, profile, "holafd")
        if _is_executable(candidate):
            logger.info(
                "holafd resolved from the dev tree (target/%s): %s",
                profile,
                candidate,
            )
            return candidate

    found = shutil.which("holafd")
    if found:
        logger.info("holafd resolved from $PATH: %s", found)
    return found


def _remove_stale_socket(path: str):
    """Remove a leftover socket file only when nothing is listening on it."""
    if not os.path.exists(path):
        return
    if _daemon_responding(path, timeout=0.2):
        return  # Live daemon — leave it alone.
    try:
        os.unlink(path)
        logger.info("removed stale socket file %s", path)
    except OSError as exc:
        logger.warning("could not remove stale socket %s: %s", path, exc)


def _daemon_launched_alive() -> bool:
    """Is the daemon *this module* launched still running?

    Uses ``proc.poll()`` (equivalent to ``kill(pid, 0)``) so a process that
    died — including a zombie that never got reaped — is detected without
    touching the socket.  When the process is gone ``_process`` is cleared so
    a later :func:`ensure_daemon_running` spawns a fresh daemon instead of
    trusting a stale pid.  Returns False when there is nothing alive to
    account for.
    """
    global _process
    proc = _process
    if proc is None:
        return False
    if proc.poll() is None:
        return True
    logger.warning(
        "holafd pid %s is no longer alive (exit code %s); "
        "a new daemon will be spawned on the next ensure_daemon_running()",
        proc.pid,
        proc.returncode,
    )
    _process = None
    return False


def ensure_daemon_running(socket_path: Optional[str] = None) -> DaemonHandle:
    """Ensure a `holafd` daemon is answering on the socket.

    If a daemon is already responding this is a cheap no-op.  Otherwise it
    locates the binary, cleans any stale socket file, spawns the daemon and
    polls until it answers (≈100ms steps, ≤2s).  Never called at import
    time.

    Returns a :class:`DaemonHandle`:

    * ``ok`` is True when the daemon answers, False when no binary was found
      or it failed to come up;
    * ``spawned_by_us`` is True **only** when this call actually spawned the
      daemon — the caller then owns its teardown and should call
      :func:`stop_daemon` when the process exits.  When a daemon was already
      running before this call (started manually by the user or by another
      process) it is False and the caller must NOT stop it;
    * ``reason`` explains a failure (empty on success) — see
      :class:`DaemonHandle`.

    Concurrency: the whole locate+spawn+wait sequence runs under a module
    lock, so concurrent callers from the UI thread pool never double-spawn.
    A caller that waits on the lock re-checks the socket under the lock and
    returns once the first caller's daemon answers (``spawned_by_us`` True, the
    spawn was still done by this module).  If the previously launched daemon
    died in the meantime (poll != None) a fresh one is spawned.
    """
    global _spawning

    path = socket_path or daemon_socket_path()
    # Fast path outside the lock: an already-answering daemon is the common
    # case (UI calls this on every engine creation).
    if _daemon_responding(path):
        return DaemonHandle(ok=True, spawned_by_us=_owns_daemon_on(path))

    with _launch_lock:
        # Re-check under the lock: while we waited, another thread may have
        # finished spawning a daemon that now answers — no need to spawn a
        # second one.
        if _daemon_responding(path):
            return DaemonHandle(ok=True, spawned_by_us=_owns_daemon_on(path))
        # Drop any dead/defunct daemon we launched earlier (kill(pid,0) via
        # poll) so we never "reuse" a zombie and spawn cleanly.
        _daemon_launched_alive()
        _spawning = True
        try:
            launched, reason = _launch_daemon_locked(path)
        finally:
            _spawning = False
        # A successful launch is by definition one we performed.
        return DaemonHandle(
            ok=launched, spawned_by_us=launched, reason=reason
        )


def _owns_daemon_on(path: str) -> bool:
    """True when the daemon answering on ``path`` was launched by this module.

    A daemon detected on the socket counts as "ours" only when we still hold a
    live subprocess handle for it and recorded the same socket path at launch
    time.  A daemon started manually by the user (or by another process) is
    never owned here, so the app must not stop it on exit.
    """
    if not _daemon_launched_alive():
        return False
    return _launched_socket == path


def _launch_daemon_locked(path: str) -> Tuple[bool, str]:
    """Locate, spawn and wait for the daemon (caller must hold ``_launch_lock``).

    Returns ``(ok, reason)``: ``ok`` is True once the daemon answers on
    ``path``; on failure ``reason`` is a short, user-facing explanation
    (surfaced by ``main.py`` when it falls back to the Python engine).  Never
    raises on a missing binary / failed spawn — the caller and the tests rely
    on that.
    """
    global _process, _launched_socket

    binary = _find_holafd_binary()
    if not binary:
        reason = (
            "holafd binary not found (build it with ./scripts/build_rust.sh, "
            "or set HOLAFD_BIN)"
        )
        logger.error("%s: cannot start the Rust daemon at %s", reason, path)
        return False, reason

    _remove_stale_socket(path)

    backend = os.environ.get("HOLAFD_BACKEND", DEFAULT_BACKEND)
    log_path = f"{path}.log"
    logger.info("launching %s --socket %s --backend %s (log: %s)", binary, path, backend, log_path)
    try:
        log_file = open(log_path, "ab", buffering=0)
        proc = subprocess.Popen(
            [binary, "--socket", path, "--backend", backend],
            stdout=log_file,
            stderr=subprocess.STDOUT,
        )
        log_file.close()  # Child keeps its inherited fd.
    except OSError as exc:
        logger.error("failed to launch holafd: %s", exc)
        return False, f"failed to launch holafd: {exc}"

    _process = proc
    _launched_socket = path

    # Wait for the daemon to answer (bind + accept loop startup).
    deadline = time.monotonic() + STARTUP_MAX_WAIT_S
    while time.monotonic() < deadline:
        if _daemon_responding(path):
            logger.info("holafd is up on %s (pid %s)", path, proc.pid)
            return True, ""
        if proc.poll() is not None:
            logger.error(
                "holafd exited early (code %s) while starting on %s — see %s",
                proc.returncode,
                path,
                log_path,
            )
            return (
                False,
                f"holafd exited during startup (code {proc.returncode}); "
                f"see {log_path}",
            )
        time.sleep(STARTUP_POLL_INTERVAL_S)

    logger.error("timed out waiting for holafd on %s (see %s)", path, log_path)
    return (
        False,
        f"timed out waiting for holafd on {path}; see {log_path}",
    )


def stop_daemon() -> bool:
    """Terminate the daemon launched by :func:`ensure_daemon_running`.

    Sequence: ``terminate()``, then ``wait(STOP_GRACE_S=3s)``, then ``kill()``
    if the graceful stop timed out, then a short ``wait`` to reap the child
    so no zombie is left behind.  Finally the socket file we created is
    removed (only when nothing is listening on it anymore).  Returns True when
    this module owned and stopped a process, False when there was nothing to
    stop (a daemon started elsewhere is left alone).  Runs under
    ``_launch_lock`` so it can never race a concurrent spawn.
    """
    global _process, _launched_socket
    with _launch_lock:
        proc = _process
        if proc is None or proc.poll() is not None:
            _process = None
            return False
        owned_socket = _launched_socket
        logger.info("stopping holafd (pid %s, socket %s)", proc.pid, owned_socket)
        proc.terminate()
        try:
            proc.wait(timeout=STOP_GRACE_S)
        except subprocess.TimeoutExpired:
            logger.warning(
                "holafd pid %s did not exit within %.0fs; sending SIGKILL",
                proc.pid,
                STOP_GRACE_S,
            )
            proc.kill()
            try:
                proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                logger.error("holafd pid %s would not die after SIGKILL", proc.pid)
        _process = None
        # The daemon is gone; drop the socket file we created — but only if
        # nothing else started answering on it meanwhile (a respawned daemon
        # keeps the same path).
        if owned_socket and os.path.exists(owned_socket) and not _daemon_responding(owned_socket, timeout=0.2):
            try:
                os.unlink(owned_socket)
                logger.info("removed socket file %s", owned_socket)
            except OSError as exc:
                logger.warning("could not remove socket %s: %s", owned_socket, exc)
        return True
