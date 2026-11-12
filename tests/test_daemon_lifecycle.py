"""Tests for the daemon lifecycle guarantee: **no orphaned `holafd`, ever**.

The bug this covers: the app's teardown was connected to
``QApplication.aboutToQuit`` only, so any *external* death of the process
(SIGTERM on session logout, ``kill``, SIGHUP from a closed terminal, SIGINT)
killed Python without ever stopping the Rust daemon it had spawned.

Three nested nets are verified here:

* **kernel belt** — ``PR_SET_PDEATHSIG`` armed in the child right after
  ``fork`` (:class:`PdeathsigUnitTest`), plus the real end-to-end proof that a
  ``SIGKILL``ed app leaves no daemon and no PipeWire node behind
  (:class:`LiveTeardownTest`);
* **signal handlers** — SIGTERM/SIGINT/SIGHUP run the same idempotent teardown
  as a clean Qt quit (:class:`ShutdownControllerUnitTest`,
  :class:`AppTeardownUnitTest`, and the live SIGTERM/SIGHUP/SIGINT test);
* **forced exit** — the watchdog thread and the wakeup-fd reader
  (:class:`ShutdownControllerUnitTest`), which cover a teardown that hangs or a
  main thread stuck inside C code.

The ``Live*`` tests need a real `holafd` binary **and** a reachable PipeWire;
they are skipped otherwise.  To run them:

    eval "$(./scripts/start-pipewire-headless.sh --export)"
    python3 -m unittest tests.test_daemon_lifecycle -v

    python3 -m unittest discover -s tests -v
"""

import json
import logging
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from unittest import mock

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if REPO_ROOT not in sys.path:
    sys.path.insert(0, REPO_ROOT)

import main  # noqa: E402  (importable without PySide6: Qt is imported lazily)
import src.backend.rust_launcher as launcher  # noqa: E402
import src.backend.signal_shutdown as signal_shutdown  # noqa: E402
from src.backend.signal_shutdown import ShutdownController  # noqa: E402

logging.disable(logging.CRITICAL)  # Keep unittest output clean.


class _FakeProc:
    """Minimal ``subprocess.Popen`` stand-in (no fork, no signal)."""

    def __init__(self, pid=4242, alive=True):
        self.pid = pid
        self.returncode = None if alive else 0
        self.signals = []
        self.terminated = 0
        self.killed = 0

    def poll(self):
        return self.returncode

    def send_signal(self, sig):
        self.signals.append(sig)

    def terminate(self):
        self.terminated += 1
        self.returncode = -15

    def kill(self):
        self.killed += 1
        self.signals.append(signal.SIGKILL)
        self.returncode = -9

    def wait(self, timeout=None):
        return self.returncode


def _restore_launcher_state(**state):
    """Put the launcher's module globals back (they are process-wide)."""
    launcher._process = state.get("process")
    launcher._launched_socket = state.get("socket")
    launcher._spawning = False


# ---------------------------------------------------------------------------
# VERROU 1 — PR_SET_PDEATHSIG
# ---------------------------------------------------------------------------


