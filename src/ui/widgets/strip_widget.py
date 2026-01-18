from PySide6.QtWidgets import (QFrame, QVBoxLayout, QHBoxLayout, QPushButton, 
                                QSlider, QLabel, QWidget, QMenu, QInputDialog, QComboBox, QCheckBox, QSizePolicy)
from PySide6.QtCore import Qt, Signal, QTimer, QEvent, QRect
from PySide6.QtGui import QAction, QPainter, QColor, QLinearGradient, QBrush
from src.models.strip_model import StripType, StripMode
# NEW IMPORT
from src.ui.dialogs.effect_settings_dialog import EffectSettingsDialog

class VUMeterWidget(QWidget):
    """
    Vertical bar displaying audio level.
    """
    def __init__(self, parent=None):
        super().__init__(parent)
        self.setFixedWidth(6)  # Thin bar
        self.setSizePolicy(QSizePolicy.Fixed, QSizePolicy.Expanding)
        self.level = 0.0  # 0.0 to 1.0
        
        # Colors
        self.bg_color = QColor("#222")
        self.gradient = QLinearGradient(0, 0, 0, 1) # Coordinates will be updated in paintEvent
        self.gradient.setColorAt(0.0, QColor("#ff3333")) # Red (Top)
        self.gradient.setColorAt(0.2, QColor("#ffff33")) # Yellow
        self.gradient.setColorAt(1.0, QColor("#33ff33")) # Green (Bottom)

    def set_level(self, val):
        self.level = max(0.0, min(1.0, val))
        self.update() # Trigger repaint

    def paintEvent(self, event):
        painter = QPainter(self)
        rect = self.rect()
        
        # Draw Background
        painter.fillRect(rect, self.bg_color)
        
        # Calculate filled height based on level
        fill_height = int(rect.height() * self.level)
        if fill_height > 0:
            # Draw form bottom to top
            fill_rect = QRect(0, rect.height() - fill_height, rect.width(), fill_height)
            
            # Update gradient vector to match current widget height
            self.gradient.setFinalStop(0, rect.height())
            self.gradient.setStart(0, 0)
            
            painter.fillRect(fill_rect, QBrush(self.gradient))

