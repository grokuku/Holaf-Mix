"""Holaf-Mix entry point (UI + backend selection).

Backend selection (see :func:`create_audio_engine`)
---------------------------------------------------
* **Default: Rust.**  The app talks to the Rust `holafd` daemon over a Unix
  socket (`IpcAudioEngine`), spawned on demand with `--backend pipewire`.
* **Opt-out: Python.**  `HOLAF_USE_PYTHON=1` (or the legacy `HOLAF_USE_RUST=0`)
  keeps the classic in-process `AudioEngine` that drives PipeWire directly via
  `pw-cli`/`pactl`.
* **Fallback.**  When the daemon cannot be started (binary missing, socket
  unreachable, daemon died on startup) the app degrades to the Python engine.
  Never silent: a WARNING is logged and a notice with the reason is shown in
  the window's status bar.

Qt is imported lazily inside :func:`main` so the backend factory stays
importable (and unit-testable) on machines without PySide6 / mido, e.g. a
headless CI container.

Shutdown (two entry points, one teardown)
-----------------------------------------
``shutdown_all()`` — engine stop, MIDI port closed, Rust daemon stopped when
this process spawned it — is connected to ``QApplication.aboutToQuit`` *and*
run by the POSIX signal handlers installed by
:class:`src.backend.signal_shutdown.ShutdownController` (SIGTERM / SIGINT /
SIGHUP).  It is idempotent, so a clean quit and a signal can never tear the
app down twice.  The third net is the kernel: holafd is spawned with
``PR_SET_PDEATHSIG``, so it also dies when the app is SIGKILLed.
"""

import logging
import os
import sys
import threading

# Centralized logging configuration. Done here (not at module level in
# audio_engine.py) so the first-call-wins behavior of basicConfig doesn't
# make import order matter.
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
)

# Keep the last few hundred log lines in memory as well: the hidden
# diagnostics window (Ctrl+Shift+D) shows them in its "Journal" tab, and the
# report the user copies embeds them.  Without this the only copy of the log is
# stderr, which a desktop-launched app does not have.  Never fatal: a failing
# diagnostics handler must not prevent startup.
try:
    from src.diagnostics.logbuffer import install_log_handler

    install_log_handler()
except Exception:  # noqa: BLE001 - diagnostics is optional
    logging.getLogger("main").debug("in-memory log buffer not installed", exc_info=True)

logger = logging.getLogger("main")

# Env vars controlling backend selection.
ENV_PYTHON_ENGINE = "HOLAF_USE_PYTHON"   # HOLAF_USE_PYTHON=1 -> classic engine
ENV_RUST_ENGINE = "HOLAF_USE_RUST"       # legacy: HOLAF_USE_RUST=0 -> classic

_TRUTHY = frozenset({"1", "true", "yes", "on"})
_FALSY = frozenset({"0", "false", "no", "off"})

# How long the "fell back to the Python engine" notice stays in the status bar.
FALLBACK_NOTICE_TIMEOUT_MS = 30000


def _env_flag(name: str) -> bool:
    """True when the env var ``name`` is set to a truthy value.

    Unset / empty / anything unrecognised means False, so a typo can never
    silently switch the backend.
    """
    return os.environ.get(name, "").strip().lower() in _TRUTHY


def python_engine_forced() -> bool:
    """Is the classic Python engine explicitly requested?

    * ``HOLAF_USE_PYTHON=1`` (preferred, current name), or
    * ``HOLAF_USE_RUST=0`` (legacy opt-in spelling from J4, kept for the
      scripts/documentation written while Rust mode was opt-in).

    The Rust engine is the default, so ``HOLAF_USE_RUST=1`` is now redundant
    and simply means "default" (a failed daemon still falls back rather than
    aborting startup).
    """
    if _env_flag(ENV_PYTHON_ENGINE):
        return True
    legacy = os.environ.get(ENV_RUST_ENGINE, "").strip().lower()
    return legacy in _FALSY