class PdeathsigUnitTest(unittest.TestCase):
    """The kernel belt: what is armed, with which signal, and its race guard."""

    def test_arms_pdeathsig_with_sigterm(self):
        calls = []

        def fake_prctl(*args):
            calls.append(args)
            return 0

        alive = launcher._apply_parent_death_signal(
            os.getppid(), prctl=fake_prctl
        )
        self.assertTrue(alive)
        self.assertEqual(len(calls), 1)
        self.assertEqual(
            calls[0],
            (
                launcher.PR_SET_PDEATHSIG,  # 1 == PR_SET_PDEATHSIG
                int(signal.SIGTERM),
                0,
                0,
                0,
            ),
        )
        self.assertEqual(launcher.PR_SET_PDEATHSIG, 1)

    def test_prctl_failure_is_logged_but_not_fatal(self):
        logging.disable(logging.NOTSET)
        try:
            with self.assertLogs("RustLauncher", level="WARNING") as logs:
                alive = launcher._apply_parent_death_signal(
                    os.getppid(), prctl=lambda *a: -1
                )
        finally:
            logging.disable(logging.CRITICAL)
        self.assertTrue(alive)  # Parent still alive: the daemon may run.
        self.assertTrue(any("PDEATHSIG" in line for line in logs.output))

    def test_parent_died_between_fork_and_prctl(self):
        # The parent-death signal only fires on *future* parent death, so a
        # child forked by an already-dying parent must give up (see the race
        # documented on _make_pdeathsig_preexec).
        alive = launcher._apply_parent_death_signal(999999, prctl=lambda *a: 0)
        self.assertFalse(alive)

    def test_no_prctl_available_is_a_no_op(self):
        # Non-Linux / libc without prctl: the signal handlers remain the net.
        original = launcher._prctl, launcher._prctl_loaded
        try:
            launcher._prctl = None
            launcher._prctl_loaded = True
            self.assertIsNone(launcher._load_prctl())
            self.assertTrue(
                launcher._apply_parent_death_signal(os.getppid(), prctl=None)
            )
        finally:
            launcher._prctl, launcher._prctl_loaded = original

    def test_preexec_exits_when_the_parent_is_gone(self):
        original = launcher._load_prctl
        launcher._load_prctl = lambda: (lambda *a: 0)
        try:
            preexec = launcher._make_pdeathsig_preexec(expected_parent_pid=999999)
            self.assertIsNotNone(preexec)
            with mock.patch("os._exit") as fake_exit:
                preexec()
            fake_exit.assert_called_once_with(1)
        finally:
            launcher._load_prctl = original

    def test_preexec_is_a_no_op_when_the_parent_lives(self):
        original = launcher._load_prctl
        launcher._load_prctl = lambda: (lambda *a: 0)
        try:
            preexec = launcher._make_pdeathsig_preexec(
                expected_parent_pid=os.getppid()
            )
            with mock.patch("os._exit") as fake_exit:
                preexec()
            fake_exit.assert_not_called()
        finally:
            launcher._load_prctl = original

    def test_preexec_carries_the_application_pid(self):
        """The child compares getppid() with the pid captured *before* fork."""
        original_apply = launcher._apply_parent_death_signal
        original_load = launcher._load_prctl
        seen = []
        launcher._load_prctl = lambda: (lambda *a: 0)
        launcher._apply_parent_death_signal = (
            lambda parent_pid=None, prctl=None, log=True: seen.append(parent_pid) or True
        )
        try:
            launcher._make_pdeathsig_preexec()()
        finally:
            launcher._apply_parent_death_signal = original_apply
            launcher._load_prctl = original_load
        self.assertEqual(seen, [os.getpid()])

    def test_preexec_disabled_by_env(self):
        os.environ[launcher.ENV_NO_PDEATHSIG] = "1"
        try:
            self.assertIsNone(launcher._make_pdeathsig_preexec())
        finally:
            os.environ.pop(launcher.ENV_NO_PDEATHSIG, None)

    def test_popen_receives_preexec_and_keeps_the_session(self):
        """The daemon must keep the app's session/process group on purpose."""
        captured = {}

        def fake_popen(argv, **kwargs):
            captured["argv"] = argv
            captured.update(kwargs)
            return _FakeProc()

        original = launcher.subprocess.Popen
        launcher.subprocess.Popen = fake_popen
        try:
            with tempfile.TemporaryDirectory() as tmp:
                log_path = os.path.join(tmp, "daemon.log")
                launcher._spawn_daemon_process(["/bin/true"], log_path)
                self.assertTrue(os.path.exists(log_path))
        finally:
            launcher.subprocess.Popen = original

        self.assertIn("preexec_fn", captured)
        self.assertTrue(callable(captured["preexec_fn"]))
        # Not a new session: terminal/session signals must still reach holafd.
        self.assertFalse(captured.get("start_new_session", False))

    def test_daemon_is_forked_from_a_permanent_thread(self):
        """PDEATHSIG is bound to the *forking thread*, hence the spawner thread."""
        first = launcher._ensure_spawner()
        second = launcher._ensure_spawner()
        self.assertIs(first, second)  # One thread for the whole process…
        thread = launcher._spawner_thread
        self.assertIsNotNone(thread)
        self.assertTrue(thread.is_alive())
        self.assertTrue(thread.daemon)  # …which dies only with the process.
        self.assertNotEqual(thread, threading.current_thread())


