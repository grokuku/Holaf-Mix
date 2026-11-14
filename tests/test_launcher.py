"""Tests for the Rust-UI launcher (`src/launcher.py`, migration B1).

The UI of Holaf-Mix is now the Rust/egui app: `main.py` locates `holaf-ui` and
runs it.  What must hold, and what this module locks:

* **resolution** — `$HOLAF_UI_BIN` wins, then the frozen bundle (with the
  executable bit restored, PyInstaller extracts without it), then the dev tree
  (`rust/target/{release,debug}`), then `$PATH`;
* **argument passing** — the caller's flags (`--midi`, `--midi-list`, …) reach
  the binary, and `--config` is pinned to the file the Qt UI used *unless* the
  caller chose one (two UIs, one `config.json`);
* **fallback** — a missing or unlaunchable binary yields `launched=False` with
  an actionable reason (the caller then starts the Qt UI), never an exception;
* **escape hatch** — `HOLAF_UI=python` / `HOLAF_USE_PYTHON=1` select the Qt UI
  without probing anything.

No Rust binary and no Qt are needed: the resolver is pointed at fake
executables in a temp directory, and the "does it really run?" case uses a
shell script that records its own argv.

    python3 -m unittest tests.test_launcher -v
"""

import contextlib
import os
import sys
import tempfile
import unittest

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if REPO_ROOT not in sys.path:
    sys.path.insert(0, REPO_ROOT)

from src import launcher  # noqa: E402  (importable without PySide6)


_ENV_VARS = (
    launcher.ENV_UI,
    launcher.ENV_USE_PYTHON,
    launcher.ENV_UI_BIN,
    "HOLAF_USE_RUST",
)


@contextlib.contextmanager
def env(**values):
    """Set/clear the launcher env vars for one test, restoring them after."""
    saved = {name: os.environ.pop(name, None) for name in _ENV_VARS}
    for name, value in values.items():
        if value is not None:
            os.environ[name] = value
    try:
        yield
    finally:
        for name, value in saved.items():
            os.environ.pop(name, None)
            if value is not None:
                os.environ[name] = value


@contextlib.contextmanager
def frozen(meipass=None, executable=None):
    """Pretend this process runs from a PyInstaller bundle."""
    saved_frozen = getattr(sys, "frozen", None)
    saved_meipass = getattr(sys, "_MEIPASS", None)
    saved_executable = sys.executable
    sys.frozen = True  # type: ignore[attr-defined]
    if meipass is not None:
        sys._MEIPASS = meipass  # type: ignore[attr-defined]
    if executable is not None:
        sys.executable = executable
    try:
        yield
    finally:
        for name, value in (
            ("frozen", saved_frozen),
            ("_MEIPASS", saved_meipass),
        ):
            if value is None:
                if hasattr(sys, name):
                    delattr(sys, name)
            else:
                setattr(sys, name, value)
        sys.executable = saved_executable


def write_fake_binary(directory, name="holaf-ui", *, executable=True, body=None):
    """Create a fake `holaf-ui` and return its path.

    The default body is a POSIX shell script (so `subprocess.run` really
    executes something); `executable=False` leaves the mode at 0o644, which is
    what PyInstaller's onefile extraction used to produce.
    """
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(body if body is not None else "#!/bin/sh\nexit 0\n")
    os.chmod(path, 0o755 if executable else 0o644)
    return path


@contextlib.contextmanager
def isolated(root=None, which=None):
    """Silence the parts of the search path a test is not about.

    `_repo_root()` and `shutil.which()` are redirected so a test asserting "no
    binary anywhere" cannot be fooled by a real `rust/target/debug/holaf-ui`
    left over in the checkout (or by a packaged `holaf-ui` on `$PATH`).
    """
    saved_root = launcher._repo_root
    saved_which = launcher.shutil.which
    launcher._repo_root = lambda: root if root is not None else tempfile.mkdtemp()
    launcher.shutil.which = lambda name: which
    try:
        yield
    finally:
        launcher._repo_root = saved_root
        launcher.shutil.which = saved_which