def create_audio_engine():
    """Instantiate the audio backend: Rust daemon by default, Python on demand.

    Default path
        `rust_launcher.ensure_daemon_running()` locates `holafd`, starts it
        with ``--backend pipewire`` (override: ``$HOLAFD_BACKEND``) unless one
        is already answering on ``$HOLAF_SOCKET`` / ``/tmp/holaf-$USER.sock``,
        then builds an :class:`IpcAudioEngine`.  The returned engine carries
        ``_rust_daemon_spawned_by_us`` (True only when *this* process launched
        the daemon) so ``main()`` stops it on exit without killing one the user
        started manually before launch.

    Opt-out
        ``HOLAF_USE_PYTHON=1`` (or ``HOLAF_USE_RUST=0``) returns the classic
        :class:`AudioEngine` and never touches the Rust daemon.

    Fallback (never silent)
        Any failure on the Rust path — binary not found, daemon not answering,
        IPC client import error — is logged at WARNING level and the classic
        :class:`AudioEngine` is returned instead.  The returned engine then
        carries ``_rust_fallback_reason`` (a human-readable string) which
        ``main()`` surfaces in the window's status bar.

    NOTE: main_window.py also calls pipewire_utils.* directly for device/app
    discovery and app routing; wiring those through the daemon is a later
    milestone, so the Rust backend currently serves the engine state only.
    """
    if python_engine_forced():
        logger.info(
            "classic Python engine selected (%s=1 or %s=0)",
            ENV_PYTHON_ENGINE,
            ENV_RUST_ENGINE,
        )
        return _create_python_engine()

    try:
        from src.backend.ipc_audio_engine import IpcAudioEngine
        from src.backend.rust_launcher import ensure_daemon_running

        daemon = ensure_daemon_running()
        if not daemon.ok:
            raise RuntimeError(
                daemon.reason
                or "the Rust daemon (holafd) is not answering on "
                + _socket_path_for_message()
            )
        engine = IpcAudioEngine()
        # Ownership flag for the app-quit teardown (see shutdown_all in
        # main()): stop the daemon only when we launched it, never one that
        # was already running (launched manually by the user / a previous
        # session) before this app started.
        engine._rust_daemon_spawned_by_us = daemon.spawned_by_us
        engine._rust_fallback_reason = None
        logger.info(
            "Rust engine active (daemon %s)",
            "spawned by us" if daemon.spawned_by_us else "already running",
        )
        return engine
    except Exception as exc:  # noqa: BLE001 - any failure must fall back
        reason = str(exc) or exc.__class__.__name__
        logger.warning(
            "Rust backend unavailable (%s) — falling back to the classic "
            "Python engine. Set HOLAF_USE_PYTHON=1 to silence this notice.",
            reason,
        )
        engine = _create_python_engine()
        engine._rust_fallback_reason = reason
        return engine


def _socket_path_for_message() -> str:
    """Best-effort socket path for error messages (never raises)."""
    try:
        from src.backend.ipc_audio_engine import default_socket_path
        return default_socket_path()
    except Exception:  # noqa: BLE001 - diagnostics only
        return "<unknown socket>"


def _create_python_engine():
    """Build the classic Python engine (direct PipeWire, no Rust daemon)."""
    from src.backend.audio_engine import AudioEngine
    return AudioEngine()


def _show_fallback_notice(window, reason: str) -> None:
    """Tell the user (discreetly) that the Rust backend could not be used.

    Least intrusive but visible: a persistent message in the QMainWindow
    status bar for :data:`FALLBACK_NOTICE_TIMEOUT_MS` — no modal dialog to
    dismiss, no extra click before mixing.  Never raises: a UI failure here
    must not abort startup.
    """
    message = (
        "Rust audio backend unavailable — running the classic Python engine. "
        f"Reason: {reason}"
    )
    try:
        window.statusBar().showMessage(message, FALLBACK_NOTICE_TIMEOUT_MS)
    except Exception:  # noqa: BLE001 - notice is best effort
        logger.debug("could not show the fallback notice in the status bar")
    # The degrading itself was already logged at WARNING by
    # create_audio_engine(); this only records that the user was told.
    logger.info("%s", message)


