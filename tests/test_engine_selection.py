"""Tests for the backend *selection* (Rust by default, Python on opt-out/fallback).

Covers the product decision that shipped with J5 ("Rust is the default mode"):

* no env var            -> IpcAudioEngine, daemon started with --backend pipewire
* HOLAF_USE_PYTHON=1    -> classic AudioEngine (opt-out)
* HOLAF_USE_RUST=0      -> classic AudioEngine (legacy opt-out spelling)
* daemon unavailable    -> classic AudioEngine + a logged warning + a
                           `_rust_fallback_reason` the UI can display
* frozen (PyInstaller)  -> holafd is resolved next to sys.executable / in
                           sys._MEIPASS and gets its executable bit restored

No Qt, no PipeWire and no real daemon is required: the launcher entry point is
monkeypatched and the frozen layout is simulated in a temp directory.

    python -m unittest discover -s tests -v
"""

import contextlib
import logging
import os
import stat
import sys
import tempfile
import unittest

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if REPO_ROOT not in sys.path:
    sys.path.insert(0, REPO_ROOT)

import main  # noqa: E402  (importable without PySide6: Qt is imported lazily)
import src.backend.rust_launcher as launcher  # noqa: E402
from src.backend.audio_engine import AudioEngine  # noqa: E402
from src.backend.ipc_audio_engine import IpcAudioEngine  # noqa: E402
from src.backend.rust_launcher import DaemonHandle  # noqa: E402

logging.disable(logging.CRITICAL)  # Keep unittest output clean.


@contextlib.contextmanager
def logging_enabled():
    """Re-enable logging for ``assertLogs`` despite the module-level silence.

    ``logging.disable(CRITICAL)`` (above, shared with the other test modules)
    short-circuits every logger, so ``assertLogs`` would capture nothing.
    Wrapping a capture in this context restores the silence afterwards.
    """
    logging.disable(logging.NOTSET)
    try:
        yield
    finally:
        logging.disable(logging.CRITICAL)


class _EnvSandbox:
    """Context manager: set/clear the backend env vars for one test."""

    VARS = ("HOLAF_USE_PYTHON", "HOLAF_USE_RUST", "HOLAFD_BIN", "HOLAFD_BACKEND")

    def __init__(self, **values):
        self.values = values
        self._saved = {}

    def __enter__(self):
        for name in self.VARS:
            self._saved[name] = os.environ.pop(name, None)
        for name, value in self.values.items():
            if value is not None:
                os.environ[name] = value
        return self

    def __exit__(self, *exc):
        for name, value in self._saved.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value
        return False


class _LauncherPatch:
    """Temporarily replace ``rust_launcher.ensure_daemon_running``.

    ``create_audio_engine`` imports the function inside its body, so patching
    the module attribute is enough to intercept the call.
    """

    def __init__(self, handle=None, exc=None):
        self.handle = handle
        self.exc = exc
        self.calls = 0

    def __call__(self, socket_path=None):
        self.calls += 1
        if self.exc is not None:
            raise self.exc
        return self.handle

    def __enter__(self):
        self._orig = launcher.ensure_daemon_running
        launcher.ensure_daemon_running = self
        return self

    def __exit__(self, *exc):
        launcher.ensure_daemon_running = self._orig
        return False


class _FrozenSandbox:
    """Simulate a PyInstaller frozen app in a temp directory.

    Sets ``sys.frozen`` / ``sys.executable`` / ``sys._MEIPASS`` exactly like the
    bootloader does, and restores them afterwards (plus any leftover launcher
    state).
    """

    def __init__(self):
        self.tmpdir = tempfile.mkdtemp(prefix="holaf-frozen-")
        self.app_dir = os.path.join(self.tmpdir, "app")
        os.makedirs(self.app_dir)
        self.exe = os.path.join(self.app_dir, "holaf-mix")
        with open(self.exe, "w") as fh:
            fh.write("#!/bin/sh\nexit 0\n")
        os.chmod(self.exe, 0o755)

    def _saved(self):
        return {
            "frozen": getattr(sys, "frozen", None),
            "executable": sys.executable,
            "meipass": getattr(sys, "_MEIPASS", None),
        }

    def __enter__(self):
        self._prev = self._saved()
        self._prev_env = os.environ.get("HOLAFD_BIN")
        os.environ.pop("HOLAFD_BIN", None)
        sys.frozen = True
        sys.executable = self.exe
        return self

    def place_daemon(self, directory=None, mode=0o644):
        """Drop a fake `holafd` into ``directory`` (default: the app dir)."""
        directory = directory or self.app_dir
        os.makedirs(directory, exist_ok=True)
        path = os.path.join(directory, "holafd")
        with open(path, "w") as fh:
            fh.write("#!/bin/sh\nexit 0\n")
        # Simulate an extraction that lost the executable bit (onefile).
        os.chmod(path, mode)
        return path

    def set_meipass(self, path):
        sys._MEIPASS = path

    def __exit__(self, *exc):
        if self._prev["frozen"] is None:
            try:
                del sys.frozen
            except AttributeError:
                pass
        else:
            sys.frozen = self._prev["frozen"]
        sys.executable = self._prev["executable"]
        if self._prev["meipass"] is None:
            try:
                del sys._MEIPASS
            except AttributeError:
                pass
        else:
            sys._MEIPASS = self._prev["meipass"]
        if self._prev_env is None:
            os.environ.pop("HOLAFD_BIN", None)
        else:
            os.environ["HOLAFD_BIN"] = self._prev_env
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)
        return False