class KillDaemonNowUnitTest(unittest.TestCase):
    """`kill_daemon_now()` is the lock-free last resort used by the watchdog."""

    def setUp(self):
        self._saved = (launcher._process, launcher._launched_socket)
        _restore_launcher_state(process=None, socket=None)
        self._tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        launcher._process, launcher._launched_socket = self._saved
        self._tmp.cleanup()

    def test_kills_the_daemon_we_spawned(self):
        proc = _FakeProc()
        sock = os.path.join(self._tmp.name, "d.sock")
        open(sock, "w").close()
        _restore_launcher_state(process=proc, socket=sock)
        self.assertTrue(launcher.kill_daemon_now())
        self.assertEqual(proc.signals, [signal.SIGKILL])
        self.assertIsNone(launcher._process)
        self.assertFalse(os.path.exists(sock))  # Stale socket cleaned up.

    def test_never_touches_a_daemon_we_did_not_spawn(self):
        # A daemon started manually by the user: no Popen handle -> nothing to
        # kill, and the function must not invent a pid.
        self.assertFalse(launcher.kill_daemon_now())
        self.assertIsNone(launcher._process)

    def test_already_dead_daemon_is_not_signalled(self):
        proc = _FakeProc(alive=False)
        _restore_launcher_state(process=proc, socket=None)
        self.assertFalse(launcher.kill_daemon_now())
        self.assertEqual(proc.signals, [])
        self.assertIsNone(launcher._process)


# ---------------------------------------------------------------------------
# VERROU 2 — signal handlers
# ---------------------------------------------------------------------------


class AppTeardownUnitTest(unittest.TestCase):
    """`main.make_shutdown_all()`: ownership + idempotency of the teardown."""

    class _Engine:
        def __init__(self, spawned_by_us):
            self._rust_daemon_spawned_by_us = spawned_by_us
            self.shutdowns = 0

        def shutdown(self):
            self.shutdowns += 1

    class _Midi:
        def __init__(self):
            self.closes = 0

        def close_port(self):
            self.closes += 1

    def _run(self, spawned_by_us, times=1):
        engine = self._Engine(spawned_by_us)
        midi = self._Midi()
        stop_calls = []
        original = launcher.stop_daemon
        launcher.stop_daemon = lambda: stop_calls.append(1) or True
        try:
            shutdown_all = main.make_shutdown_all(engine, midi)
            for _ in range(times):
                shutdown_all()
        finally:
            launcher.stop_daemon = original
        return engine, midi, stop_calls

    def test_stops_the_daemon_we_spawned(self):
        engine, midi, stop_calls = self._run(spawned_by_us=True)
        self.assertEqual(len(stop_calls), 1)
        self.assertEqual(engine.shutdowns, 1)
        self.assertEqual(midi.closes, 1)

    def test_leaves_a_daemon_we_did_not_spawn_alone(self):
        engine, _midi, stop_calls = self._run(spawned_by_us=False)
        self.assertEqual(stop_calls, [])
        self.assertEqual(engine.shutdowns, 1)

    def test_python_engine_mode_has_nothing_to_stop(self):
        engine, _midi, stop_calls = self._run(spawned_by_us=None)  # classic engine
        self.assertEqual(stop_calls, [])
        self.assertEqual(engine.shutdowns, 1)

    def test_teardown_runs_at_most_once(self):
        engine, midi, stop_calls = self._run(spawned_by_us=True, times=3)
        self.assertEqual(engine.shutdowns, 1)
        self.assertEqual(midi.closes, 1)
        self.assertEqual(len(stop_calls), 1)

    def test_a_broken_engine_still_stops_the_daemon(self):
        class _Broken(self._Engine):
            def shutdown(self):
                raise RuntimeError("engine exploded")

        engine = _Broken(True)
        stop_calls = []
        original = launcher.stop_daemon
        launcher.stop_daemon = lambda: stop_calls.append(1) or True
        try:
            main.make_shutdown_all(engine, self._Midi())()
        finally:
            launcher.stop_daemon = original
        self.assertEqual(len(stop_calls), 1)


