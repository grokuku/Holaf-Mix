import sys
import logging
from PySide6.QtWidgets import QApplication
from src.ui.main_window import MainWindow
from src.backend.audio_engine import AudioEngine
from src.backend.midi_engine import MidiEngine

# Centralized logging configuration. Done here (not at module level in
# audio_engine.py) so the first-call-wins behavior of basicConfig doesn't
# make import order matter.
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
)

def main():
    try:
        app = QApplication(sys.argv)

        # 1. Initialize Backend Engines
        audio = AudioEngine()
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