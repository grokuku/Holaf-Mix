"""launcher — resolve and run the Rust ``holaf-ui`` binary (migration B1).

The Holaf-Mix user interface is the Rust/egui app
(``rust/crates/holaf-ui``).  ``main.py`` no longer opens a Qt window by
itself: it is a *launcher* that locates the ``holaf-ui`` executable and runs
it, handing the command line through unchanged (``--config``, ``--midi``,
``--midi-list``, ``--always-on-top``, …).

Escape hatch
------------
``HOLAF_UI=python`` (or the older ``HOLAF_USE_PYTHON=1``) runs the classic
PySide6 interface exactly as before — the Qt code is still in the tree, still
tested, and still the fallback when the Rust binary is missing.

Where the binary is looked for
------------------------------
Same order as :mod:`src.backend.rust_launcher` uses for ``holafd``:
``$HOLAF_UI_BIN`` → frozen bundle (next to ``sys.executable``, then
``sys._MEIPASS``, then ``<app dir>/_internal``) → dev tree
(``rust/target/release`` then ``rust/target/debug``) → ``$PATH``.  The rule
that matched is logged, so a "which UI am I running?" question is answered by
the log alone.

The config path is *pinned* by the launcher
-------------------------------------------
``holaf_core::config::default_config_path()`` resolves from the **current
directory** (walking up to the repo root), while the Qt app resolves from its
own location (``src/config/settings._resolve_config_path``).  Started from a
.desktop file, the current directory is the user's home: the Rust UI would
look for ``$HOME/config.json`` and silently create a fresh one instead of
opening the user's mixer.  The launcher therefore always passes
``--config <same file the Qt UI would use>`` unless the caller gave one, so the
two UIs share one ``config.json`` — which is what makes the switch reversible.

Failure policy
--------------
* binary not found / not executable → :class:`LaunchResult` with
  ``launched=False`` and an actionable ``reason``; the caller (``main.main``)
  logs it and falls back to the Qt interface.  Never a traceback;
* binary found and started → its exit code is propagated untouched.  The
  launcher never "retries" with Qt: on a headless machine ``holaf-ui`` prints
  eframe's own "no display server" message and exits 1, and opening a second
  GUI toolkit would only hide that.
"""

import logging
import os
import shutil
import subprocess
import sys
from typing import List, NamedTuple, Optional, Sequence

logger = logging.getLogger("Launcher")

# ---------------------------------------------------------------------------
# Environment
# ---------------------------------------------------------------------------
#: ``HOLAF_UI=rust`` (default) or ``HOLAF_UI=python``.
ENV_UI = "HOLAF_UI"
#: ``HOLAF_USE_PYTHON=1`` — compat spelling of ``HOLAF_UI=python``.
ENV_USE_PYTHON = "HOLAF_USE_PYTHON"
#: ``HOLAF_UI_BIN=/path/to/holaf-ui`` — explicit binary (must be executable).
ENV_UI_BIN = "HOLAF_UI_BIN"

#: Name of the Rust UI executable.
UI_BINARY_NAME = "holaf-ui"

_TRUTHY = frozenset({"1", "true", "yes", "on"})
_FALSY = frozenset({"0", "false", "no", "off"})

# Profiles probed in the dev tree, in order (a release build is what users get).
_DEV_PROFILES = ("release", "debug")


class LaunchResult(NamedTuple):
    """Outcome of :func:`launch`.

    ``launched`` is True as soon as the Rust UI process was started — the
    ``exit_code`` is then the UI's own (0 = clean quit).  When False,
    ``reason`` says why nothing ran (a single, user-actionable line) and the
    caller must fall back to the Qt interface.
    """

    launched: bool
    exit_code: int
    reason: str


# ---------------------------------------------------------------------------
# UI choice
# ---------------------------------------------------------------------------
def _env_flag(name: str) -> bool:
    """True when ``name`` holds a truthy value; unset/garbage means False."""
    return os.environ.get(name, "").strip().lower() in _TRUTHY