class ShutdownControllerUnitTest(unittest.TestCase):
    """Signal handlers, idempotency, and the forced-exit safety nets."""

    def _controller(self, **kwargs):
        self.cleanups = []
        self.hard_cleanups = []

        def cleanup():
            self.cleanups.append(1)

        def hard_cleanup():
            self.hard_cleanups.append(1)

        kwargs.setdefault("exit_code", 0)
        kwargs.setdefault("watchdog_timeout", 0)  # No stray watchdog in tests.
        kwargs.setdefault("quit_timeout", 0)
        kwargs.setdefault("use_wakeup_fd", False)
        return ShutdownController(
            cleanup, hard_cleanup=hard_cleanup, **kwargs
        )

    # -- installation -------------------------------------------------------

    def test_handlers_are_installed_for_term_int_and_hup(self):
        controller = self._controller()
        previous = {}
        try:
            previous = controller.install()
            self.assertEqual(signal.getsignal(signal.SIGTERM), controller._handle)
            self.assertEqual(signal.getsignal(signal.SIGINT), controller._handle)
            if hasattr(signal, "SIGHUP"):
                self.assertEqual(
                    signal.getsignal(signal.SIGHUP), controller._handle
                )
                self.assertIn(signal.SIGHUP, controller.installed_signals)
        finally:
            controller.restore()
        # restore() puts the previous dispositions back (SIG_DFL by default).
        self.assertEqual(signal.getsignal(signal.SIGTERM), previous[signal.SIGTERM])
        self.assertNotEqual(signal.getsignal(signal.SIGTERM), controller._handle)

    # -- the signal path ----------------------------------------------------

    def test_sigterm_runs_the_cleanup_then_exits(self):
        controller = self._controller()
        with mock.patch("os._exit") as fake_exit, mock.patch(
            "logging.shutdown", lambda: None
        ):
            controller.request_shutdown(signal.SIGTERM)
        self.assertEqual(self.cleanups, [1])
        fake_exit.assert_called_once_with(0)

    def test_handler_is_idempotent(self):
        """Two signals: one teardown, one immediate exit (like holafd)."""
        controller = self._controller(quit_ui=lambda: None)
        with mock.patch("os._exit") as fake_exit, mock.patch(
            "logging.shutdown", lambda: None
        ):
            controller._handle(signal.SIGTERM, None)
            controller._handle(signal.SIGTERM, None)
        self.assertEqual(self.cleanups, [1])
        self.assertEqual(fake_exit.call_args_list, [
            mock.call(signal_shutdown.EXIT_CODE_SECOND_SIGNAL)
        ])

    def test_signals_report_a_shutdown_request(self):
        controller = self._controller()
        with mock.patch("os._exit"), mock.patch("logging.shutdown", lambda: None):
            self.assertFalse(controller.shutdown_requested)
            controller.request_shutdown(signal.SIGHUP)

    def test_cleanup_failure_still_exits(self):
        controller = self._controller()
        controller._cleanup = mock.Mock(side_effect=RuntimeError("boom"))
        with mock.patch("os._exit") as fake_exit, mock.patch(
            "logging.shutdown", lambda: None
        ):
            controller.request_shutdown(signal.SIGTERM)
        controller._cleanup.assert_called_once()
        fake_exit.assert_called_once_with(0)

    def test_quit_ui_is_used_instead_of_a_brutal_exit(self):
        quit_calls = []
        controller = self._controller(quit_ui=lambda: quit_calls.append(1))
        with mock.patch("os._exit") as fake_exit:
            controller.request_shutdown(signal.SIGTERM)
        self.assertEqual(self.cleanups, [1])
        self.assertEqual(quit_calls, [1])       # Qt event loop asked to unwind…
        fake_exit.assert_not_called()           # …and main() exits normally.

    # -- forced-exit nets ---------------------------------------------------

    def test_watchdog_forces_the_exit_when_the_teardown_hangs(self):
        controller = self._controller(watchdog_timeout=0.2)
        controller._cleanup = lambda: time.sleep(5)  # Never returns in time.
        with mock.patch("os._exit") as fake_exit, mock.patch(
            "logging.shutdown", lambda: None
        ):
            worker = threading.Thread(
                target=controller.request_shutdown, args=(signal.SIGTERM,)
            )
            worker.daemon = True
            worker.start()
            deadline = time.monotonic() + 3
            while not self.hard_cleanups and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(self.hard_cleanups, "le watchdog n'a pas réagi")
            fake_exit.assert_called_once_with(0)

    def test_wakeup_fd_arms_the_watchdog_when_the_main_thread_is_stuck(self):
        """A signal noticed by CPython's C handler (idle Qt loop) still exits."""
        controller = self._controller(use_wakeup_fd=True, watchdog_timeout=0.2)
        try:
            controller.install()
            self.assertIsNotNone(controller._wakeup_write_fd)
            with mock.patch("os._exit") as fake_exit, mock.patch(
                "logging.shutdown", lambda: None
            ):
                os.write(controller._wakeup_write_fd, bytes([signal.SIGTERM]))
                deadline = time.monotonic() + 3
                while not self.hard_cleanups and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertTrue(
                    self.hard_cleanups,
                    "le watchdog n'a pas été armé par le wakeup fd",
                )
                fake_exit.assert_called_once_with(0)
        finally:
            controller.restore()

    def test_wakeup_fd_does_not_fire_after_a_graceful_teardown(self):
        controller = self._controller(
            use_wakeup_fd=True, watchdog_timeout=0.2, quit_ui=lambda: None
        )
        try:
            controller.install()
            with mock.patch("os._exit") as fake_exit:
                controller.request_shutdown(signal.SIGTERM)
                time.sleep(0.4)  # > watchdog timeout
            fake_exit.assert_not_called()
            self.assertEqual(self.hard_cleanups, [])
        finally:
            controller.restore()


