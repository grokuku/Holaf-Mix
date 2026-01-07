from PySide6.QtWidgets import (QMainWindow, QWidget, QVBoxLayout, QHBoxLayout, 
                               QLabel, QScrollArea, QFrame, QPushButton, QInputDialog, QMessageBox,
                               QDialog, QCheckBox, QDialogButtonBox, QSizePolicy, QSystemTrayIcon, QMenu, QApplication, QStyle)
from PySide6.QtCore import Qt, QThreadPool, QRunnable, Slot, QTimer, QByteArray
from PySide6.QtGui import QAction, QIcon
from src.config import settings
from src.models.strip_model import Strip, StripType, StripMode
from src.ui.widgets.strip_widget import StripWidget
import pipewire_utils 

# --- Custom Dialog for App Selection ---
class AppSelectionDialog(QDialog):
    def __init__(self, running_apps, assigned_apps, parent=None):
        super().__init__(parent)
        self.setWindowTitle("Assign Applications")
        self.resize(300, 400)
        self.selected_apps = []
        
        layout = QVBoxLayout(self)
        
        info = QLabel("Select applications to route to this strip:")
        info.setWordWrap(True)
        layout.addWidget(info)
        
        self.scroll = QScrollArea()
        self.scroll.setWidgetResizable(True)
        self.scroll_content = QWidget()
        self.scroll_layout = QVBoxLayout(self.scroll_content)
        self.scroll.setWidget(self.scroll_content)
        layout.addWidget(self.scroll)
        
        self.check_boxes = []
        
        all_names = set([app['name'] for app in running_apps])
        all_names.update(assigned_apps)
        
        for name in sorted(all_names):
            cb = QCheckBox(name)
            if name in assigned_apps:
                cb.setChecked(True)
            self.scroll_layout.addWidget(cb)
            self.check_boxes.append(cb)
            
        self.scroll_layout.addStretch()
        
        buttons = QDialogButtonBox(QDialogButtonBox.Ok | QDialogButtonBox.Cancel)
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        layout.addWidget(buttons)

    def get_selected_apps(self):
        return [cb.text() for cb in self.check_boxes if cb.isChecked()]

