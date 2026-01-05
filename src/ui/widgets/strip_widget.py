from PySide6.QtWidgets import (QFrame, QVBoxLayout, QHBoxLayout, QPushButton, 
                               QSlider, QLabel, QWidget, QMenu)
from PySide6.QtCore import Qt, Signal, QTimer
from PySide6.QtGui import QAction
from src.models.strip_model import StripType

class StripWidget(QFrame):
    """
    A specific widget representing one audio strip (Input or Output).
    Includes throttling logic for the volume slider to prevent UI freeze.
    """
    # Signals to notify the main window
    volume_changed = Signal(str, float) # uid, new_volume
    mute_changed = Signal(str, bool)    # uid, new_mute_state
    delete_requested = Signal(str)      # uid
    route_changed = Signal(str, str, bool) # source_uid, target_uid, is_active
    midi_learn_requested = Signal(str, str) # uid, property ("volume" or "mute")
    
    def __init__(self, strip_model, parent=None):
        super().__init__(parent)
        self.strip = strip_model
        self._is_learning = False
        
        # --- Throttling Mechanism ---
        # We store the last volume sent to the backend
        self.last_sent_vol = self.strip.volume
        # Timer will check every 100ms if volume changed significantly
        self.update_timer = QTimer(self)
        self.update_timer.setInterval(100) # 10Hz limit
        self.update_timer.timeout.connect(self._check_and_send_volume)
        self.update_timer.start()

        # Visual Setup
        self.setFixedWidth(100)
        self.setFrameShape(QFrame.StyledPanel)
        self._update_base_style()
        self._init_ui()

    def _update_base_style(self):
        if self._is_learning:
            bg_color = "#f39c12" # Orange for Learning mode
        elif self.strip.kind == StripType.INPUT:
            bg_color = "#3daee9" 
        else:
            bg_color = "#e93d3d" 
            
        self.setStyleSheet(f"""
            StripWidget {{
                background-color: qlineargradient(x1:0, y1:0, x2:0, y2:1, stop:0 #444, stop:1 {bg_color});
                border: 2px solid {"#f39c12" if self._is_learning else "#555"};
                border-radius: 5px;
            }}
            QLabel {{ color: white; background: transparent; }}
        """)

    def _init_ui(self):
        layout = QVBoxLayout(self)
        layout.setSpacing(5)
        layout.setContentsMargins(5, 5, 5, 10)

        # --- 1. Header (Title + Delete Button) ---
        header_layout = QHBoxLayout()
        header_layout.setSpacing(0)
        
        self.lbl_name = QLabel(self.strip.label)
        self.lbl_name.setAlignment(Qt.AlignCenter)
        self.lbl_name.setStyleSheet("font-weight: bold; font-size: 11px;")
        
        btn_delete = QPushButton("×")
        btn_delete.setFixedSize(16, 16)
        btn_delete.setCursor(Qt.PointingHandCursor)
        btn_delete.setStyleSheet("""
            QPushButton { background: transparent; color: #888; border: none; font-weight: bold; font-size: 14px; }
            QPushButton:hover { color: #ff5555; }
        """)
        btn_delete.setToolTip("Remove this strip")
        btn_delete.clicked.connect(self._on_delete_clicked)

        header_layout.addWidget(self.lbl_name)
        header_layout.addWidget(btn_delete)
        layout.addLayout(header_layout)

        # --- 2. Routing Area ---
        self.routing_container = QFrame()
        self.routing_container.setStyleSheet("background-color: rgba(0,0,0,0.3); border-radius: 3px;")
        self.routing_layout = QVBoxLayout(self.routing_container)
        self.routing_layout.setContentsMargins(2, 2, 2, 2)
        self.routing_layout.setSpacing(2)
        
        if self.strip.kind == StripType.INPUT:
            self.lbl_no_route = QLabel("No Outputs")
            self.lbl_no_route.setStyleSheet("font-size: 9px; color: #aaa;")
            self.lbl_no_route.setAlignment(Qt.AlignCenter)
            self.routing_layout.addWidget(self.lbl_no_route)
            layout.addWidget(self.routing_container)
        else:
            lbl_out = QLabel("BUS MASTER")
            lbl_out.setStyleSheet("font-size: 9px; color: #aaa; margin: 10px 0;")
            lbl_out.setAlignment(Qt.AlignCenter)
            layout.addWidget(lbl_out)

        # --- 3. Volume Fader ---
        slider_layout = QHBoxLayout()
        self.slider = QSlider(Qt.Vertical)
        self.slider.setRange(0, 100)
        self.slider.setValue(int(self.strip.volume * 100))
        self.slider.setStyleSheet("""
            QSlider::groove:vertical {
                background: #222;
                width: 6px;
                border-radius: 2px;
            }
            QSlider::handle:vertical {
                background: white;
                height: 14px;
                margin: 0 -4px;
                border-radius: 3px;
            }
            QSlider::add-page:vertical { background: #555; }
            QSlider::sub-page:vertical { background: #222; }
        """)
        # Connect only local model update (Timer handles the rest)
        self.slider.valueChanged.connect(self._on_slider_move)
        
        slider_layout.addStretch()
        slider_layout.addWidget(self.slider)
        slider_layout.addStretch()
        layout.addLayout(slider_layout)

        # --- 4. Mute Button ---
        self.btn_mute = QPushButton("MUTE")
        self.btn_mute.setCheckable(True)
        self.btn_mute.setChecked(self.strip.mute)
        self.btn_mute.toggled.connect(self._on_mute_toggle)
        self._update_mute_style()
        layout.addWidget(self.btn_mute)

        # --- 5. MIDI Config ---
        self.btn_midi = QPushButton("MIDI")
        self.btn_midi.setFixedHeight(20)
        self.btn_midi.setCursor(Qt.PointingHandCursor)
        self.btn_midi.setStyleSheet("""
            QPushButton { background-color: #333; color: #888; border: none; font-size: 10px;}
            QPushButton:hover { background-color: #444; color: white; }
        """)
        self.btn_midi.clicked.connect(self._show_midi_menu)
        layout.addWidget(self.btn_midi)

    def _show_midi_menu(self):
        menu = QMenu(self)
        
        act_vol = QAction("Learn Volume", self)
        act_vol.triggered.connect(lambda: self.midi_learn_requested.emit(self.strip.uid, "volume"))
        
        act_mute = QAction("Learn Mute", self)
        act_mute.triggered.connect(lambda: self.midi_learn_requested.emit(self.strip.uid, "mute"))
        
        act_clear = QAction("Clear Mappings", self)
        act_clear.triggered.connect(self._clear_midi)

        menu.addAction(act_vol)
        menu.addAction(act_mute)
        menu.addSeparator()
        menu.addAction(act_clear)
        menu.exec(self.btn_midi.mapToGlobal(self.btn_midi.rect().bottomLeft()))

    def set_learning(self, active: bool):
        self._is_learning = active
        self._update_base_style()
        if active:
            self.btn_midi.setText("LEARNING...")
            self.btn_midi.setStyleSheet("background-color: #f39c12; color: black; font-weight: bold; font-size: 10px;")
        else:
            self.btn_midi.setText("MIDI")
            self.btn_midi.setStyleSheet("background-color: #333; color: #888; border: none; font-size: 10px;")

    def _clear_midi(self):
        self.strip.midi_volume = None
        self.strip.midi_mute = None
        # Signal fake change to force save
        self.mute_changed.emit(self.strip.uid, self.strip.mute)

    def set_routing_targets(self, output_strips):
        if self.strip.kind != StripType.INPUT:
            return

        while self.routing_layout.count():
            item = self.routing_layout.takeAt(0)
            if item.widget():
                item.widget().deleteLater()

        if not output_strips:
            self.routing_layout.addWidget(self.lbl_no_route)
            return

        for out_strip in output_strips:
            btn = QPushButton(out_strip.label[:4].upper())
            btn.setCheckable(True)
            btn.setToolTip(f"Send to {out_strip.label}")
            btn.setFixedHeight(20)
            
            is_active = out_strip.uid in self.strip.routes
            btn.setChecked(is_active)
            
            # STYLING RESTORED
            btn.setStyleSheet(f"""
                QPushButton {{ 
                    background-color: #333; 
                    color: #888; 
                    border: 1px solid #444; 
                    border-radius: 3px; 
                    font-size: 9px;
                }}
                QPushButton:checked {{ 
                    background-color: #4caf50; 
                    color: white; 
                    border: 1px solid #4caf50; 
                }}
                QPushButton:hover:!checked {{ background-color: #444; color: white; }}
            """)
            
            btn.clicked.connect(lambda checked, uid=out_strip.uid: self._on_route_toggled(uid, checked))
            self.routing_layout.addWidget(btn)

    def _on_route_toggled(self, target_uid, checked):
        if checked:
            if target_uid not in self.strip.routes:
                self.strip.routes.append(target_uid)
        else:
            if target_uid in self.strip.routes:
                self.strip.routes.remove(target_uid)
        
        self.route_changed.emit(self.strip.uid, target_uid, checked)

    def _on_slider_move(self, val):
        # Update Model + Visuals ONLY. No backend signal emitted here.
        self.strip.volume = val / 100.0

    def update_ui_from_model(self):
        """
        Called when the model changes externally (e.g. MIDI).
        We block signals to prevent a feedback loop (Slider -> Model -> UI -> Slider).
        """
        self.slider.blockSignals(True)
        self.slider.setValue(int(self.strip.volume * 100))
        self.slider.blockSignals(False)
        
        self.btn_mute.blockSignals(True)
        self.btn_mute.setChecked(self.strip.mute)
        self.btn_mute.blockSignals(False)
        self._update_mute_style()

    def _check_and_send_volume(self):
        """Executed by QTimer at 10Hz to throttle backend calls"""
        current_vol = round(self.strip.volume, 2)
        # Only emit if the model volume is different from what we last sent
        if current_vol != round(self.last_sent_vol, 2):
            self.volume_changed.emit(self.strip.uid, self.strip.volume)
            self.last_sent_vol = self.strip.volume

    def _on_mute_toggle(self, checked):
        self.strip.mute = checked
        self._update_mute_style()
        self.mute_changed.emit(self.strip.uid, checked)

    def _update_mute_style(self):
        if self.btn_mute.isChecked():
            self.btn_mute.setStyleSheet("background-color: #ff4444; color: white; font-weight: bold; border: none; padding: 5px;")
            self.btn_mute.setText("MUTED")
        else:
            self.btn_mute.setStyleSheet("background-color: #444; color: white; border: none; padding: 5px;")
            self.btn_mute.setText("MUTE")

    def _on_delete_clicked(self):
        self.delete_requested.emit(self.strip.uid)