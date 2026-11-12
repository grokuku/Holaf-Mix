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
    kill_daemon_now()         Lock-free, last-resort stop (signal handlers and
                              the shutdown watchdog).

Ownership
----------
``stop_daemon()`` only ever terminates a process this module spawned (tracked
in ``_process``) — a daemon that was already answering on the socket when the
app started (launched manually by the user or by a previous session) is left
alone.  ``DaemonHandle.spawned_by_us`` exposes the same ownership decision as
a return value of ``ensure_daemon_running()`` so the application can branch
its exit teardown explicitly.

No orphaned daemon (three nested safety nets)
---------------------------------------------
1. ``QApplication.aboutToQuit`` → ``main.shutdown_all()`` (clean window close);
2. POSIX signal handlers (``src.backend.signal_shutdown``) → SIGTERM/SIGINT/
   SIGHUP tear the daemon down gracefully;
3. **PR_SET_PDEATHSIG** — the daemon is spawned with ``prctl(PR_SET_PDEATHSIG,
   SIGTERM)``, so the *kernel* terminates it when the application dies, even
   on SIGKILL, where no Python handler can ever run.  See
   :func:`_make_pdeathsig_preexec` for the details and the caveats.  A daemon
   we did **not** spawn never gets that treatment (see ``_spawn_daemon_process``
   call site in :func:`_launch_daemon_locked`): it is only armed at spawn time.

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
import queue
import shutil
import signal
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
# How long the caller waits for the spawner thread to hand back a Popen.
SPAWN_TIMEOUT_S = 30.0

# prctl(2) option: deliver SIGTERM to the child when the parent dies.
PR_SET_PDEATHSIG = 1

# Escape hatch for debugging (e.g. attaching a daemon to a throwaway parent):
# HOLAF_NO_PDEATHSIG=1 spawns holafd without the kernel-side belt.
ENV_NO_PDEATHSIG = "HOLAF_NO_PDEATHSIG"

# Backend the daemon runs by default.  Rust mode is the application default, so
# the live PipeWire backend is what users get; `fake` stays available through
# $HOLAFD_BACKEND for debugging and tests (no audio needed).
DEFAULT_BACKEND = "pipewire"

# The subprocess this module launched (None until ensure_daemon_running runs).
_process: Optional[subprocess.Popen] = None
_launched_socket: Optional[str] = None
# Socket of the daemon a launch attempt is currently spawning.  Ownership is
# claimed here *before* the fork, so a concurrent caller that sees the socket
# answer while ``_process`` is not assigned yet still reports
# ``spawned_by_us=True`` (the daemon is ours, the handle just is not published).
_launching_socket: Optional[str] = None