# ---------------------------------------------------------------------------
# Live integration tests (real holafd + real PipeWire)
# ---------------------------------------------------------------------------

CHILD_SCRIPT = textwrap.dedent(
    """
    import logging
    import os
    import sys
    import time

    sys.path.insert(0, {repo!r})
    logging.basicConfig(level=logging.INFO)

    from src.backend import rust_launcher
    from src.backend.rust_launcher import ensure_daemon_running, stop_daemon
    from src.backend.signal_shutdown import ShutdownController

    socket_path = os.environ["HOLAF_SOCKET"]
    handle = ensure_daemon_running(socket_path)
    proc = rust_launcher._process
    print("READY", os.getpid(), proc.pid if proc else -1, flush=True)
    if not handle.ok:
        print("DAEMON-FAILED", handle.reason, flush=True)
        sys.exit(3)

    def cleanup():
        print("CLEANUP", flush=True)
        stop_daemon()

    # Exactly what main() installs: the same teardown the Qt path uses.
    controller = ShutdownController(cleanup=cleanup, quit_ui=None, exit_code=0)
    controller.install()
    print("HANDLERS-INSTALLED", flush=True)

    # A bytecode-running loop: the signal handler has to actually run.
    while True:
        time.sleep(0.05)
    """
)


def _pw_dump():
    """``pw-dump`` JSON, or None when the environment is not live."""
    global _PW_DUMP_ERROR
    _PW_DUMP_ERROR = None
    try:
        result = subprocess.run(
            ["pw-dump"], capture_output=True, timeout=15, check=True
        )
    except (OSError, subprocess.SubprocessError) as exc:
        _PW_DUMP_ERROR = f"pw-dump failed: {exc}"
        return None
    try:
        return json.loads(result.stdout.decode("utf-8", "replace"))
    except ValueError as exc:
        _PW_DUMP_ERROR = f"pw-dump produced invalid JSON: {exc}"
        return None