class BackendSelectionTest(unittest.TestCase):
    """main.create_audio_engine() picks the right engine for each situation."""

    def test_default_is_the_rust_engine(self):
        with _EnvSandbox() as _env, _LauncherPatch(
            handle=DaemonHandle(ok=True, spawned_by_us=True)
        ) as patch:
            engine = main.create_audio_engine()
        self.assertIsInstance(engine, IpcAudioEngine)
        self.assertEqual(patch.calls, 1)
        # Ownership flag drives the app-quit teardown (stop only what we ran).
        self.assertTrue(engine._rust_daemon_spawned_by_us)
        self.assertIsNone(engine._rust_fallback_reason)

    def test_default_does_not_own_a_preexisting_daemon(self):
        with _EnvSandbox(), _LauncherPatch(
            handle=DaemonHandle(ok=True, spawned_by_us=False)
        ):
            engine = main.create_audio_engine()
        self.assertIsInstance(engine, IpcAudioEngine)
        self.assertFalse(engine._rust_daemon_spawned_by_us)

    def test_holaf_use_python_opts_out(self):
        with _EnvSandbox(HOLAF_USE_PYTHON="1"), _LauncherPatch(
            handle=DaemonHandle(ok=True, spawned_by_us=True)
        ) as patch:
            engine = main.create_audio_engine()
        self.assertIsInstance(engine, AudioEngine)
        self.assertNotIsInstance(engine, IpcAudioEngine)
        # The opt-out must not spawn (nor probe) the daemon at all.
        self.assertEqual(patch.calls, 0)
        self.assertFalse(getattr(engine, "_rust_daemon_spawned_by_us", False))
        self.assertIsNone(getattr(engine, "_rust_fallback_reason", None))

    def test_legacy_holaf_use_rust_zero_opts_out(self):
        with _EnvSandbox(HOLAF_USE_RUST="0"), _LauncherPatch(
            handle=DaemonHandle(ok=True, spawned_by_us=True)
        ) as patch:
            engine = main.create_audio_engine()
        self.assertIsInstance(engine, AudioEngine)
        self.assertEqual(patch.calls, 0)

    def test_holaf_use_rust_one_is_the_default_and_falls_back(self):
        # HISTORICAL spelling: HOLAF_USE_RUST=1 used to be the opt-in. It is now
        # simply "the default", so a broken daemon degrades instead of aborting.
        with _EnvSandbox(HOLAF_USE_RUST="1"), _LauncherPatch(
            handle=DaemonHandle(ok=False, spawned_by_us=False)
        ):
            engine = main.create_audio_engine()
        self.assertIsInstance(engine, AudioEngine)
        self.assertIn("holafd", engine._rust_fallback_reason)

    def test_daemon_unavailable_falls_back_visibly(self):
        with _EnvSandbox(), _LauncherPatch(
            handle=DaemonHandle(ok=False, spawned_by_us=False)
        ), logging_enabled():
            with self.assertLogs("main", level="WARNING") as logs:
                engine = main.create_audio_engine()
        self.assertIsInstance(engine, AudioEngine)
        self.assertTrue(engine._rust_fallback_reason)  # UI-visible reason
        self.assertFalse(getattr(engine, "_rust_daemon_spawned_by_us", False))
        self.assertTrue(
            any("falling back" in line for line in logs.output),
            logs.output,
        )

    def test_launcher_exception_falls_back_instead_of_crashing(self):
        with _EnvSandbox(), _LauncherPatch(
            exc=OSError("socket unwritable")
        ), logging_enabled():
            with self.assertLogs("main", level="WARNING"):
                engine = main.create_audio_engine()
        self.assertIsInstance(engine, AudioEngine)
        self.assertIn("socket unwritable", engine._rust_fallback_reason)

    def test_missing_ipc_module_falls_back(self):
        # Simulate a broken/absent IPC client (e.g. partially installed bundle).
        saved = sys.modules.get("src.backend.ipc_audio_engine")
        sys.modules["src.backend.ipc_audio_engine"] = None  # import -> ImportError
        try:
            with _EnvSandbox(), logging_enabled():
                with self.assertLogs("main", level="WARNING"):
                    engine = main.create_audio_engine()
        finally:
            if saved is None:
                sys.modules.pop("src.backend.ipc_audio_engine", None)
            else:
                sys.modules["src.backend.ipc_audio_engine"] = saved
        self.assertIsInstance(engine, AudioEngine)
        self.assertTrue(engine._rust_fallback_reason)

    def test_fallback_notice_reaches_the_status_bar(self):
        class _StatusBar:
            def __init__(self):
                self.calls = []

            def showMessage(self, message, timeout=0):  # noqa: N802 (Qt API)
                self.calls.append((message, timeout))

        class _Window:
            def __init__(self):
                self._bar = _StatusBar()

            def statusBar(self):  # noqa: N802 (Qt API)
                return self._bar

        window = _Window()
        main._show_fallback_notice(window, "holafd not found")
        self.assertEqual(len(window._bar.calls), 1)
        message, timeout = window._bar.calls[0]
        self.assertIn("holafd not found", message)
        self.assertIn("Python", message)
        self.assertEqual(timeout, main.FALLBACK_NOTICE_TIMEOUT_MS)

    def test_fallback_notice_never_raises(self):
        class _BrokenWindow:
            def statusBar(self):  # noqa: N802 (Qt API)
                raise RuntimeError("no Qt here")

        # Must swallow the error: the notice cannot abort startup.
        main._show_fallback_notice(_BrokenWindow(), "boom")


