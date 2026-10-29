import os
import sys
import logging
from PySide6.QtWidgets import QApplication
from src.ui.main_window import MainWindow
from src.backend.midi_engine import MidiEngine

# Centralized logging configuration. Done here (not at module level in
# audio_engine.py) so the first-call-wins behavior of basicConfig doesn't
# make import order matter.
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
)

def create_audio_engine():
    """Instantiate the audio backend.

    Default: the classic Python AudioEngine (drives PipeWire directly via
    pw-cli/pactl) — unchanged behaviour.

    Rust daemon mode (J4, experimental): set the env var
    ``HOLAF_USE_RUST=1`` to use IpcAudioEngine (src/backend/ipc_audio_engine.py)
    which talks NDJSON to the Rust `holafd` daemon over a Unix socket.  The
    daemon is started on demand by rust_launcher.ensure_daemon_running() unless
    one is already running (see $HOLAF_SOCKET / $HOLAFD_BIN / $HOLAFD_BACKEND).
    The returned engine carries ``_rust_daemon_spawned_by_us`` (True only when
    *this* process launched the daemon) so ``main()`` can stop the daemon on
    exit without killing one the user started manually before launch.

    NOTE: main_window.py also calls pipewire_utils.* directly for device/app
    discovery and app routing; wiring those through the daemon is a later
    milestone, so Rust mode currently serves the engine state only.
    """
    if os.environ.get("HOLAF_USE_RUST") == "1":
        from src.backend.ipc_audio_engine import IpcAudioEngine
        from src.backend.rust_launcher import ensure_daemon_running

        daemon = ensure_daemon_running()
        if not daemon.ok:
            raise RuntimeError(
                "HOLAF_USE_RUST=1 but the Rust daemon (holafd) could not be "
                "started — is it built in rust/target and is the socket free?"
            )
        engine = IpcAudioEngine()
        # Ownership flag for the app-quit teardown (see shutdown_all in
        # main()): stop the daemon only when we launched it, never one that
        # was already running (launched manually by the user / a previous
        # session) before this app started.
        engine._rust_daemon_spawned_by_us = daemon.spawned_by_us
        return engine

    # Default path: classic Python engine (no Rust daemon involved).
    from src.backend.audio_engine import AudioEngine
    return AudioEngine()


def main():
    try:
        app = QApplication(sys.argv)

        # 1. Initialize Backend Engines
        audio = create_audio_engine()
        midi = MidiEngine()

        # 2. Initialize UI with Backend references
        window = MainWindow(audio_engine=audio, midi_engine=midi)
        window.show()

        # 3. Start Audio Engine with loaded configuration
        audio.start_engine(window.strips)

        # 4. Connect Shutdown Signal
        def shutdown_all():
            print("Application stopping...")
            audio.shutdown()
            midi.close_port()
            # Rust daemon lifecycle (HOLAF_USE_RUST=1): now that the engine is
            # stopped, tear down the daemon — but ONLY when this process
            # spawned it.  A daemon already running before launch (started
            # manually by the user) is deliberately left alone.
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