_PW_DUMP_ERROR = None


def _holaf_nodes():
    """Names of the PipeWire nodes created by holafd (Holaf_*)."""
    dump = _pw_dump()
    if dump is None:
        return None
    names = []
    for obj in dump:
        props = (obj.get("info") or {}).get("props") or {}
        name = props.get("node.name")
        if isinstance(name, str) and "Holaf_" in name:
            names.append(name)
    return sorted(names)


def _process_alive(pid):
    """Is `pid` a live (non-zombie) process?

    Zombies are excluded on purpose: in a container without a reaping init a
    defunct child lingers in ``ps``/``pgrep`` long after it died, which would
    make an "is it gone?" assertion meaningless.
    """
    try:
        with open(f"/proc/{pid}/stat", "rb") as handle:
            data = handle.read()
    except OSError:
        return False
    try:
        stat = data.rsplit(b")", 1)[1].split()[0]
    except IndexError:  # pragma: no cover - /proc always has a state field
        return False
    return stat not in (b"Z", b"X")


def _live_skip_reason():
    """Why the live tests must be skipped (None when they can run)."""
    if os.environ.get("HOLAF_NO_LIVE_DAEMON_TESTS"):
        return "HOLAF_NO_LIVE_DAEMON_TESTS is set"
    if not sys.platform.startswith("linux"):
        return "PR_SET_PDEATHSIG and /proc are Linux-only"
    if launcher._find_holafd_binary() is None:
        return "no holafd binary (build it with ./scripts/build_rust.sh)"
    if shutil.which("pw-dump") is None:
        return "pw-dump is not installed"
    if _pw_dump() is None:
        return (
            "no reachable PipeWire "
            "(eval \"$(./scripts/start-pipewire-headless.sh --export)\")"
        )
    return None