class BinaryResolutionTest(unittest.TestCase):
    """rust_launcher._find_holafd_binary() across dev / frozen layouts."""

    def setUp(self):
        # Neutralise the dev tree so only the layout under test can answer.
        self._orig_which = launcher.shutil.which
        launcher.shutil.which = lambda name: None
        self._orig_env = os.environ.pop("HOLAFD_BIN", None)

    def tearDown(self):
        launcher.shutil.which = self._orig_which
        if self._orig_env is not None:
            os.environ["HOLAFD_BIN"] = self._orig_env
        else:
            os.environ.pop("HOLAFD_BIN", None)

    def test_frozen_daemon_next_to_the_executable(self):
        with _FrozenSandbox() as box:
            expected = box.place_daemon()
            found = launcher._find_holafd_binary()
        self.assertEqual(found, expected)

    def test_frozen_daemon_in_meipass_gets_chmod_x(self):
        with _FrozenSandbox() as box:
            meipass = os.path.join(box.tmpdir, "_MEI12345")
            os.makedirs(meipass)
            extracted = box.place_daemon(meipass, mode=0o644)
            self.assertFalse(os.access(extracted, os.X_OK))
            box.set_meipass(meipass)
            found = launcher._find_holafd_binary()
            self.assertEqual(found, extracted)
            # Onefile extraction loses the bit -> the launcher restores it.
            self.assertTrue(os.access(extracted, os.X_OK))
            self.assertTrue(os.stat(extracted).st_mode & stat.S_IXUSR)

    def test_frozen_onedir_internal_dir(self):
        with _FrozenSandbox() as box:
            internal = os.path.join(box.app_dir, "_internal")
            os.makedirs(internal)
            expected = box.place_daemon(internal)
            found = launcher._find_holafd_binary()
        self.assertEqual(found, expected)

    def test_frozen_prefers_the_bundled_copy_over_the_dev_tree(self):
        with _FrozenSandbox() as box:
            bundled = box.place_daemon()
            found = launcher._find_holafd_binary()
        self.assertEqual(found, bundled)

    def test_frozen_without_bundle_falls_back_to_the_dev_tree(self):
        # A frozen app with no embedded daemon must still find the dev binary
        # (this is what keeps `pyinstaller main.py` usable while developing).
        with _FrozenSandbox():
            found = launcher._find_holafd_binary()
        self.assertIsNotNone(found)
        self.assertTrue(os.access(found, os.X_OK))

    def test_dev_tree_lookup_order(self):
        # The repo tree answers when nothing is frozen (release before debug).
        self.assertIsNone(getattr(sys, "frozen", None))
        found = launcher._find_holafd_binary()
        self.assertIsNotNone(found)
        self.assertEqual(
            os.path.basename(found), "holafd"
        )
        self.assertIn(os.path.join("rust", "target"), found)

    def test_holafd_bin_env_wins(self):
        with tempfile.TemporaryDirectory() as tmp:
            custom = os.path.join(tmp, "my-holafd")
            with open(custom, "w") as fh:
                fh.write("#!/bin/sh\nexit 0\n")
            os.chmod(custom, 0o755)
            os.environ["HOLAFD_BIN"] = custom
            try:
                self.assertEqual(launcher._find_holafd_binary(), custom)
            finally:
                os.environ.pop("HOLAFD_BIN", None)

    def test_nonexistent_holafd_bin_is_ignored(self):
        os.environ["HOLAFD_BIN"] = "/nonexistent/holafd"
        try:
            found = launcher._find_holafd_binary()
        finally:
            os.environ.pop("HOLAFD_BIN", None)
        self.assertNotEqual(found, "/nonexistent/holafd")


