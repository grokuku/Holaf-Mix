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
"""

import logging
import os
import sys

# Centralized logging configuration. Done here (not at module level in
# audio_engine.py) so the first-call-wins behavior of basicConfig doesn't
# make import order matter.
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
)

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

        # 4. Connect Shutdown Signal
        def shutdown_all():
            print("Application stopping...")
            audio.shutdown()
            midi.close_port()
            # Rust daemon lifecycle: now that the engine is stopped, tear down
            # the daemon — but ONLY when this process spawned it.  A daemon
            # already running before launch (started manually by the user) is
            # deliberately left alone.  After a fallback on the Python engine
            # the attribute is absent/False, so nothing is ever stopped.
            if getattr(audio, "_rust_daemon_spawned_by_us", False):
                from src.backend.rust_launcher import stop_daemon
                try:
                    stop_daemon()
                except Exception:
                    logging.exception(
                        "Failed to stop the Rust daemon on exit"
                    )

        app.aboutToQuit.connect(shutdown_all)

        sys.exit(app.exec())
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