class LiveTeardownTest(unittest.TestCase):
    """End-to-end: a real app process, a real holafd, real PipeWire.

    These are the tests that prove the bug is fixed — they are only skipped
    when the machine has no daemon/PipeWire to talk to.
    """

    @classmethod
    def setUpClass(cls):
        reason = _live_skip_reason()
        if reason:
            raise unittest.SkipTest(reason)
        cls.holafctl = os.path.join(REPO_ROOT, "dist", "holafctl")
        if not os.access(cls.holafctl, os.X_OK):
            raise unittest.SkipTest("dist/holafctl is missing")

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="holaf-teardown-")
        self.tmp = self._tmp.name
        self.socket_path = os.path.join(self.tmp, "daemon.sock")
        self.child = None
        self.pump = None
        self.output = []

    def tearDown(self):
        if self.child is not None and self.child.poll() is None:
            self.child.kill()
            self.child.wait(timeout=10)
        if self.pump is not None:
            self.pump.join(timeout=5)
        if self.child is not None and self.child.stdout is not None:
            self.child.stdout.close()
        self._tmp.cleanup()

    # -- helpers ------------------------------------------------------------

    def _pump_output(self, pipe):
        """Drain the child's merged stdout/stderr (no select/readline races)."""
        try:
            for line in pipe:
                self.output.append(line)
        except (ValueError, OSError):
            pass  # Pipe closed by tearDown.

    def _start_app(self, tag=""):
        """Run the child "app": spawns holafd, installs handlers, then idles."""
        script = os.path.join(self.tmp, "fake_app.py")
        if not os.path.exists(script):
            with open(script, "w") as handle:
                handle.write(CHILD_SCRIPT.format(repo=REPO_ROOT))
        env = dict(os.environ)
        env["HOLAF_SOCKET"] = self.socket_path
        env.pop("HOLAFD_BIN", None)
        env["HOLAFD_BACKEND"] = "pipewire"
        self.output = []
        self.child = subprocess.Popen(
            [sys.executable, "-u", script],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=env,
            bufsize=1,
        )
        self.pump = threading.Thread(
            target=self._pump_output, args=(self.child.stdout,), daemon=True
        )
        self.pump.start()

        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            for line in list(self.output):
                if line.startswith("READY"):
                    parts = line.split()
                    self.app_pid = int(parts[1])
                    self.daemon_pid = int(parts[2])
                    return
                if line.startswith("DAEMON-FAILED"):
                    self.fail(f"{tag}daemon failed to start: {line.strip()}")
            if self.child.poll() is not None:
                self.fail(
                    f"{tag}the fake app died during startup:\n{''.join(self.output)}"
                )
            time.sleep(0.02)
        self.fail(f"{tag}the fake app did not report READY:\n{''.join(self.output)}")

    def _build_graph(self, tag=""):
        """Create real Holaf_* PipeWire nodes through the daemon."""
        config = os.path.join(self.tmp, "config.json")
        with open(os.path.join(REPO_ROOT, "config.json")) as handle:
            data = json.load(handle)
        strip = data["strips"][0]
        strip.update(
            {
                "uid": "teardown-test",
                "label": "Teardown",
                "kind": "output",
                "mode": "virtual",
                "device_name": None,
                "routes": [],
                "assigned_apps": [],
            }
        )
        data["strips"] = [strip]
        with open(config, "w") as handle:
            json.dump(data, handle)
        try:
            applied = subprocess.run(
                [
                    self.holafctl,
                    "--socket",
                    self.socket_path,
                    "apply-snapshot",
                    "--config",
                    config,
                ],
                capture_output=True,
                timeout=30,
            )
        except subprocess.SubprocessError as exc:
            self.fail(f"{tag}holafctl crashed: {exc}")
        if applied.returncode != 0:
            self.fail(
                f"{tag}apply-snapshot failed (rc={applied.returncode}): "
                f"{applied.stdout.decode(errors='replace')}"
                f"{applied.stderr.decode(errors='replace')}\n"
                f"{self._daemon_log()}"
            )
        deadline = time.monotonic() + 10
        nodes = None
        while time.monotonic() < deadline:
            nodes = _holaf_nodes()
            if nodes:
                return nodes
            time.sleep(0.1)
        self.fail(
            f"{tag}the daemon created no Holaf_* PipeWire node "
            f"({_PW_DUMP_ERROR or 'pw-dump sees no Holaf_ node'})\n"
            f"{self._daemon_log()}"
        )

    def _daemon_log(self, limit=2000):
        """Tail of the daemon's log (the launcher writes <socket>.log)."""
        path = f"{self.socket_path}.log"
        try:
            with open(path, "rb") as handle:
                data = handle.read()
        except OSError:
            return f"(no daemon log at {path})"
        return f"--- {path} ---\n" + data[-limit:].decode("utf-8", "replace")

    def _wait_gone(self, pid, timeout=2.0):
        """Seconds until `pid` disappeared, or None if it survived ``timeout``."""
        started = time.monotonic()
        while time.monotonic() - started < timeout:
            if not _process_alive(pid):
                return time.monotonic() - started
            time.sleep(0.02)
        return None

    def _wait_back_to_baseline(self, baseline, timeout=2.0):
        """New Holaf_* nodes still present after ``timeout`` (empty = clean).

        Compared against ``baseline`` because another daemon (another test, the
        user's own session) may legitimately own Holaf_* nodes — what matters is
        that *our* daemon left nothing behind.
        """
        deadline = time.monotonic() + timeout
        while True:
            current = _holaf_nodes()
            new = None if current is None else [n for n in current if n not in baseline]
            if not new or time.monotonic() >= deadline:
                return new
            time.sleep(0.05)

    def _finish_child(self, timeout=20):
        """Reap the child and collect everything it printed."""
        returncode = self.child.wait(timeout=timeout)
        self.pump.join(timeout=5)
        return returncode, "".join(self.output)

    # -- the scenarios ------------------------------------------------------

    def test_sigkill_of_the_app_kills_the_daemon_within_2s(self):
        """VERROU 1: SIGKILL runs no handler at all — only the kernel can help."""
        baseline = _holaf_nodes() or []
        self._start_app()
        nodes = self._build_graph()
        self.assertTrue(nodes, "graph was built")
        self.assertTrue(_process_alive(self.daemon_pid))

        started = time.monotonic()
        os.kill(self.app_pid, signal.SIGKILL)
        returncode, output = self._finish_child()
        gone_after = self._wait_gone(self.daemon_pid, timeout=2.0)
        total = time.monotonic() - started

        self.assertEqual(returncode, -signal.SIGKILL)
        # Proof that no Python teardown could possibly have run:
        self.assertNotIn("CLEANUP", output)
        self.assertIsNotNone(
            gone_after,
            f"holafd (pid {self.daemon_pid}) survived the SIGKILL of the app for "
            f"more than 2s — the PR_SET_PDEATHSIG belt did not fire",
        )
        self.assertLess(total, 2.0)
        self.assertEqual(
            self._wait_back_to_baseline(baseline),
            [],
            f"de PipeWire debris left behind ({_PW_DUMP_ERROR or 'nodes'})",
        )

    def test_external_signals_stop_the_daemon_and_exit_zero(self):
        """VERROU 2: SIGTERM / SIGHUP / SIGINT → clean exit 0, daemon stopped."""
        for signame in ("SIGTERM", "SIGHUP", "SIGINT"):
            with self.subTest(signal=signame):
                self._teardown_scenario(getattr(signal, signame), signame)

    def _teardown_scenario(self, signum, signame):
        if self.child is not None:
            if self.child.poll() is None:
                self.child.kill()
                self.child.wait(timeout=10)
            if self.pump is not None:
                self.pump.join(timeout=5)
            self.child.stdout.close()
            self.child = None
            self.pump = None
            self.socket_path = os.path.join(self.tmp, f"{signame}.sock")

        baseline = _holaf_nodes() or []
        self._start_app(tag=f"{signame}: ")
        self.assertTrue(self._build_graph(tag=f"{signame}: "))
        self.assertTrue(_process_alive(self.daemon_pid))

        started = time.monotonic()
        os.kill(self.app_pid, signum)
        returncode, output = self._finish_child()
        total = time.monotonic() - started

        self.assertEqual(
            returncode,
            0,
            f"{signame}: the app exited with {returncode}, expected 0.\n{output}",
        )
        self.assertIn(f"received {signame}, shutting down", output)
        self.assertIn("CLEANUP", output)
        gone_after = self._wait_gone(self.daemon_pid, timeout=2.0)
        self.assertIsNotNone(
            gone_after,
            f"{signame}: holafd (pid {self.daemon_pid}) outlived the app",
        )
        self.assertLess(total, 20.0)
        self.assertEqual(
            self._wait_back_to_baseline(baseline),
            [],
            f"{signame}: PipeWire debris ({_PW_DUMP_ERROR or 'nodes'})",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