def make_shutdown_all(audio, midi):
    """Build the **idempotent** application teardown.

    The returned callable is used by both exit paths:

    * ``QApplication.aboutToQuit`` — clean window close / ``app.quit()``;
    * the POSIX signal handlers installed by
      :class:`src.backend.signal_shutdown.ShutdownController` (SIGTERM,
      SIGINT, SIGHUP) — a session logout, a closed terminal, ``kill``…

    so it must run at most once, and it must be safe to re-enter from a signal
    handler that interrupts the main thread mid-teardown: the guard lock is
    taken *without blocking*, so a nested call returns immediately instead of
    deadlocking (which would orphan the daemon).

    The Rust daemon is stopped only when this process spawned it
    (``_rust_daemon_spawned_by_us``); a daemon the user started manually is
    deliberately left running.  A Python-engine fallback has no such attribute,
    so nothing is ever stopped there.
    """
    lock = threading.Lock()
    state = {"done": False}

    def shutdown_all():
        if not lock.acquire(blocking=False):
            logger.info("shutdown already in progress — ignoring the extra call")
            return
        try:
            if state["done"]:
                logger.debug("shutdown already completed — ignoring the extra call")
                return
            state["done"] = True
        finally:
            lock.release()

        print("Application stopping...")
        try:
            audio.shutdown()
        except Exception:  # noqa: BLE001 - a broken engine must not block the daemon stop
            logger.exception("audio engine shutdown failed")
        try:
            midi.close_port()
        except Exception:  # noqa: BLE001 - idem for MIDI
            logger.exception("MIDI port close failed")
        if getattr(audio, "_rust_daemon_spawned_by_us", False):
            from src.backend.rust_launcher import stop_daemon
            try:
                stop_daemon()
            except Exception:  # noqa: BLE001 - exit path: never crash here
                logging.exception(
                    "Failed to stop the Rust daemon on exit"
                )

    return shutdown_all


def main():
    # Qt / mido are imported here (not at module level) so create_audio_engine()
    # stays importable without PySide6 (headless tests, CLI tools).
    from PySide6.QtWidgets import QApplication

    from src.backend.midi_engine import MidiEngine
    from src.ui.main_window import MainWindow

    try:
        app = QApplication(sys.argv)

        # 1. Initialize Backend Engines
        audio = create_audio_engine()
        midi = MidiEngine()

        # 2. Initialize UI with Backend references
        window = MainWindow(audio_engine=audio, midi_engine=midi)
        window.show()

        # 2b. Surface a Rust→Python downgrade (the factory already logged it).
        fallback_reason = getattr(audio, "_rust_fallback_reason", None)
        if fallback_reason:
            _show_fallback_notice(window, fallback_reason)

        # 3. Start Audio Engine with loaded configuration
        audio.start_engine(window.strips)

        # 4. Connect Shutdown Signal (clean Qt quit: window closed, app.quit())
        shutdown_all = make_shutdown_all(audio, midi)

        app.aboutToQuit.connect(shutdown_all)

        # 5. POSIX signals (SIGTERM / SIGINT / SIGHUP).
        # ``aboutToQuit`` is a Qt signal: it only fires when Qt itself decides
        # the app is done.  A session logout (SIGTERM), a closed terminal
        # (SIGHUP) or Ctrl-C (SIGINT) bypasses Qt entirely, which used to leave
        # the holafd daemon orphaned.  The controller runs the same teardown
        # from the signal handler, guards it with a watchdog thread, and lets
        # the kernel (PR_SET_PDEATHSIG on the daemon) cover SIGKILL.
        from src.backend.signal_shutdown import ShutdownController

        def _hard_stop_daemon():
            """Lock-free last resort: force-stop the daemon we spawned."""
            from src.backend.rust_launcher import kill_daemon_now

            kill_daemon_now()

        shutdown_controller = ShutdownController(
            cleanup=shutdown_all,
            hard_cleanup=_hard_stop_daemon,
            quit_ui=app.quit,
        )
        shutdown_controller.install()

        try:
            sys.exit(app.exec())
        finally:
            shutdown_controller.restore()
    except Exception as e:
        # A crash during startup (e.g. missing PipeWire daemon, broken
        # config, no display) previously left a raw traceback with no
        # context. Log it and surface a user-visible message when possible.
        logging.exception("Fatal startup error")
        try:
            from PySide6.QtWidgets import QMessageBox
            QMessageBox.critical(
                None,
                "Holaf-Mix — Startup Error",
                f"The application failed to start:\n\n{e}\n\nSee logs for details.",
            )
        except Exception:
            # QMessageBox may fail (no display) — fall back to stderr.
            print(f"Fatal startup error: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