class TestUiChoice(unittest.TestCase):
    """`HOLAF_UI` / `HOLAF_USE_PYTHON` decide which interface runs."""

    def test_the_rust_ui_is_the_default(self):
        with env():
            self.assertFalse(launcher.python_ui_forced())
        with env(HOLAF_UI="rust"):
            self.assertFalse(launcher.python_ui_forced())

    def test_holaf_ui_python_requests_the_qt_ui(self):
        with env(HOLAF_UI="python"):
            self.assertTrue(launcher.python_ui_forced())
        # Case / whitespace are free: an env var typed in a shell.
        with env(HOLAF_UI=" Python "):
            self.assertTrue(launcher.python_ui_forced())

    def test_the_legacy_spellings_still_work(self):
        # HOLAF_USE_PYTHON=1 already meant "no Rust" for the audio backend, so a
        # user who set it must not get a Rust window.
        with env(HOLAF_USE_PYTHON="1"):
            self.assertTrue(launcher.python_ui_forced())
        with env(HOLAF_USE_RUST="0"):
            self.assertTrue(launcher.python_ui_forced())
        with env(HOLAF_USE_PYTHON="0"):
            self.assertFalse(launcher.python_ui_forced())

    def test_an_unknown_value_falls_back_to_rust(self):
        with env(HOLAF_UI="qt5"):
            self.assertFalse(launcher.python_ui_forced())


