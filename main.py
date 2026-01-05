import sys
from PySide6.QtWidgets import QApplication
from src.ui.main_window import MainWindow
from src.backend.audio_engine import AudioEngine
from src.backend.midi_engine import MidiEngine

def main():
    app = QApplication(sys.argv)
    
    # 1. Initialize Backend Engines
    # Audio controls PipeWire
    audio = AudioEngine()
    # MIDI listens to hardware
    midi = MidiEngine()
    
    # 2. Initialize UI with Backend references
    window = MainWindow(audio_engine=audio, midi_engine=midi)
    window.show()
    
    # 3. Start Audio Engine with loaded configuration
    # The window loads the strips in its __init__, so we access them here
    audio.start_engine(window.strips)
    
    # 4. Connect Shutdown Signal
    def shutdown_all():
        print("Application stopping...")
        audio.shutdown()
        midi.close_port()
        
    app.aboutToQuit.connect(shutdown_all)
    
    sys.exit(app.exec())

if __name__ == "__main__":
    main()