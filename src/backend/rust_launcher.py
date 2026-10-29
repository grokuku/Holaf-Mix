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

The daemon binary is located via (in order): ``$HOLAFD_BIN``, the repo's
``rust/target/release/holafd``, ``rust/target/debug/holafd``, then the ``PATH``
(``shutil.which``).  Its stdout/stderr are appended to ``<socket>.log`` so
Rust ``env_logger`` output is easy to inspect after a crash.

Backend choice
--------------
Until the real PipeWire backend lands (J1/J2) only ``fake`` answers orders, so
the launcher defaults ``holafd --backend fake`` (in-memory engine) — enough to
validate the full IPC stack.  Override with ``$HOLAFD_BACKEND`` (fake or
unimplemented).  When the real backend ships, the default becomes the live
PipeWire backend and the fake remains for tests.
"""

import logging
import os
import shutil
import socket
import subprocess
import threading
import time
from typing import Optional

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

    ``bool(handle)`` mirrors ``ok`` so existing ``if not
    ensure_daemon_running():`` checks keep working while new callers can still
    read ``handle.spawned_by_us`` to decide who owns the teardown.
    """

    __slots__ = ("ok", "spawned_by_us")

    def __init__(self, ok: bool, spawned_by_us: bool):
        self.ok = ok
        self.spawned_by_us = spawned_by_us

    def __bool__(self) -> bool:
        return self.ok

    def __repr__(self) -> str:
        return f"DaemonHandle(ok={self.ok!r}, spawned_by_us={self.spawned_by_us!r})"

# How long a single liveness probe may block.
PROBE_TIMEOUT_S = 0.5
# Polling cadence / overall budget while waiting for the daemon to start.
STARTUP_POLL_INTERVAL_S = 0.1
STARTUP_MAX_WAIT_S = 2.0
# How long stop_daemon lets a graceful terminate() finish before kill().
STOP_GRACE_S = 3.0

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


def _find_holafd_binary() -> Optional[str]:
    """Locate the `holafd` executable or return None."""
    env_bin = os.environ.get("HOLAFD_BIN")
    if env_bin:
        if os.path.isfile(env_bin) and os.access(env_bin, os.X_OK):
            return env_bin
        logger.warning("HOLAFD_BIN=%s is not an executable file; ignoring", env_bin)

    # <repo-root>/rust/target/<profile>/holafd  (this file lives at
    # <root>/src/backend/rust_launcher.py, so root is 3 dirs up).
    root = os.path.dirname(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    )
    target = os.path.join(root, "rust", "target")
    for profile in ("release", "debug"):
        candidate = os.path.join(target, profile, "holafd")
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate

    found = shutil.which("holafd")
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
      process) it is False and the caller must NOT stop it.

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
            launched = _launch_daemon_locked(path)
        finally:
            _spawning = False
        # A successful launch is by definition one we performed.
        return DaemonHandle(ok=launched, spawned_by_us=launched)


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


def _launch_daemon_locked(path: str) -> bool:
    """Locate, spawn and wait for the daemon (caller must hold ``_launch_lock``).

    Returns True once the daemon answers on ``path``.  When the binary cannot
    be found it logs a clear message and returns False **without raising** —
    the caller (main.py's engine factory) turns that into a user-facing
    startup error, and tests rely on the False return.
    """
    global _process, _launched_socket

    binary = _find_holafd_binary()
    if not binary:
        logger.error(
            "holafd binary not found (set HOLAFD_BIN or build rust/): "
            "cannot start the Rust daemon at %s",
            path,
        )
        return False

    _remove_stale_socket(path)

    backend = os.environ.get("HOLAFD_BACKEND", "fake")
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
        return False

    _process = proc
    _launched_socket = path

    # Wait for the daemon to answer (bind + accept loop startup).
    deadline = time.monotonic() + STARTUP_MAX_WAIT_S
    while time.monotonic() < deadline:
        if _daemon_responding(path):
            logger.info("holafd is up on %s (pid %s)", path, proc.pid)
            return True
        if proc.poll() is not None:
            logger.error(
                "holafd exited early (code %s) while starting on %s — see %s",
                proc.returncode,
                path,
                log_path,
            )
            return False
        time.sleep(STARTUP_POLL_INTERVAL_S)

    logger.error("timed out waiting for holafd on %s (see %s)", path, log_path)
    return False


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