class TestResolution(unittest.TestCase):
    """Where the `holaf-ui` executable is looked for."""

    def test_holaf_ui_bin_wins_and_must_be_executable(self):
        with tempfile.TemporaryDirectory() as tmp, env(), isolated(root=tmp):
            binary = write_fake_binary(tmp)
            with env(HOLAF_UI_BIN=binary):
                self.assertEqual(launcher.find_ui_binary(), binary)
            # A path that exists but is not executable is ignored (never
            # chmod-ed: the user named it).
            plain = write_fake_binary(tmp, name="not-executable", executable=False)
            with env(HOLAF_UI_BIN=plain):
                self.assertFalse(os.access(plain, os.X_OK))
                self.assertIsNone(launcher.find_ui_binary())
            # A typo in the variable must not crash the launcher.
            with env(HOLAF_UI_BIN=os.path.join(tmp, "nope")):
                self.assertIsNone(launcher.find_ui_binary())

    def test_the_frozen_bundle_is_probed_and_chmodded(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = write_fake_binary(tmp, executable=False)
            app_dir = os.path.join(tmp, "app")
            os.makedirs(app_dir)
            exe = os.path.join(app_dir, "holaf-mix")
            with open(exe, "w", encoding="utf-8") as handle:
                handle.write("")
            with env(), frozen(meipass=tmp, executable=exe), isolated(root=tmp):
                self.assertEqual(launcher.find_ui_binary(), binary)
            # PyInstaller extracts the payload without the executable bit; the
            # resolver restores it so the launch cannot fail with EACCES.
            self.assertTrue(os.access(binary, os.X_OK))

    def test_the_frozen_app_directory_is_probed_first(self):
        with tempfile.TemporaryDirectory() as tmp:
            app_dir = os.path.join(tmp, "app")
            os.makedirs(app_dir)
            beside = write_fake_binary(app_dir)
            write_fake_binary(tmp)  # also in _MEIPASS: the nearer copy wins
            exe = os.path.join(app_dir, "holaf-mix")
            with open(exe, "w", encoding="utf-8") as handle:
                handle.write("")
            with env(), frozen(meipass=tmp, executable=exe), isolated(root=tmp):
                self.assertEqual(launcher.find_ui_binary(), beside)

    def test_the_dev_tree_is_probed_release_then_debug(self):
        with tempfile.TemporaryDirectory() as tmp, env(), isolated(root=tmp):
            release = os.path.join(tmp, "rust", "target", "release")
            debug = os.path.join(tmp, "rust", "target", "debug")
            os.makedirs(release)
            os.makedirs(debug)
            debug_bin = write_fake_binary(debug)
            self.assertEqual(launcher.find_ui_binary(), debug_bin)
            release_bin = write_fake_binary(release)
            self.assertEqual(launcher.find_ui_binary(), release_bin)

    def test_path_is_the_last_resort(self):
        with tempfile.TemporaryDirectory() as tmp, env():
            binary = write_fake_binary(tmp)
            empty = os.path.join(tmp, "empty")
            os.makedirs(empty)
            with isolated(root=empty, which=binary):
                self.assertEqual(launcher.find_ui_binary(), binary)

    def test_nothing_found_returns_none(self):
        with tempfile.TemporaryDirectory() as tmp, env(), isolated(root=tmp):
            self.assertIsNone(launcher.find_ui_binary())


class TestCommandLine(unittest.TestCase):
    """What is handed to the binary."""

    def test_the_config_path_is_pinned(self):
        command = launcher.build_command("/bin/holaf-ui", ["--midi", "Mix MIDI 1"])
        self.assertEqual(command[0], "/bin/holaf-ui")
        self.assertEqual(command[1:3], ["--midi", "Mix MIDI 1"])
        self.assertEqual(command[3], "--config")
        self.assertEqual(command[4], launcher.config_path())

    def test_an_explicit_config_is_never_duplicated(self):
        for argv in (
            ["--config", "/tmp/x.json"],
            ["-c", "/tmp/x.json"],
            ["--config=/tmp/x.json"],
        ):
            command = launcher.build_command("/bin/holaf-ui", argv)
            self.assertEqual(command, ["/bin/holaf-ui", *argv], argv)
        self.assertTrue(launcher.has_config_argument(["-c", "x"]))
        self.assertFalse(launcher.has_config_argument(["--midi", "x"]))

    def test_the_config_path_is_the_one_the_qt_ui_uses(self):
        # Source tree: `<repo>/config.json`, same as
        # `src/config/settings._resolve_config_path()`.
        self.assertEqual(
            launcher.config_path(), os.path.join(REPO_ROOT, "config.json")
        )
        from src.config.settings import CONFIG_FILE

        self.assertEqual(launcher.config_path(), CONFIG_FILE)

    def test_frozen_config_lives_next_to_the_executable(self):
        with tempfile.TemporaryDirectory() as tmp:
            exe = os.path.join(tmp, "holaf-mix")
            with open(exe, "w", encoding="utf-8") as handle:
                handle.write("")
            with frozen(meipass=tmp, executable=exe):
                self.assertEqual(
                    launcher.config_path(), os.path.join(tmp, "config.json")
                )


class TestLaunch(unittest.TestCase):
    """The launch outcome (and the fallback the caller derives from it)."""

    def test_a_missing_binary_is_reported_not_raised(self):
        result = launcher.launch([], finder=lambda: None)
        self.assertFalse(result.launched)
        self.assertEqual(result.exit_code, 0)
        self.assertIn("holaf-ui", result.reason)
        self.assertIn(launcher.ENV_UI_BIN, result.reason)

    def test_the_exit_code_of_the_ui_is_propagated(self):
        calls = []

        def runner(binary, argv):
            calls.append((binary, argv))
            return launcher.LaunchResult(True, 0, "")

        result = launcher.launch(
            ["--midi-list"], finder=lambda: "/bin/holaf-ui", runner=runner
        )
        self.assertTrue(result.launched)
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(calls, [("/bin/holaf-ui", ["--midi-list"])])

    def test_a_real_executable_receives_the_arguments(self):
        with tempfile.TemporaryDirectory() as tmp, env():
            record = os.path.join(tmp, "argv.txt")
            binary = write_fake_binary(
                tmp,
                body=f'#!/bin/sh\nprintf "%s\\n" "$@" > {record}\nexit 7\n',
            )
            with env(HOLAF_UI_BIN=binary):
                result = launcher.launch(["--midi", "Mix MIDI 1"])
            self.assertTrue(result.launched)
            self.assertEqual(result.exit_code, 7, "the UI's exit code is propagated")
            with open(record, encoding="utf-8") as handle:
                argv = handle.read().splitlines()
            self.assertEqual(argv[:2], ["--midi", "Mix MIDI 1"])
            self.assertEqual(argv[2], "--config")
            self.assertEqual(argv[3], launcher.config_path())

    def test_an_unlaunchable_file_falls_back(self):
        with tempfile.TemporaryDirectory() as tmp, env():
            # Executable bit set, but no shebang and not an ELF: execve fails
            # with ENOEXEC, which must become a reason, not a traceback.
            broken = write_fake_binary(tmp, body="not a program\n")
            result = launcher.run_binary(broken, [])
            self.assertFalse(result.launched)
            self.assertIn("cannot execute", result.reason)
            self.assertIn(broken, result.reason)


if __name__ == "__main__":
    unittest.main()