class StripWidget(QFrame):
    """
    A specific widget representing one audio strip (Input or Output).
    """
    # Signals to notify the main window
    volume_changed = Signal(str, float) # uid, new_volume
    mute_changed = Signal(str, bool)    # uid, new_mute_state
    mono_changed = Signal(str, bool)    # uid, new_mono_state
    label_changed = Signal(str, str)    # uid, new_label
    delete_requested = Signal(str)      # uid
    route_changed = Signal(str, str, bool) # source_uid, target_uid, is_active
    midi_learn_requested = Signal(str, str) # uid, property ("volume", "mute", "mono")
    device_changed = Signal(str, str)   # uid, device_name (for Output/Input)
    app_selection_requested = Signal(str) # uid, requests app dialog
    default_changed = Signal(str, bool) # uid, is_default
    effect_toggled = Signal(str, str, bool) # uid, effect_name, is_active
    effect_params_changed = Signal(str, str) # uid, effect_name (implies params updated in model)
    
    def __init__(self, strip_model, parent=None):
        super().__init__(parent)
        self.strip = strip_model
        self._is_learning = False
        
        # --- Throttling Mechanism ---
        self.last_sent_vol = self.strip.volume
        self.update_timer = QTimer(self)
        self.update_timer.setInterval(50) # 20Hz limit (Requested change)
        self.update_timer.timeout.connect(self._check_and_send_volume)
        self.update_timer.start()

        # Visual Setup
        self.setFixedWidth(100)
        self.setFrameShape(QFrame.StyledPanel)
        self._update_base_style()
        self._init_ui()

    def _update_base_style(self):
        if self._is_learning:
            bg_color = "#f39c12" # Orange
        elif self.strip.kind == StripType.INPUT:
            bg_color = "#3daee9" # Blue
        elif self.strip.kind == StripType.OUTPUT:
            if self.strip.device_name is None:
                # VIRTUAL OUTPUT (BUS) -> Purple
                bg_color = "#9b59b6" 
            else:
                # PHYSICAL OUTPUT -> Red
                bg_color = "#e93d3d" 
            
        self.setStyleSheet(f"""
            StripWidget {{
                background-color: qlineargradient(x1:0, y1:0, x2:0, y2:1, stop:0 #444, stop:1 {bg_color});
                border: 2px solid {"#f39c12" if self._is_learning else "#555"};
                border-radius: 5px;
            }}
            QLabel {{ color: white; background: transparent; }}
            QCheckBox {{ color: #ccc; font-size: 9px; spacing: 4px; }}
            QCheckBox::indicator {{ width: 10px; height: 10px; }}
        """)

    def _init_ui(self):
        layout = QVBoxLayout(self)
        layout.setSpacing(5)
        layout.setContentsMargins(5, 5, 5, 10)

        # --- 1. Header ---
        header_layout = QHBoxLayout()
        header_layout.setSpacing(0)
        
        self.lbl_name = QLabel(self.strip.label)
        self.lbl_name.setAlignment(Qt.AlignCenter)
        self.lbl_name.setStyleSheet("font-weight: bold; font-size: 11px;")
        self.lbl_name.setToolTip("Double-click or Right-click to rename")
        self.lbl_name.setContextMenuPolicy(Qt.CustomContextMenu)
        self.lbl_name.customContextMenuRequested.connect(self._on_label_context_menu)
        self.lbl_name.installEventFilter(self)
        
        btn_delete = QPushButton("×")
        btn_delete.setFixedSize(16, 16)
        btn_delete.setCursor(Qt.PointingHandCursor)
        btn_delete.setStyleSheet("""
            QPushButton { background: transparent; color: #888; border: none; font-weight: bold; font-size: 14px; }
            QPushButton:hover { color: #ff5555; }
        """)
        btn_delete.clicked.connect(self._on_delete_clicked)

        header_layout.addWidget(self.lbl_name)
        header_layout.addWidget(btn_delete)
        layout.addLayout(header_layout)

        # --- 2. Source / Device Selector ---
        self.device_container = QFrame()
        self.device_container.setStyleSheet("background: transparent;")
        dev_layout = QVBoxLayout(self.device_container)
        dev_layout.setContentsMargins(0, 0, 0, 0)
        dev_layout.setSpacing(2)
        layout.addWidget(self.device_container)

        if self.strip.kind == StripType.OUTPUT:
            self.lbl_dev_type = QLabel("DEVICE OUT")
            self.lbl_dev_type.setStyleSheet("font-size: 8px; color: #aaa; margin-top: 5px;")
            self.lbl_dev_type.setAlignment(Qt.AlignCenter)
            dev_layout.addWidget(self.lbl_dev_type)

            self.device_combo = QComboBox()
            self._style_combo(self.device_combo)
            self.device_combo.currentIndexChanged.connect(self._on_device_changed)
            dev_layout.addWidget(self.device_combo)
        
        else: # INPUT
            lbl_src = QLabel("SOURCE IN")
            lbl_src.setStyleSheet("font-size: 8px; color: #aaa; margin-top: 5px;")
            lbl_src.setAlignment(Qt.AlignCenter)
            dev_layout.addWidget(lbl_src)

            self.device_combo = QComboBox()
            self._style_combo(self.device_combo)
            self.device_combo.currentIndexChanged.connect(self._on_device_changed)
            dev_layout.addWidget(self.device_combo)

            # Default Checkbox
            self.cb_default = QCheckBox("Default Sink")
            self.cb_default.setChecked(self.strip.is_default)
            self.cb_default.toggled.connect(self._on_default_toggled)
            dev_layout.addWidget(self.cb_default)

            self.btn_apps = QPushButton("SELECT APPS")
            self.btn_apps.setCursor(Qt.PointingHandCursor)
            self.btn_apps.setStyleSheet("""
                QPushButton { background-color: #555; color: white; border-radius: 3px; font-size: 9px; padding: 2px; }
                QPushButton:hover { background-color: #777; }
            """)
            self.btn_apps.clicked.connect(lambda: self.app_selection_requested.emit(self.strip.uid))
            
            sp = self.btn_apps.sizePolicy()
            sp.setRetainSizeWhenHidden(True)
            self.btn_apps.setSizePolicy(sp)
            dev_layout.addWidget(self.btn_apps)
            
            # --- EFFECTS SECTION ---
            self._init_fx_section(dev_layout)

            self._update_app_btn_visibility()

        # --- 3. Routing Area ---
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

        # --- 4. Volume Fader & VU Meters ---
        fader_area_layout = QHBoxLayout()
        fader_area_layout.setSpacing(4)
        
        self.vu_left = VUMeterWidget()
        fader_area_layout.addWidget(self.vu_left)
        
        self.slider = QSlider(Qt.Vertical)
        self.slider.setRange(0, 100)
        self.slider.setValue(int(self.strip.volume * 100))
        self.slider.setStyleSheet("""
            QSlider::groove:vertical { background: #222; width: 6px; border-radius: 2px; }
            QSlider::handle:vertical { background: white; height: 14px; margin: 0 -4px; border-radius: 3px; }
            QSlider::add-page:vertical { background: #555; }
            QSlider::sub-page:vertical { background: #222; }
        """)
        self.slider.valueChanged.connect(self._on_slider_move)
        fader_area_layout.addWidget(self.slider)

        self.vu_right = VUMeterWidget()
        fader_area_layout.addWidget(self.vu_right)
        layout.addLayout(fader_area_layout)

        # --- 5. Controls (Mute & Mono) ---
        controls_layout = QHBoxLayout()
        controls_layout.setSpacing(2)
        
        self.btn_mono = QPushButton("MONO")
        self.btn_mono.setCheckable(True)
        self.btn_mono.setFixedWidth(45)
        self.btn_mono.setChecked(self.strip.is_mono)
        self.btn_mono.toggled.connect(self._on_mono_toggle)
        self._update_mono_style()
        controls_layout.addWidget(self.btn_mono)

        self.btn_mute = QPushButton("MUTE")
        self.btn_mute.setCheckable(True)
        self.btn_mute.setChecked(self.strip.mute)
        self.btn_mute.toggled.connect(self._on_mute_toggle)
        self._update_mute_style()
        controls_layout.addWidget(self.btn_mute)
        layout.addLayout(controls_layout)

        # --- 6. MIDI Config ---
        self.btn_midi = QPushButton("MIDI")
        self.btn_midi.setFixedHeight(20)
        self.btn_midi.setCursor(Qt.PointingHandCursor)
        self.btn_midi.setStyleSheet("""
            QPushButton { background-color: #333; color: #888; border: none; font-size: 10px;}
            QPushButton:hover { background-color: #444; color: white; }
        """)
        self.btn_midi.clicked.connect(self._show_midi_menu)
        layout.addWidget(self.btn_midi)
        
        self._refresh_device_ui_state()

    def _init_fx_section(self, parent_layout):
        """Initializes the FX toggle buttons for input strips."""
        fx_frame = QFrame()
        fx_layout = QHBoxLayout(fx_frame)
        fx_layout.setContentsMargins(0, 5, 0, 5)
        fx_layout.setSpacing(2)
        
        self.fx_buttons = {}
        effects = [
            ("gate", "GT", "Noise Gate"),
            ("noise_cancel", "RN", "RNNoise (IA)"),
            ("eq", "EQ", "Equalizer"),
            ("compressor", "CP", "Compressor")
        ]
        
        for key, label, tooltip in effects:
            btn = QPushButton(label)
            btn.setCheckable(True)
            btn.setFixedSize(22, 18)
            btn.setToolTip(f"{tooltip} (Right-Click to configure)")
            btn.setCursor(Qt.PointingHandCursor)
            
            # Context Menu for Settings
            btn.setContextMenuPolicy(Qt.CustomContextMenu)
            # Use lambda to capture the key for this specific button
            btn.customContextMenuRequested.connect(lambda pos, k=key: self._on_fx_context_menu(k, pos))
            
            # Initial state from model (Handling new structure)
            fx_data = self.strip.effects.get(key, {})
            # Should be a dict now, but safely handle if it was bool
            is_active = fx_data.get('active', False) if isinstance(fx_data, dict) else fx_data
            
            btn.setChecked(is_active)
            self._update_fx_button_style(btn, is_active)
            
            btn.toggled.connect(lambda checked, k=key, b=btn: self._on_fx_toggled(k, checked, b))
            
            fx_layout.addWidget(btn)
            self.fx_buttons[key] = btn
            
        parent_layout.addWidget(fx_frame)

    def _on_fx_context_menu(self, effect_key, pos):
        """Opens the configuration dialog for the effect."""
        fx_data = self.strip.effects.get(effect_key)
        if not fx_data or not isinstance(fx_data, dict):
            return # Should not happen with new model
            
        params = fx_data.get('params', {})
        if not params:
            return # No parameters to configure (e.g. maybe RNNoise)
            
        dlg = EffectSettingsDialog(effect_key, params, self)
        # When user tweaks a slider, we update the model AND emit signal to engine
        dlg.params_changed.connect(lambda p, v: self.effect_params_changed.emit(self.strip.uid, effect_key))
        dlg.exec()

    def _on_fx_toggled(self, effect_key, checked, button):
        # Update Model (Structure is now {active: bool, params: dict})
        if effect_key in self.strip.effects:
            if isinstance(self.strip.effects[effect_key], dict):
                self.strip.effects[effect_key]['active'] = checked
            else:
                # Fallback safety
                self.strip.effects[effect_key] = checked
                
        self._update_fx_button_style(button, checked)
        self.effect_toggled.emit(self.strip.uid, effect_key, checked)

    def _update_fx_button_style(self, button, active):
        if active:
            button.setStyleSheet("""
                QPushButton { background-color: #2ecc71; color: white; border-radius: 2px; font-size: 8px; font-weight: bold; border: none; }
            """)
        else:
            button.setStyleSheet("""
                QPushButton { background-color: #333; color: #666; border-radius: 2px; font-size: 8px; border: 1px solid #444; }
                QPushButton:hover { background-color: #444; color: #999; }
            """)

    def _style_combo(self, combo):
        combo.setStyleSheet("""
            QComboBox { background-color: #222; color: white; border: 1px solid #444; border-radius: 3px; font-size: 9px; padding: 2px; }
            QComboBox::drop-down { border: none; }
            QComboBox QAbstractItemView { background-color: #222; selection-background-color: #444; }
        """)

    def set_device_list(self, devices):
        if not hasattr(self, 'device_combo'): return
        self.device_combo.blockSignals(True)
        self.device_combo.clear()
        if self.strip.kind == StripType.INPUT:
            self.device_combo.addItem("Apps / Virtual", None)
        else:
            self.device_combo.addItem("Virtual Sink (Bus)", None)
        selected_index = 0
        found_current = False
        for i, dev in enumerate(devices):
            name = dev.get('name')
            desc = dev.get('description', name)
            if len(desc) > 15: desc = desc[:15] + "..."
            self.device_combo.addItem(desc, name)
            if self.strip.device_name == name:
                selected_index = i + 1
                found_current = True
        if self.strip.device_name and not found_current:
            missing_label = f"{self.strip.device_name} (Not Found)"
            if len(missing_label) > 15: missing_label = missing_label[:15] + "..."
            self.device_combo.addItem(missing_label, self.strip.device_name)
            selected_index = self.device_combo.count() - 1
        self.device_combo.setCurrentIndex(selected_index)
        self.device_combo.blockSignals(False)
        self._refresh_device_ui_state()

    def _refresh_device_ui_state(self):
        self._update_app_btn_visibility()
        self._update_base_style()
        if self.strip.kind == StripType.OUTPUT and hasattr(self, 'lbl_dev_type'):
            if self.strip.device_name is None:
                self.lbl_dev_type.setText("VIRTUAL BUS")
                self.lbl_dev_type.setStyleSheet("font-size: 8px; color: #dcd0ff; margin-top: 5px; font-weight: bold;")
            else:
                self.lbl_dev_type.setText("DEVICE OUT")
                self.lbl_dev_type.setStyleSheet("font-size: 8px; color: #aaa; margin-top: 5px;")

    def _on_device_changed(self, index):
        device_name = self.device_combo.itemData(index)
        self.strip.device_name = device_name
        self._refresh_device_ui_state()
        self.device_changed.emit(self.strip.uid, device_name)

    def _on_default_toggled(self, checked):
        self.strip.is_default = checked
        self._update_app_btn_visibility()
        self.default_changed.emit(self.strip.uid, checked)

    def _update_app_btn_visibility(self):
        if hasattr(self, 'btn_apps'):
            is_virtual = (self.strip.device_name is None)
            is_not_default = not self.strip.is_default
            should_show = is_virtual and is_not_default
            self.btn_apps.setVisible(should_show)

    def eventFilter(self, obj, event):
        if obj == self.lbl_name and event.type() == QEvent.MouseButtonDblClick:
            self._rename_strip()
            return True
        return super().eventFilter(obj, event)

    def _on_label_context_menu(self, pos):
        menu = QMenu(self)
        action_rename = QAction("Rename...", self)
        action_rename.triggered.connect(self._rename_strip)
        menu.addAction(action_rename)
        menu.exec(self.lbl_name.mapToGlobal(pos))

    def _rename_strip(self):
        new_name, ok = QInputDialog.getText(self, "Rename Strip", "New Name:", text=self.strip.label)
        if ok and new_name:
            new_name = new_name.strip()
            if new_name:
                self.strip.label = new_name
                self.lbl_name.setText(new_name)
                self.label_changed.emit(self.strip.uid, new_name)

    def set_routing_targets(self, output_strips):
        if self.strip.kind != StripType.INPUT: return
        while self.routing_layout.count():
            item = self.routing_layout.takeAt(0)
            if item.widget(): item.widget().deleteLater()
        if not output_strips:
            self.routing_layout.addWidget(self.lbl_no_route)
            return
        for out_strip in output_strips:
            btn = QPushButton(out_strip.label[:4].upper())
            btn.setCheckable(True)
            btn.setFixedHeight(20)
            btn.setChecked(out_strip.uid in self.strip.routes)
            btn.setStyleSheet(f"""
                QPushButton {{ background-color: #333; color: #888; border: 1px solid #444; border-radius: 3px; font-size: 9px; }}
                QPushButton:checked {{ background-color: #4caf50; color: white; border: 1px solid #4caf50; }}
                QPushButton:hover:!checked {{ background-color: #444; color: white; }}
            """)
            btn.clicked.connect(lambda checked, uid=out_strip.uid: self._on_route_toggled(uid, checked))
            self.routing_layout.addWidget(btn)

    def _on_route_toggled(self, target_uid, checked):
        if checked:
            if target_uid not in self.strip.routes: self.strip.routes.append(target_uid)
        else:
            if target_uid in self.strip.routes: self.strip.routes.remove(target_uid)
        self.route_changed.emit(self.strip.uid, target_uid, checked)

    def _show_midi_menu(self):
        menu = QMenu(self)
        act_vol = QAction("Learn Volume", self)
        act_vol.triggered.connect(lambda: self.midi_learn_requested.emit(self.strip.uid, "volume"))
        act_mute = QAction("Learn Mute", self)
        act_mute.triggered.connect(lambda: self.midi_learn_requested.emit(self.strip.uid, "mute"))
        act_mono = QAction("Learn Mono", self)
        act_mono.triggered.connect(lambda: self.midi_learn_requested.emit(self.strip.uid, "mono"))
        act_clear = QAction("Clear Mappings", self)
        act_clear.triggered.connect(self._clear_midi)
        menu.addAction(act_vol)
        menu.addAction(act_mute)
        menu.addAction(act_mono)
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
        self.strip.midi_mono = None
        self.mute_changed.emit(self.strip.uid, self.strip.mute)

    def _on_slider_move(self, val):
        self.strip.volume = val / 100.0

    def set_default_state(self, is_default: bool):
        if hasattr(self, 'cb_default'):
            self.cb_default.blockSignals(True)
            self.cb_default.setChecked(is_default)
            self.cb_default.blockSignals(False)
            self.strip.is_default = is_default
            self._update_app_btn_visibility()

    def update_ui_from_model(self):
        self.slider.blockSignals(True)
        self.slider.setValue(int(self.strip.volume * 100))
        self.slider.blockSignals(False)
        self.btn_mute.blockSignals(True)
        self.btn_mute.setChecked(self.strip.mute)
        self.btn_mute.blockSignals(False)
        self._update_mute_style()
        self.btn_mono.blockSignals(True)
        self.btn_mono.setChecked(self.strip.is_mono)
        self.btn_mono.blockSignals(False)
        self._update_mono_style()
        if hasattr(self, 'cb_default'):
            self.set_default_state(self.strip.is_default)
            
        # Update FX buttons
        if hasattr(self, 'fx_buttons'):
            for key, btn in self.fx_buttons.items():
                # Handle dictionary structure
                fx_data = self.strip.effects.get(key, {})
                active = fx_data.get('active', False) if isinstance(fx_data, dict) else fx_data
                
                btn.blockSignals(True)
                btn.setChecked(active)
                self._update_fx_button_style(btn, active)
                btn.blockSignals(False)

    def _check_and_send_volume(self):
        current_vol = round(self.strip.volume, 2)
        if current_vol != round(self.last_sent_vol, 2):
            self.volume_changed.emit(self.strip.uid, self.strip.volume)
            self.last_sent_vol = self.strip.volume

    def _on_mute_toggle(self, checked):
        self.strip.mute = checked
        self._update_mute_style()
        self.mute_changed.emit(self.strip.uid, checked)

    def _on_mono_toggle(self, checked):
        self.strip.is_mono = checked
        self._update_mono_style()
        self.mono_changed.emit(self.strip.uid, checked)

    def _update_mute_style(self):
        if self.btn_mute.isChecked():
            self.btn_mute.setStyleSheet("background-color: #ff4444; color: white; font-weight: bold; border: none; border-radius: 3px; font-size: 9px;")
            self.btn_mute.setText("MUTED")
        else:
            self.btn_mute.setStyleSheet("background-color: #444; color: white; border: none; border-radius: 3px; font-size: 9px;")
            self.btn_mute.setText("MUTE")

    def _update_mono_style(self):
        if self.btn_mono.isChecked():
            self.btn_mono.setStyleSheet("background-color: #3daee9; color: white; font-weight: bold; border: none; border-radius: 3px; font-size: 9px;")
        else:
            self.btn_mono.setStyleSheet("background-color: #444; color: #888; border: none; border-radius: 3px; font-size: 9px;")

    def _on_delete_clicked(self):
        self.delete_requested.emit(self.strip.uid)

    def update_vumeter(self, left, right):
        if self.strip.is_mono:
            mono_val = max(left, right)
            left = right = mono_val
        if hasattr(self, 'vu_left'): self.vu_left.set_level(left)
        if hasattr(self, 'vu_right'): self.vu_right.set_level(right)