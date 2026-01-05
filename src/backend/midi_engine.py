import mido
import threading
import logging
from PySide6.QtCore import QObject, Signal

logger = logging.getLogger("MidiEngine")

class MidiEngine(QObject):
    """
    Handles MIDI input, mapping detection (Learn mode), and message processing.
    Runs a background thread to listen to mido ports without blocking the UI.
    """
    # Signals to communicate with the UI
    message_received = Signal(object)  # Emitted for every message
    mapping_detected = Signal(str, str, dict) # strip_uid, property_name, mapping_data

    def __init__(self):
        super().__init__()
        self.inport = None
        self.listening = False
        self.thread = None
        
        # Learning state
        self.learning_mode = False
        self.learning_context = {"uid": None, "property": None} 

    def get_input_names(self):
        try:
            return mido.get_input_names()
        except Exception as e:
            logger.error(f"Error listing MIDI ports: {e}")
            return []

    def open_port(self, port_name):
        self.close_port()
        try:
            self.inport = mido.open_input(port_name)
            self.listening = True
            self.thread = threading.Thread(target=self._listen_loop, daemon=True)
            self.thread.start()
            logger.info(f"Opened MIDI port: {port_name}")
            return True
        except Exception as e:
            logger.error(f"Failed to open MIDI port {port_name}: {e}")
            return False

    def close_port(self):
        self.listening = False
        if self.inport:
            self.inport.close()
            self.inport = None
        if self.thread:
            # We don't strictly join here to avoid UI hang if thread is blocked,
            # but daemon=True handles cleanup on exit.
            pass

    def start_learning(self, strip_uid, property_name):
        """
        Enters learning mode for a specific strip and property.
        """
        self.learning_mode = True
        self.learning_context = {"uid": strip_uid, "property": property_name}
        logger.info(f"MIDI Learning started for {strip_uid} - {property_name}")

    def _listen_loop(self):
        """Background thread listening for MIDI messages."""
        while self.listening and self.inport:
            try:
                # Iterate pending messages to drain buffer
                for msg in self.inport.iter_pending():
                    if self.learning_mode:
                        self._process_learn(msg)
                    else:
                        self.message_received.emit(msg)
                
                # Sleep briefly to prevent CPU hogging (100% usage fix)
                threading.Event().wait(0.005) 
            except Exception as e:
                logger.error(f"Error in MIDI loop: {e}")
                break

    def _process_learn(self, msg):
        """Captures the first relevant message during learning mode."""
        mapping = None
        
        # Filter unwanted messages (clock, active sensing, etc if needed)
        if msg.type == 'clock' or msg.type == 'active_sensing':
            return

        if msg.type == 'control_change':
            mapping = {
                'type': 'control_change',
                'channel': msg.channel,
                'control': msg.control
            }
        elif msg.type in ['note_on', 'note_off']:
            # We map the Note Number. Velocity doesn't matter for the mapping key.
            mapping = {
                'type': 'note_on',
                'channel': msg.channel,
                'note': msg.note
            }

        if mapping:
            uid = self.learning_context["uid"]
            prop = self.learning_context["property"]
            # Reset learn mode immediately
            self.learning_mode = False
            self.mapping_detected.emit(uid, prop, mapping)
            logger.info(f"Mapping detected: {prop} -> {mapping}")