# --- Worker for ThreadPool ---
class BackendWorker(QRunnable):
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
        
        # --- WINDOW FLAGS (Always on Top) ---
        self.setWindowFlags(self.windowFlags() | Qt.WindowStaysOnTopHint)
        
        self.audio_engine = audio_engine
        self.midi_engine = midi_engine
        self.thread_pool = QThreadPool()
        self.thread_pool.setMaxThreadCount(4)
        
        self.midi_lookup = {}
        self.widgets = {}
        
        # --- Load Config (Strips + Geometry) ---
        self.strips, window_geo_hex = settings.load_config()
        if window_geo_hex:
            self.restoreGeometry(QByteArray.fromHex(window_geo_hex.encode()))

        self._rebuild_midi_lookup()
        
        self.central_widget = QWidget()
        self.setCentralWidget(self.central_widget)
        
        self.main_layout = QHBoxLayout(self.central_widget)
        self.main_layout.setSpacing(0)
        self.main_layout.setContentsMargins(0, 0, 0, 0)
        
        self.inputs_container = self._create_section("INPUTS", "#2b2b2b", StripType.INPUT)
        self.main_layout.addWidget(self.inputs_container)

        self.outputs_container = self._create_section("OUTPUTS", "#1e1e1e", StripType.OUTPUT)
        self.main_layout.addWidget(self.outputs_container)
        
        self.refresh_ui()
        self._init_midi()
        self._init_tray_icon()

        # Timer for routing enforcement
        self.enforce_timer = QTimer(self)
        self.enforce_timer.timeout.connect(self._enforce_app_routing)
        self.enforce_timer.start(5000) 
        
        # Timer for VU Meters (High frequency)
        self.meter_timer = QTimer(self)
        self.meter_timer.setInterval(40) # 40ms = 25 FPS
        self.meter_timer.timeout.connect(self._update_meters)
        self.meter_timer.start()

    def _init_tray_icon(self):
        """Sets up the System Tray Icon for minimize/restore behavior."""
        self.tray_icon = QSystemTrayIcon(self)
        
        # We need an icon. Using standard system icon.
        style = QApplication.style()
        # FIX: Access SP_MediaVolume via QStyle class, not instance
        icon = style.standardIcon(QStyle.SP_MediaVolume)
        self.tray_icon.setIcon(icon)
        
        # Tray Menu
        tray_menu = QMenu()
        restore_action = QAction("Show/Hide", self)
        restore_action.triggered.connect(self._toggle_window)
        tray_menu.addAction(restore_action)
        
        quit_action = QAction("Quit", self)
        quit_action.triggered.connect(self._force_quit)
        tray_menu.addAction(quit_action)
        
        self.tray_icon.setContextMenu(tray_menu)
        self.tray_icon.activated.connect(self._on_tray_activated)
        self.tray_icon.show()

    def _toggle_window(self):
        if self.isVisible():
            self.hide()
        else:
            self.showNormal()
            self.activateWindow()

    def _on_tray_activated(self, reason):
        if reason == QSystemTrayIcon.Trigger:
            self._toggle_window()

    def _force_quit(self):
        """Actually close the app."""
        self._save_state()
        QApplication.quit()

    def closeEvent(self, event):
        """
        Override close event to minimize instead of quitting,
        BUT save state just in case.
        """
        self._save_state()
        if self.tray_icon.isVisible():
            self.hide()
            event.ignore()
        else:
            event.accept()

    def _save_state(self):
        """Saves current strips config and window geometry."""
        geo_hex = self.saveGeometry().toHex().data().decode()
        settings.save_config(self.strips, geo_hex)

    def _init_midi(self):
        if not self.midi_engine: return
        self.midi_engine.mapping_detected.connect(self.on_midi_mapping_detected)
        self.midi_engine.message_received.connect(self.on_midi_message_received)
        ports = self.midi_engine.get_input_names()
        if ports:
            target = next((p for p in ports if "mix" in p.lower()), ports[0])
            self.midi_engine.open_port(target)

    def _rebuild_midi_lookup(self):
        self.midi_lookup.clear()
        for strip in self.strips:
            # Volume
            if strip.midi_volume:
                m = strip.midi_volume
                key = (m['type'], m['channel'], m.get('control') or m.get('note'))
                self.midi_lookup[key] = (strip.uid, "volume")
            # Mute
            if strip.midi_mute:
                m = strip.midi_mute
                key = (m['type'], m['channel'], m.get('control') or m.get('note'))
                self.midi_lookup[key] = (strip.uid, "mute")
            # Mono
            if strip.midi_mono:
                m = strip.midi_mono
                key = (m['type'], m['channel'], m.get('control') or m.get('note'))
                self.midi_lookup[key] = (strip.uid, "mono")

    def on_midi_message_received(self, msg):
        identifier = getattr(msg, 'control', None) if msg.type == 'control_change' else getattr(msg, 'note', None)
        if identifier is None: return
        
        key = (msg.type, msg.channel, identifier)
        if key in self.midi_lookup:
            uid, prop = self.midi_lookup[key]
            strip = next((s for s in self.strips if s.uid == uid), None)
            if not strip: return
            
            # --- ACTION LOGIC ---
            if prop == "volume":
                strip.volume = msg.value / 127.0
            
            elif prop == "mute":
                val = getattr(msg, 'velocity', 127) if hasattr(msg, 'velocity') else msg.value
                # Toggle only on button press (value > 0), ignore release (value 0)
                if val > 0: 
                    strip.mute = not strip.mute
                    # BUGFIX: Ensure backend is notified!
                    self._run_in_background(self.audio_engine.set_mute, strip.uid, strip.mute)

            elif prop == "mono":
                val = getattr(msg, 'velocity', 127) if hasattr(msg, 'velocity') else msg.value
                # Toggle only on button press
                if val > 0:
                    strip.is_mono = not strip.is_mono
                    # BUGFIX: Ensure backend is notified!
                    self._run_in_background(self.audio_engine.set_mono, strip.uid, strip.is_mono)

            # Refresh UI
            widget = self.widgets.get(uid)
            if widget:
                QTimer.singleShot(0, widget.update_ui_from_model)

    def on_midi_mapping_detected(self, uid, prop, mapping):
        strip = next((s for s in self.strips if s.uid == uid), None)
        if strip:
            if prop == "volume": 
                strip.midi_volume = mapping
            elif prop == "mute": 
                strip.midi_mute = mapping
            elif prop == "mono": 
                strip.midi_mono = mapping
                
            self._save_state()
        
        widget = self.widgets.get(uid)
        if widget: widget.set_learning(False)
        self._rebuild_midi_lookup()

    def on_midi_learn_requested(self, uid, prop):
        if self.midi_engine:
            widget = self.widgets.get(uid)
            if widget: widget.set_learning(True)
            self.midi_engine.start_learning(uid, prop)

    def _create_section(self, title, bg_color, strip_kind):
        container = QWidget()
        container.setStyleSheet(f"background-color: {bg_color};")
        container.setSizePolicy(QSizePolicy.Preferred, QSizePolicy.Expanding)
        
        layout = QVBoxLayout(container)
        layout.setContentsMargins(0, 0, 0, 0)
        
        header_widget = QWidget()
        header_layout = QHBoxLayout(header_widget)
        header_layout.setContentsMargins(10, 10, 10, 0)
        
        label = QLabel(title)
        label.setStyleSheet("font-weight: bold; color: white; font-size: 14px;")
        
        btn_add = QPushButton("+")
        btn_add.setFixedSize(30, 30)
        btn_add.setCursor(Qt.PointingHandCursor)
        btn_add.setStyleSheet("""
            QPushButton { background-color: #444; color: white; border-radius: 15px; font-weight: bold; }
            QPushButton:hover { background-color: #666; }
        """)
        btn_add.clicked.connect(lambda: self.on_add_clicked(strip_kind))

        header_layout.addWidget(label)
        header_layout.addSpacing(10)
        header_layout.addWidget(btn_add)
        header_layout.addStretch() 
        
        layout.addWidget(header_widget)

        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setFrameShape(QFrame.NoFrame)
        scroll.setStyleSheet("background: transparent;")
        
        content_widget = QWidget()
        content_layout = QHBoxLayout(content_widget)
        content_layout.setAlignment(Qt.AlignTop | Qt.AlignLeft)
        content_layout.setSpacing(10)
        content_layout.setContentsMargins(10, 10, 10, 10) 
        content_widget.setLayout(content_layout)
        scroll.setWidget(content_widget)
        layout.addWidget(scroll)
        container.content_layout = content_layout 
        return container

    def refresh_ui(self):
        self._clear_layout(self.inputs_container.content_layout)
        self._clear_layout(self.outputs_container.content_layout)
        self.widgets.clear()
        
        output_strips = [s for s in self.strips if s.kind == StripType.OUTPUT]

        for strip in self.strips:
            widget = StripWidget(strip)
            self.widgets[strip.uid] = widget
            
            # --- SIGNAL CONNECTIONS ---
            widget.volume_changed.connect(self.on_strip_volume_changed)
            widget.mute_changed.connect(self.on_strip_mute_changed)
            widget.mono_changed.connect(self.on_strip_mono_changed) 
            widget.label_changed.connect(self.on_strip_label_changed)
            widget.delete_requested.connect(self.on_strip_delete_requested)
            widget.route_changed.connect(self.on_strip_route_changed)
            widget.midi_learn_requested.connect(self.on_midi_learn_requested)
            widget.device_changed.connect(self.on_strip_device_changed)
            widget.app_selection_requested.connect(self.on_app_selection_requested)
            widget.default_changed.connect(self.on_strip_default_changed) 
            
            if strip.kind == StripType.INPUT:
                widget.set_routing_targets(output_strips)
                self.inputs_container.content_layout.addWidget(widget)
            else:
                self.outputs_container.content_layout.addWidget(widget)
        
        self._populate_devices()
        
        # Increase delay to ensure layout count is accurate
        QTimer.singleShot(50, self._adjust_window_size)

    def _adjust_window_size(self):
        """
        Calculates required width and forces distribution via Stretch Factors.
        """
        STRIP_WIDTH = 100
        SPACING = 10
        MARGIN_TOTAL_PER_SECTION = 22
        MIN_SECTION_WIDTH = 160 
        
        def calculate_section_width(layout):
            count = layout.count()
            if count == 0:
                return MIN_SECTION_WIDTH
            total = (count * STRIP_WIDTH) + (max(0, count - 1) * SPACING) + MARGIN_TOTAL_PER_SECTION
            return max(total, MIN_SECTION_WIDTH)

        width_inputs = calculate_section_width(self.inputs_container.content_layout)
        width_outputs = calculate_section_width(self.outputs_container.content_layout)
        
        self.main_layout.setStretch(0, width_inputs)
        self.main_layout.setStretch(1, width_outputs)

        total_width = width_inputs + width_outputs
        
        self.setMinimumWidth(0)
        self.setFixedWidth(total_width)
    
    def _update_meters(self):
        """
        Polls the audio engine for VU meter levels and updates the widgets.
        """
        if not self.audio_engine: return
        
        levels = self.audio_engine.get_meter_levels()
        for uid, (left, right) in levels.items():
            widget = self.widgets.get(uid)
            if widget:
                widget.update_vumeter(left, right)

    def _populate_devices(self):
        nodes = pipewire_utils.get_audio_nodes()
        
        # Sort nodes alphabetically by description for better UX
        nodes.sort(key=lambda x: x.get('description', '').lower())
        
        sinks = [n for n in nodes if n.get('media_class') == 'Audio/Sink']
        sources = [n for n in nodes if n.get('media_class') == 'Audio/Source']

        for uid, widget in self.widgets.items():
            if widget.strip.kind == StripType.OUTPUT:
                widget.set_device_list(sinks)
            else:
                widget.set_device_list(sources)

    def _clear_layout(self, layout):
        while layout.count():
            item = layout.takeAt(0)
            if item.widget(): item.widget().deleteLater()

    # --- Actions ---

    def on_add_clicked(self, kind):
        name, ok = QInputDialog.getText(self, "Add Strip", "Enter name for new strip:")
        if ok and name:
            new_strip = Strip(label=name, kind=kind, mode=StripMode.VIRTUAL)
            self.strips.append(new_strip)
            self._save_state()
            if self.audio_engine:
                self._run_in_background(self.audio_engine.start_engine, self.strips)
            self.refresh_ui()

    def on_strip_delete_requested(self, uid):
        strip_to_remove = next((s for s in self.strips if s.uid == uid), None)
        if not strip_to_remove: return
        reply = QMessageBox.question(self, "Confirm Delete", 
            f"Are you sure you want to remove '{strip_to_remove.label}'?",
            QMessageBox.Yes | QMessageBox.No, QMessageBox.No)
        if reply == QMessageBox.Yes:
            self.strips.remove(strip_to_remove)
            if strip_to_remove.kind == StripType.OUTPUT:
                for s in self.strips:
                    if s.kind == StripType.INPUT and strip_to_remove.uid in s.routes:
                        s.routes.remove(strip_to_remove.uid)
            self._save_state()
            if self.audio_engine:
                def reload():
                    self.audio_engine.shutdown() 
                    self.audio_engine.start_engine(self.strips)
                self._run_in_background(reload)
            self.refresh_ui()

    def on_app_selection_requested(self, uid):
        strip = next((s for s in self.strips if s.uid == uid), None)
        if not strip: return
        running_apps = pipewire_utils.get_sink_inputs()
        dlg = AppSelectionDialog(running_apps, strip.assigned_apps, self)
        if dlg.exec():
            selected = dlg.get_selected_apps()
            strip.assigned_apps = selected
            self._save_state()
            self._move_apps_to_strip(strip, running_apps)

    def _move_apps_to_strip(self, strip, running_apps=None):
        if not running_apps:
            running_apps = pipewire_utils.get_sink_inputs()
        target_sink_name = f"Holaf_Strip_{strip.uid}"
        for app in running_apps:
            if app['name'] in strip.assigned_apps:
                pipewire_utils.move_sink_input(app['id'], target_sink_name)

    def _enforce_app_routing(self):
        running_apps = pipewire_utils.get_sink_inputs()
        for strip in self.strips:
            if strip.kind == StripType.INPUT and strip.mode == StripMode.VIRTUAL:
                 if strip.assigned_apps and not strip.is_default:
                     self._move_apps_to_strip(strip, running_apps)

    # --- Backend Interfacing ---
    def _run_in_background(self, func, *args):
        if self.audio_engine:
            worker = BackendWorker(func, *args)
            self.thread_pool.start(worker)

    def on_strip_volume_changed(self, uid, new_vol):
        self._run_in_background(self.audio_engine.set_volume, uid, new_vol)

    def on_strip_mute_changed(self, uid, is_muted):
        self._run_in_background(self.audio_engine.set_mute, uid, is_muted)
        # Note: We don't save explicitly here to avoid lag, saved on exit.

    def on_strip_mono_changed(self, uid, is_mono):
        self._run_in_background(self.audio_engine.set_mono, uid, is_mono)
        self._save_state()
    
    def on_strip_label_changed(self, uid, new_label):
        self._save_state()

    def on_strip_route_changed(self, source_uid, target_uid, active):
        self._run_in_background(self.audio_engine.update_routing, source_uid, target_uid, active)
        self._save_state()

    def on_strip_device_changed(self, uid, device_name):
        strip = next((s for s in self.strips if s.uid == uid), None)
        if not strip: return
        strip.device_name = device_name
        if device_name:
            strip.mode = StripMode.PHYSICAL
        else:
            strip.mode = StripMode.VIRTUAL
            strip.device_name = None
        self._save_state()
        
        if self.audio_engine:
            def reload():
                self.audio_engine.shutdown()
                self.audio_engine.start_engine(self.strips)
            self._run_in_background(reload)

    def on_strip_default_changed(self, uid, is_default):
        if is_default:
            for s in self.strips:
                if s.kind == StripType.INPUT and s.uid != uid:
                    s.is_default = False
                    w = self.widgets.get(s.uid)
                    if w: w.set_default_state(False)
            target = next((s for s in self.strips if s.uid == uid), None)
            if target: target.is_default = True
            self._save_state()
            if self.audio_engine:
                self._run_in_background(self.audio_engine.set_system_default, uid)