def python_ui_forced() -> bool:
    """Is the classic PySide6 interface explicitly requested?

    * ``HOLAF_UI=python`` (current spelling), or
    * ``HOLAF_USE_PYTHON=1`` (compat: the variable that already meant "no Rust"
      for the audio backend, so a user who set it must not be surprised by a
      Rust window appearing).

    Anything else (unset, ``HOLAF_UI=rust``, a typo) means the Rust UI, which
    is the product default since migration B1.
    """
    choice = os.environ.get(ENV_UI, "").strip().lower()
    if choice and choice not in ("rust", "python"):
        logger.warning(
            "%s=%r is not a UI name (expected 'rust' or 'python') — using rust",
            ENV_UI,
            choice,
        )
    if choice == "python":
        return True
    if _env_flag(ENV_USE_PYTHON):
        return True
    legacy_rust = os.environ.get("HOLAF_USE_RUST", "").strip().lower()
    return legacy_rust in _FALSY


# ---------------------------------------------------------------------------
# Binary resolution
# ---------------------------------------------------------------------------
def _is_executable(path: str) -> bool:
    return os.path.isfile(path) and os.access(path, os.X_OK)


def _ensure_executable(path: str) -> bool:
    """True when ``path`` is an executable file, ``chmod +x``-ing it first.

    PyInstaller onefile bundles extract their payload — including the embedded
    ``holaf-ui`` — into ``sys._MEIPASS`` on every launch, and the extracted copy
    may be missing its executable bit.  Same helper (and same rationale) as
    :func:`src.backend.rust_launcher._ensure_executable`.
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
    """Directories to probe for a bundled ``holaf-ui`` in a frozen build.

    The app directory (where the distribution tarball puts ``holaf-ui`` and
    where onedir's top level lives), ``sys._MEIPASS`` (onefile extraction dir /
    onedir's ``_internal/``) and an explicit ``<app dir>/_internal`` for
    bootloaders that do not expose ``_MEIPASS``.
    """
    exe_dir = os.path.dirname(os.path.abspath(sys.executable))
    candidates = [exe_dir]
    meipass = getattr(sys, "_MEIPASS", None)
    if meipass:
        candidates.append(os.path.abspath(meipass))
    candidates.append(os.path.join(exe_dir, "_internal"))
    seen = set()
    ordered = []
    for directory in candidates:
        if directory not in seen:
            seen.add(directory)
            ordered.append(directory)
    return ordered


def _repo_root() -> str:
    """Repository root, derived from this file (``<root>/src/launcher.py``)."""
    return os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def find_ui_binary() -> Optional[str]:
    """Locate the ``holaf-ui`` executable, or return None.

    Search order: ``$HOLAF_UI_BIN`` → frozen bundle → ``rust/target/{release,
    debug}`` (dev checkout) → ``$PATH``.  The matching rule is logged.
    """
    env_bin = os.environ.get(ENV_UI_BIN)
    if env_bin:
        # Strict on purpose: an explicit path must already be executable.  We
        # never chmod a file the user named — a typo would otherwise turn an
        # arbitrary file into an executed "UI".
        if _is_executable(env_bin):
            logger.info("%s resolved from $%s: %s", UI_BINARY_NAME, ENV_UI_BIN, env_bin)
            return env_bin
        logger.warning(
            "%s=%s is not an executable file; ignoring", ENV_UI_BIN, env_bin
        )

    if getattr(sys, "frozen", False):
        for directory in _frozen_candidate_dirs():
            candidate = os.path.join(directory, UI_BINARY_NAME)
            if _ensure_executable(candidate):
                logger.info(
                    "%s resolved from the PyInstaller bundle: %s",
                    UI_BINARY_NAME,
                    candidate,
                )
                return candidate
        logger.info(
            "frozen app: no %s next to %s nor in %s — trying the repo/PATH lookup",
            UI_BINARY_NAME,
            os.path.abspath(sys.executable),
            getattr(sys, "_MEIPASS", "<no _MEIPASS>"),
        )

    target = os.path.join(_repo_root(), "rust", "target")
    for profile in _DEV_PROFILES:
        candidate = os.path.join(target, profile, UI_BINARY_NAME)
        if _is_executable(candidate):
            logger.info(
                "%s resolved from the dev tree (target/%s): %s",
                UI_BINARY_NAME,
                profile,
                candidate,
            )
            return candidate

    found = shutil.which(UI_BINARY_NAME)
    if found:
        logger.info("%s resolved from $PATH: %s", UI_BINARY_NAME, found)
    return found


# ---------------------------------------------------------------------------
# Command line
# ---------------------------------------------------------------------------
def config_path() -> str:
    """The ``config.json`` the Qt UI would use (see module docstring).

    Frozen: next to the executable (writable, survives upgrades).  Source tree:
    the repository root.  Mirrors
    :func:`src.config.settings._resolve_config_path` on purpose — one file for
    both UIs is what makes ``HOLAF_UI`` an escape hatch rather than a fork.
    """
    if getattr(sys, "frozen", False):
        base_dir = os.path.dirname(os.path.abspath(sys.executable))
    else:
        base_dir = _repo_root()
    return os.path.join(base_dir, "config.json")


def has_config_argument(argv: Sequence[str]) -> bool:
    """Does ``argv`` already choose its own config file?"""
    for arg in argv:
        if arg in ("--config", "-c") or arg.startswith("--config="):
            return True
    return False


def build_command(binary: str, argv: Sequence[str]) -> List[str]:
    """Full command line: ``binary`` + the caller's args + the pinned config.

    The caller's arguments come first so an explicit ``--config`` still wins
    (it is then the only one: see :func:`has_config_argument`).
    """
    command = [binary, *argv]
    if not has_config_argument(argv):
        command.extend(["--config", config_path()])
    return command


# ---------------------------------------------------------------------------
# Running
# ---------------------------------------------------------------------------
def run_binary(binary: str, argv: Sequence[str]) -> LaunchResult:
    """Start ``binary`` and wait for it, returning its exit code.

    The child inherits stdin/stdout/stderr and the process group: a Ctrl-C or a
    session logout reaches it directly (`holaf-ui` has no daemon to orphan — it
    drives the PipeWire backend in-process), and its console output reaches the
    user's terminal unfiltered.  A failure to *start* (permissions, wrong
    architecture, file vanished between the probe and the exec) is reported as
    ``launched=False`` instead of raising.
    """
    command = build_command(binary, argv)
    logger.info("launching %s", " ".join(command))
    try:
        completed = subprocess.run(command, check=False)
    except OSError as exc:
        return LaunchResult(
            launched=False,
            exit_code=0,
            reason=f"cannot execute {binary}: {exc}",
        )
    except KeyboardInterrupt:
        # The child got the same SIGINT (same process group) and is gone; the
        # shell convention for an interrupted foreground job is 130.
        return LaunchResult(launched=True, exit_code=130, reason="")
    return LaunchResult(launched=True, exit_code=completed.returncode, reason="")


def launch(
    argv: Sequence[str],
    finder=find_ui_binary,
    runner=run_binary,
) -> LaunchResult:
    """Locate the Rust UI and run it with ``argv``.

    ``finder``/``runner`` are injectable so the resolution and the fallback
    logic are unit-testable without building the binary (the tests pass fake
    ones).  Returns a :class:`LaunchResult`; never raises on a missing binary.
    """
    binary = finder()
    if not binary:
        return LaunchResult(
            launched=False,
            exit_code=0,
            reason=(
                f"the {UI_BINARY_NAME} binary was not found (build it with "
                "./scripts/build_rust.sh, or set "
                f"{ENV_UI_BIN})"
            ),
        )
    return runner(binary, list(argv))