# UI worker threads may call ensure_daemon_running concurrently from a thread
# pool.  A single *reentrant* lock serialises the locate+spawn+wait sequence so
# two callers can never double-spawn, and ``_spawning`` records "a launch
# attempt is in flight" for diagnostics (and lets stop_daemon wait its turn).
# Reentrant on purpose: a SIGTERM handler runs *inside* the main thread, so a
# signal arriving while that same thread is inside the launcher would deadlock
# on a plain Lock — with an RLock the teardown simply proceeds.
_launch_lock = threading.RLock()
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
    time — or when one of our launches is in flight on that very path (the
    daemon can answer before the spawning thread published ``_process``).
    A daemon started manually by the user (or by another process) is never
    owned here, so the app must not stop it on exit.
    """
    if _daemon_launched_alive():
        return _launched_socket == path
    return _launching_socket == path and _spawning


# ---------------------------------------------------------------------------
# Parent-death signal (PR_SET_PDEATHSIG) — the kernel-side "no orphan" belt
# ---------------------------------------------------------------------------

# prctl(2) resolved once, *before* any fork (ctypes must not be imported — nor
# dlopen'ed — in the forked child, where only the async-signal-safe subset of
# libc is guaranteed).
_prctl = None
_prctl_loaded = False
_prctl_lock = threading.Lock()


def _pdeathsig_enabled() -> bool:
    """Is the kernel-side belt wanted on this machine?"""
    if os.environ.get(ENV_NO_PDEATHSIG, "").strip().lower() in ("1", "true", "yes", "on"):
        return False
    return sys.platform.startswith("linux")


def _load_prctl():
    """Resolve ``libc.prctl`` once (None when unavailable).

    Called from the *parent* before ``fork()`` so the child only has to invoke
    an already-resolved C function pointer.  Never raises: a missing libc or a
    non-Linux platform simply means "no PDEATHSIG" (the signal handlers of
    ``src.backend.signal_shutdown`` still cover every signal-driven exit).
    """
    global _prctl, _prctl_loaded
    with _prctl_lock:
        if _prctl_loaded:
            return _prctl
        _prctl_loaded = True
        try:
            import ctypes

            libc = ctypes.CDLL("libc.so.6", use_errno=True)
            function = libc.prctl
            function.restype = ctypes.c_int
            function.argtypes = (
                ctypes.c_int,
                ctypes.c_ulong,
                ctypes.c_ulong,
                ctypes.c_ulong,
                ctypes.c_ulong,
            )
            _prctl = function
        except Exception as exc:  # noqa: BLE001 - optional belt
            logger.warning(
                "cannot resolve libc.prctl (%s): the daemon will not get "
                "PR_SET_PDEATHSIG",
                exc,
            )
            _prctl = None
        return _prctl


def _apply_parent_death_signal(
    expected_parent_pid: Optional[int] = None, prctl=None, log: bool = True
) -> bool:
    """Child-side half of the PDEATHSIG belt (runs after ``fork``, before ``exec``).

    Arms ``PR_SET_PDEATHSIG`` with SIGTERM so the kernel signals this process
    as soon as its parent dies — the only mechanism that survives a SIGKILL of
    the application (no Python, no Qt, no handler can run there).

    Returns True when the caller may keep running, False when the parent died
    between ``fork`` and this call: PDEATHSIG only fires on *future* parent
    death, so a child forked by an already-dying parent would survive forever.
    The caller must then exit immediately (see :func:`_make_pdeathsig_preexec`).

    ``prctl`` is injectable so the behaviour is unit-testable without forking or
    touching the real libc; ``expected_parent_pid`` defaults to this process'
    parent.  ``log=False`` silences the diagnostics: the real child calls it
    from ``preexec_fn``, where taking the logging lock could deadlock against a
    thread that held it at fork time.
    """
    function = prctl if prctl is not None else _load_prctl()
    if function is None:
        return True  # No PDEATHSIG available: the signal handlers remain.
    result = function(PR_SET_PDEATHSIG, int(signal.SIGTERM), 0, 0, 0)
    if result != 0 and log:
        errno = None
        try:
            import ctypes

            errno = ctypes.get_errno()
        except Exception:  # noqa: BLE001 - diagnostics only
            pass
        logger.warning(
            "prctl(PR_SET_PDEATHSIG, SIGTERM) failed (rc=%s%s); the daemon "
            "will not be killed by the kernel on parent death",
            result,
            f", errno={errno}" if errno is not None else "",
        )
    parent_pid = os.getppid() if expected_parent_pid is None else expected_parent_pid
    if os.getppid() != parent_pid:
        if log:
            logger.warning(
                "parent (pid %s) died before PR_SET_PDEATHSIG could be armed "
                "(now reparented to %s); exiting instead of running orphaned",
                parent_pid,
                os.getppid(),
            )
        return False
    return True


def _make_pdeathsig_preexec(expected_parent_pid: Optional[int] = None):
    """Build the ``preexec_fn`` given to :class:`subprocess.Popen` for holafd.

    Returns None when the belt does not apply (non-Linux, no libc.prctl,
    ``$HOLAF_NO_PDEATHSIG``), so the caller spawns exactly as before.

    Caveats (documented because they are why the code looks like this):

    * ``preexec_fn`` runs between ``fork`` and ``exec`` in a process that still
      shares the parent's memory.  It must stay *minimal*: here it only calls
      an already-resolved C function plus ``getppid``/``_exit`` — no import, no
      allocation, no lock acquisition (not even the logging lock, hence
      ``log=False``), hence no risk of inheriting a lock held by another
      thread (see the next point).
    * PDEATHSIG is bound to the *thread* that forked, not to the process: if
      the daemon were spawned by a short-lived UI worker thread, the daemon
      would be killed as soon as that thread exits, while the app lives on.
      ``_spawn_daemon_process`` therefore always forks from one permanent
      spawner thread that dies only with the process.
    * The daemon is deliberately **not** started in a new session
      (``start_new_session`` is left False): it stays in the application's
      process group, so terminal/session signals (Ctrl-C, SIGHUP on terminal
      close) reach it too and it shuts down cleanly by itself.  PDEATHSIG then
      covers the SIGKILL case, where no signal is delivered to the group.
    * Only applied at spawn time: a daemon that was already running before we
      started (``spawned_by_us=False``) never goes through this path, so the
      kernel can never kill a daemon the user launched manually.
    """
    if not _pdeathsig_enabled():
        logger.debug(
            "PR_SET_PDEATHSIG disabled (%s=%s); relying on the signal handlers",
            ENV_NO_PDEATHSIG,
            os.environ.get(ENV_NO_PDEATHSIG, ""),
        )
        return None
    function = _load_prctl()
    if function is None:
        return None
    parent_pid = os.getpid() if expected_parent_pid is None else expected_parent_pid

    def _preexec_pdeathsig():
        """Runs in the forked child, before ``execv`` (keep it tiny).

        No logging on purpose: another thread may have held the logging lock at
        fork time, and taking it here would hang the child forever.
        """
        if not _apply_parent_death_signal(parent_pid, prctl=function, log=False):
            os._exit(1)

    return _preexec_pdeathsig


class _SpawnRequest:
    """One ``Popen`` request handed to the permanent spawner thread."""

    __slots__ = ("argv", "log_path", "proc", "error", "done")

    def __init__(self, argv: List[str], log_path: str):
        self.argv = argv
        self.log_path = log_path
        self.proc: Optional[subprocess.Popen] = None
        self.error: Optional[BaseException] = None
        self.done = threading.Event()


_spawner_lock = threading.Lock()
_spawner_queue: Optional["queue.Queue[_SpawnRequest]"] = None
_spawner_thread: Optional[threading.Thread] = None


def _spawner_loop(requests: "queue.Queue[_SpawnRequest]") -> None:
    """Permanent thread that performs every daemon ``fork`` (see below)."""
    while True:
        request = requests.get()
        if request is None:
            return
        try:
            log_file = open(request.log_path, "ab", buffering=0)
            try:
                request.proc = subprocess.Popen(
                    request.argv,
                    stdout=log_file,
                    stderr=subprocess.STDOUT,
                    preexec_fn=_make_pdeathsig_preexec(),
                )
            finally:
                log_file.close()  # Child keeps its inherited fd.
        except BaseException as exc:  # noqa: BLE001 - reported to the caller
            request.error = exc
        finally:
            request.done.set()


def _ensure_spawner() -> "queue.Queue[_SpawnRequest]":
    """Return the (single, permanent) spawner queue, starting the thread lazily."""
    global _spawner_queue, _spawner_thread
    with _spawner_lock:
        if (
            _spawner_queue is None
            or _spawner_thread is None
            or not _spawner_thread.is_alive()
        ):
            _spawner_queue = queue.Queue()
            _spawner_thread = threading.Thread(
                target=_spawner_loop,
                args=(_spawner_queue,),
                name="holafd-spawner",
                daemon=True,
            )
            _spawner_thread.start()
        return _spawner_queue


def _spawn_daemon_process(argv: List[str], log_path: str) -> subprocess.Popen:
    """``Popen`` holafd from the permanent spawner thread and return it.

    Why a dedicated thread: ``PR_SET_PDEATHSIG`` delivers its signal when the
    *forking thread* terminates.  ``ensure_daemon_running`` may be called from
    a transient UI worker thread — forking from there would make the kernel
    kill the daemon mid-session.  The spawner thread lives as long as the
    process, so "parent thread died" becomes exactly "the application died".

    Raises the underlying ``OSError`` on a failed spawn; the caller turns it
    into a user-facing ``reason``.
    """
    requests = _ensure_spawner()
    request = _SpawnRequest(list(argv), log_path)
    requests.put(request)
    if not request.done.wait(SPAWN_TIMEOUT_S):
        raise OSError(
            f"the holafd spawner thread did not answer within {SPAWN_TIMEOUT_S:.0f}s"
        )
    if request.error is not None:
        if isinstance(request.error, OSError):
            raise request.error
        raise OSError(str(request.error)) from request.error
    if request.proc is None:
        raise OSError("the holafd spawner thread returned no process")
    return request.proc


def _launch_daemon_locked(path: str) -> Tuple[bool, str]:
    """Locate, spawn and wait for the daemon (caller must hold ``_launch_lock``).

    Returns ``(ok, reason)``: ``ok`` is True once the daemon answers on
    ``path``; on failure ``reason`` is a short, user-facing explanation
    (surfaced by ``main.py`` when it falls back to the Python engine).  Never
    raises on a missing binary / failed spawn — the caller and the tests rely
    on that.
    """
    global _process, _launched_socket, _launching_socket

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
    # Claim ownership before forking (see _owns_daemon_on).
    _launching_socket = path
    try:
        proc = _spawn_daemon_process(
            [binary, "--socket", path, "--backend", backend], log_path
        )
    except OSError as exc:
        logger.error("failed to launch holafd: %s", exc)
        return False, f"failed to launch holafd: {exc}"
    finally:
        _launching_socket = None

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


def kill_daemon_now(sig: int = signal.SIGKILL) -> bool:
    """Last-resort, **lock-free** stop of the daemon this module spawned.

    Used from the two contexts where :func:`stop_daemon` could block forever:

    * a POSIX signal handler that fired *inside* :func:`stop_daemon` (or inside
      :func:`ensure_daemon_running`) on the same thread — waiting on
      ``_launch_lock`` there would hang the teardown and orphan the daemon;
    * the shutdown watchdog thread, which runs precisely because the graceful
      teardown never finished.

    It therefore touches nothing but module globals and ``os.kill``: no lock,
    no socket probe, no ``wait()``.  Only the process **we** launched is ever
    signalled (``_process`` is None when the daemon was already running), so
    ownership is preserved.  Returns True when a signal was sent.

    ``SIGKILL`` is the default because this path must be unconditional; the
    daemon's PipeWire nodes are reclaimed by the PipeWire server when its
    client connection dies, so nothing is left behind.  A stale ``.sock`` file
    is unlinked here and would also be cleaned by ``_remove_stale_socket`` on
    the next launch.
    """
    global _process, _launched_socket
    proc = _process
    owned_socket = _launched_socket
    _process = None
    _launched_socket = None
    killed = False
    if proc is not None:
        try:
            if proc.poll() is None:
                logger.warning(
                    "force-killing holafd (pid %s, signal %s)", proc.pid, sig
                )
                proc.send_signal(sig)
                killed = True
        except OSError as exc:
            logger.warning("could not signal holafd: %s", exc)
    if owned_socket:
        try:
            os.unlink(owned_socket)
            logger.info("removed socket file %s", owned_socket)
        except OSError:
            pass  # Already gone / never created: nothing to clean.
    return killed