class DaemonBackendTest(unittest.TestCase):
    """The daemon must be started with the LIVE PipeWire backend by default."""

    class _FakeProc:
        def __init__(self, pid=4242):
            self.pid = pid
            self.returncode = None

        def poll(self):
            return self.returncode

        def terminate(self):
            self.returncode = -15

        def kill(self):
            self.returncode = -9

        def wait(self, timeout=None):
            return self.returncode

    def _spawn_and_capture(self, env=None):
        """Run ensure_daemon_running() with a fake Popen; return the argv used."""
        spawned = []
        orig = (
            launcher._find_holafd_binary,
            launcher.subprocess.Popen,
            launcher._daemon_responding,
            launcher._process,
            launcher._launched_socket,
        )
        path = os.path.join(
            tempfile.gettempdir(), f"holaf-backend-{os.getpid()}.sock"
        )
        saved_env = {k: os.environ.get(k) for k in ("HOLAFD_BACKEND",)}
        os.environ.pop("HOLAFD_BACKEND", None)
        for key, value in (env or {}).items():
            os.environ[key] = value
        try:
            launcher._find_holafd_binary = lambda: "/usr/bin/false"
            launcher.subprocess.Popen = lambda cmd, *a, **kw: (
                spawned.append(cmd) or self._FakeProc()
            )
            launcher._daemon_responding = lambda path_, timeout=0.5: bool(spawned)
            launcher._process = None
            launcher._launched_socket = None
            handle = launcher.ensure_daemon_running(socket_path=path)
            self.assertTrue(handle.ok)
            launcher.stop_daemon()
            return spawned[0]
        finally:
            (
                launcher._find_holafd_binary,
                launcher.subprocess.Popen,
                launcher._daemon_responding,
                launcher._process,
                launcher._launched_socket,
            ) = orig
            for key, value in saved_env.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
            for leftover in (path, f"{path}.log"):
                try:
                    os.unlink(leftover)
                except OSError:
                    pass

    def test_default_backend_is_pipewire(self):
        self.assertEqual(launcher.DEFAULT_BACKEND, "pipewire")
        argv = self._spawn_and_capture()
        self.assertIn("--backend", argv)
        self.assertEqual(argv[argv.index("--backend") + 1], "pipewire")

    def test_backend_can_be_overridden_for_debug(self):
        argv = self._spawn_and_capture({"HOLAFD_BACKEND": "fake"})
        self.assertEqual(argv[argv.index("--backend") + 1], "fake")

    def test_missing_binary_explains_the_failure(self):
        """A missing holafd must yield an actionable reason, not a bare False."""
        orig_find = launcher._find_holafd_binary
        orig_respond = launcher._daemon_responding
        path = os.path.join(
            tempfile.gettempdir(), f"holaf-nobin-{os.getpid()}.sock"
        )
        try:
            launcher._find_holafd_binary = lambda: None
            launcher._daemon_responding = lambda path_, timeout=0.5: False
            handle = launcher.ensure_daemon_running(socket_path=path)
            self.assertFalse(handle.ok)
            self.assertIn("binary not found", handle.reason)
            self.assertIn("build_rust.sh", handle.reason)
        finally:
            launcher._find_holafd_binary = orig_find
            launcher._daemon_responding = orig_respond
            for leftover in (path, f"{path}.log"):
                try:
                    os.unlink(leftover)
                except OSError:
                    pass


if __name__ == "__main__":
    unittest.main(verbosity=2)
