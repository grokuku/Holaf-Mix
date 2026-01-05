from PySide6.QtWidgets import (QMainWindow, QWidget, QVBoxLayout, QHBoxLayout, 
                               QLabel, QScrollArea, QFrame, QPushButton, QInputDialog, QMessageBox)
from PySide6.QtCore import Qt, QThreadPool, QRunnable, Slot, QTimer
from src.config import settings
from src.models.strip_model import Strip, StripType, StripMode
from src.ui.widgets.strip_widget import StripWidget

# --- Worker for ThreadPool ---
class BackendWorker(QRunnable):
    """
    Worker thread to execute backend commands without freezing the UI.
    """
    def __init__(self, func, *args, **kwargs):
        super().__init__()
        self.func = func
        self.args = args
        self.kwargs = kwargs

    @Slot()
    def run(self):
        try:
            self.func(*self.args, **self.kwargs)
        except Exception as e:
            print(f"Backend Task Error: {e}")

class MainWindow(QMainWindow):
    def __init__(self, audio_engine=None, midi_engine=None):
        super().__init__()
        self.setWindowTitle("Holaf-Mix")
        self.resize(1100, 700)
        
        # 0. Store Engines
        self.audio_engine = audio_engine
        self.midi_engine = midi_engine
        
        # Thread Pool for non-blocking backend calls
        self.thread_pool = QThreadPool()
        self.thread_pool.setMaxThreadCount(4)

        # MIDI Lookup Cache: (type, channel, identifier) -> (strip_uid, property)
        self.midi_lookup = {}
        # Widget Registry: uid -> StripWidget instance
        self.widgets = {} 

        # 1. Load Data
        self.strips = settings.load_config()
        self._rebuild_midi_lookup()

        # 2. Main Layout Setup
        self.central_widget = QWidget()
        self.setCentralWidget(self.central_widget)
        
        self.main_layout = QHBoxLayout(self.central_widget)
        self.main_layout.setSpacing(0)
        self.main_layout.setContentsMargins(0, 0, 0, 0)

        # --- Left Side: INPUTS ---
        self.inputs_container = self._create_section("INPUTS", "#2b2b2b", StripType.INPUT)
        self.main_layout.addWidget(self.inputs_container)

        # --- Right Side: OUTPUTS ---
        self.outputs_container = self._create_section("OUTPUTS", "#1e1e1e", StripType.OUTPUT)
        self.main_layout.addWidget(self.outputs_container)

        # 3. Populate UI & MIDI
        self.refresh_ui()
        self._init_midi()

    def _init_midi(self):
        """Initializes MIDI connection and signals."""
        if not self.midi_engine:
            return
        
        self.midi_engine.mapping_detected.connect(self.on_midi_mapping_detected)
        self.midi_engine.message_received.connect(self.on_midi_message_received)
        
        # Auto-connect strategy
        ports = self.midi_engine.get_input_names()
        if ports:
            # Try to find 'mix' (e.g. Akai MidiMix), otherwise take the first one
            target = next((p for p in ports if "mix" in p.lower()), ports[0])
            self.midi_engine.open_port(target)

    def _rebuild_midi_lookup(self):
        """Rebuilds the hash map for O(1) MIDI message processing."""
        self.midi_lookup.clear()
        for strip in self.strips:
            # Volume Mappings
            if strip.midi_volume:
                m = strip.midi_volume
                key = (m['type'], m['channel'], m.get('control') or m.get('note'))
                self.midi_lookup[key] = (strip.uid, "volume")
            # Mute Mappings
            if strip.midi_mute:
                m = strip.midi_mute
                key = (m['type'], m['channel'], m.get('control') or m.get('note'))
                self.midi_lookup[key] = (strip.uid, "mute")

    # --- MIDI Logic ---

    def on_midi_message_received(self, msg):
        """
        CRITICAL: This runs in the MIDI thread context usually.
        We update the Model immediately, but we DO NOT call AudioEngine directly.
        We ask the UI to update, and the UI Widget's timer will handle the AudioEngine throttling.
        """
        identifier = getattr(msg, 'control', None) if msg.type == 'control_change' else getattr(msg, 'note', None)
        if identifier is None:
            return

        key = (msg.type, msg.channel, identifier)
        if key in self.midi_lookup:
            uid, prop = self.midi_lookup[key]
            strip = next((s for s in self.strips if s.uid == uid), None)
            
            if not strip:
                return

            # Update Model
            if prop == "volume":
                # Convert MIDI 0-127 to Float 0.0-1.0
                strip.volume = msg.value / 127.0
            elif prop == "mute":
                # Toggle logic for buttons
                val = getattr(msg, 'velocity', 127) if hasattr(msg, 'velocity') else msg.value
                if val > 0: # Press event
                    strip.mute = not strip.mute
            
            # Trigger UI Update (Safe Threading)
            widget = self.widgets.get(uid)
            if widget:
                # Use SingleShot to push the update to the Main UI Thread
                QTimer.singleShot(0, widget.update_ui_from_model)

    def on_midi_mapping_detected(self, uid, prop, mapping):
        # 1. Update Model and Save
        strip = next((s for s in self.strips if s.uid == uid), None)
        if strip:
            if prop == "volume":
                strip.midi_volume = mapping
            else:
                strip.midi_mute = mapping
            settings.save_config(self.strips)
        
        # 2. Reset visual state on widget
        widget = self.widgets.get(uid)
        if widget:
            widget.set_learning(False)
        
        # 3. Refresh lookup table
        self._rebuild_midi_lookup()

    def on_midi_learn_requested(self, uid, prop):
        if self.midi_engine:
            widget = self.widgets.get(uid)
            if widget:
                widget.set_learning(True)
            self.midi_engine.start_learning(uid, prop)

    # --- UI Construction ---

    def _create_section(self, title, bg_color, strip_kind):
        """Helper to create a column with a Header (+ button) and a ScrollArea."""
        container = QWidget()
        container.setStyleSheet(f"background-color: {bg_color};")
        
        layout = QVBoxLayout(container)
        
        # --- Header ---
        header_widget = QWidget()
        header_layout = QHBoxLayout(header_widget)
        header_layout.setContentsMargins(10, 10, 10, 0)
        
        label = QLabel(title)
        label.setStyleSheet("font-weight: bold; color: white; font-size: 14px;")
        
        btn_add = QPushButton("+")
        btn_add.setFixedSize(30, 30)
        btn_add.setCursor(Qt.PointingHandCursor)
        # STYLING RESTORED
        btn_add.setStyleSheet("""
            QPushButton { background-color: #444; color: white; border-radius: 15px; font-weight: bold; }
            QPushButton:hover { background-color: #666; }
        """)
        btn_add.clicked.connect(lambda: self.on_add_clicked(strip_kind))

        header_layout.addWidget(label)
        header_layout.addStretch()
        header_layout.addWidget(btn_add)
        
        layout.addWidget(header_widget)

        # --- Scroll Area ---
        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setFrameShape(QFrame.NoFrame)
        scroll.setStyleSheet("background: transparent;")
        
        content_widget = QWidget()
        content_layout = QHBoxLayout(content_widget)
        content_layout.setAlignment(Qt.AlignTop | Qt.AlignLeft)
        content_layout.setSpacing(10)
        content_widget.setLayout(content_layout)
        
        scroll.setWidget(content_widget)
        layout.addWidget(scroll)

        container.content_layout = content_layout 
        return container

    def refresh_ui(self):
        """Clears and rebuilds the strips based on self.strips data."""
        self._clear_layout(self.inputs_container.content_layout)
        self._clear_layout(self.outputs_container.content_layout)
        self.widgets.clear()
        
        # Identify Outputs for routing
        output_strips = [s for s in self.strips if s.kind == StripType.OUTPUT]

        for strip in self.strips:
            widget = StripWidget(strip)
            self.widgets[strip.uid] = widget
            
            # Connect Signals
            widget.volume_changed.connect(self.on_strip_volume_changed)
            widget.mute_changed.connect(self.on_strip_mute_changed)
            widget.delete_requested.connect(self.on_strip_delete_requested)
            widget.route_changed.connect(self.on_strip_route_changed)
            widget.midi_learn_requested.connect(self.on_midi_learn_requested)
            
            if strip.kind == StripType.INPUT:
                widget.set_routing_targets(output_strips)
                self.inputs_container.content_layout.addWidget(widget)
            else:
                self.outputs_container.content_layout.addWidget(widget)

    def _clear_layout(self, layout):
        while layout.count():
            item = layout.takeAt(0)
            if item.widget():
                item.widget().deleteLater()

    # --- Actions ---

    def on_add_clicked(self, kind):
        name, ok = QInputDialog.getText(self, "Add Strip", "Enter name for new strip:")
        if ok and name:
            new_strip = Strip(
                label=name,
                kind=kind,
                mode=StripMode.VIRTUAL 
            )
            self.strips.append(new_strip)
            settings.save_config(self.strips)
            
            # Restart engine in background
            if self.audio_engine:
                self._run_in_background(self.audio_engine.start_engine, self.strips)

            self.refresh_ui()

    def on_strip_delete_requested(self, uid):
        strip_to_remove = next((s for s in self.strips if s.uid == uid), None)
        if not strip_to_remove: return

        reply = QMessageBox.question(
            self, "Confirm Delete", 
            f"Are you sure you want to remove '{strip_to_remove.label}'?",
            QMessageBox.Yes | QMessageBox.No, QMessageBox.No
        )

        if reply == QMessageBox.Yes:
            self.strips.remove(strip_to_remove)
            
            # Clean up routes
            if strip_to_remove.kind == StripType.OUTPUT:
                for s in self.strips:
                    if s.kind == StripType.INPUT and strip_to_remove.uid in s.routes:
                        s.routes.remove(strip_to_remove.uid)

            settings.save_config(self.strips)
            
            # Restart Engine
            if self.audio_engine:
                def reload():
                    self.audio_engine.shutdown() 
                    self.audio_engine.start_engine(self.strips)
                self._run_in_background(reload)

            self.refresh_ui()

    # --- Backend Interfacing ---

    def _run_in_background(self, func, *args):
        if self.audio_engine:
            worker = BackendWorker(func, *args)
            self.thread_pool.start(worker)

    def on_strip_volume_changed(self, uid, new_vol):
        # Triggered by the Widget's Throttled Timer
        self._run_in_background(self.audio_engine.set_volume, uid, new_vol)

    def on_strip_mute_changed(self, uid, is_muted):
        # Triggered directly (buttons don't need throttling usually)
        self._run_in_background(self.audio_engine.set_mute, uid, is_muted)
        settings.save_config(self.strips)

    def on_strip_route_changed(self, source_uid, target_uid, active):
        self._run_in_background(self.audio_engine.update_routing, source_uid, target_uid, active)
        settings.save_config(self.strips)