from PySide6.QtWidgets import (QDialog, QVBoxLayout, QHBoxLayout, QLabel,
                               QSlider, QDoubleSpinBox, QPushButton, QWidget)
from PySide6.QtCore import Qt

class EffectSettingsDialog(QDialog):
    """
    Dynamic dialog to configure LADSPA effect parameters.

    The dialog mutates the `current_params` dict in place. Callers should
    emit their own "params changed" signal once on dialog close (not on
    every slider tick), so the audio engine restarts at most once per
    dialog session.
    """
    # Note: the per-slider `params_changed` signal was removed — it was
    # emitted but never connected anywhere. The in-place dict mutation is
    # the actual mechanism used to propagate changes.

    def __init__(self, effect_name, current_params, parent=None):
        super().__init__(parent)
        self.setWindowTitle(f"Settings: {effect_name.upper()}")
        self.setMinimumWidth(420)
        self.setModal(True)
        self.current_params = current_params
        self.effect_name = effect_name
        
        self._init_ui()

    def _init_ui(self):
        main_layout = QVBoxLayout(self)

        container = QWidget()
        self.form_layout = QVBoxLayout(container)
        self.form_layout.setSpacing(15)

        # Sort params to keep order (especially for EQ frequencies)
        # Simple heuristic: try to sort by numeric value in key if possible (for Hz), else alpha
        sorted_keys = sorted(self.current_params.keys(), key=self._sort_key)

        for param_key in sorted_keys:
            # Skip params whose name contains '=' — their value is filtered
            # out by the SPA-JSON formatter (e.g. gate "Output select
            # (-1 = key listen, 0 = gate, 1 = bypass)").  Showing a control
            # that never reaches PipeWire would be misleading.
            if '=' in param_key:
                continue
            val = self.current_params[param_key]
            self._add_control(param_key, val)

        main_layout.addWidget(container)

        # Close Button
        btn_close = QPushButton("Close")
        btn_close.clicked.connect(self.accept)
        btn_close.setStyleSheet("""
            QPushButton { background-color: #444; color: white; padding: 8px; border-radius: 4px; }
            QPushButton:hover { background-color: #555; }
        """)
        main_layout.addWidget(btn_close)

        # Auto-size the dialog to fit all controls (no scroll bar).
        # Each effect has a different number of params so the height
        # naturally adapts (EQ=15 → tall, tube=2 → short).
        self.adjustSize()
        # Clamp to available screen height to avoid going off-screen
        # on very small displays.
        screen = self.screen()
        if screen is not None:
            max_h = screen.availableGeometry().height() - 50
            if self.height() > max_h:
                self.setMaximumHeight(max_h)

    def _sort_key(self, key):
        """Helper to sort EQ bands like 50Hz, 100Hz correctly.
        Handles both legacy names ('50Hz') and full LADSPA port names
        ('50Hz gain (low shelving)')."""
        import re
        m = re.match(r'(\d+)\s*Hz', key)
        if m:
            return int(m.group(1))
        return key

    def _add_control(self, param_name, current_value):
        row_widget = QWidget()
        row_layout = QHBoxLayout(row_widget)
        row_layout.setContentsMargins(0, 0, 0, 0)
        
        # Label
        lbl = QLabel(param_name)
        lbl.setFixedWidth(120)
        lbl.setStyleSheet("color: white; font-size: 11px;")
        
        # SpinBox (Value)
        spin = QDoubleSpinBox()
        spin.setFixedWidth(60)
        spin.setStyleSheet("color: white; background: #444; border: none;")
        
        # Slider
        slider = QSlider(Qt.Horizontal)
        slider.setStyleSheet("""
            QSlider::groove:horizontal { background: #444; height: 6px; border-radius: 3px; }
            QSlider::handle:horizontal { background: #3daee9; width: 14px; margin: -4px 0; border-radius: 7px; }
        """)
        
        # Determine Range based on param name / context
        # This is a bit of a "Magic config" mapping based on LADSPA common ranges
        min_val, max_val, step = self._get_range_for_param(param_name)
        
        spin.setRange(min_val, max_val)
        spin.setSingleStep(step)
        spin.setValue(current_value)
        
        # Slider is int only, so we map it
        slider_factor = 10 if step < 1 else 1
        slider.setRange(int(min_val * slider_factor), int(max_val * slider_factor))
        slider.setValue(int(current_value * slider_factor))

        # Connect signals
        # We use a lambda to break loops, but capture vars carefully
        slider.valueChanged.connect(lambda v: spin.setValue(v / slider_factor))
        spin.valueChanged.connect(lambda v: slider.setValue(int(v * slider_factor)))
        
        # Main update signal
        spin.valueChanged.connect(lambda v: self._on_value_changed(param_name, v))

        row_layout.addWidget(lbl)
        row_layout.addWidget(slider)
        row_layout.addWidget(spin)
        
        self.form_layout.addWidget(row_widget)

    def _get_range_for_param(self, name):
        """Returns (min, max, step) based on parameter name."""
        name_l = name.lower()

        # --- RNNoise (librnnoise_ladspa) params ---
        if "vad threshold" in name_l:
            return (0.0, 99.0, 1.0)
        if "dry mix" in name_l:
            return (0.0, 1.0, 0.01)
        if "grace" in name_l:
            # VAD Grace Period (0-1000) and Retroactive VAD Grace (0-200)
            if "retroactive" in name_l:
                return (0.0, 200.0, 1.0)
            return (0.0, 1000.0, 1.0)

        # --- Compressor sc4_1882: RMS/peak (0-1, no unit keyword) ---
        if "rms" in name_l:
            return (0.0, 1.0, 0.01)

        # --- TAP TubeWarmth: Drive (0.1-10) ---
        # NOTE: checked BEFORE the generic "db"/"hz"/"ms" rules so "drive"
        # never falls through to a wrong range.
        if "drive" in name_l:
            return (0.1, 10.0, 0.1)

        # --- TAP TubeWarmth: Tape--Tube Blend (-10 to +10) ---
        if "blend" in name_l:
            return (-10.0, 10.0, 0.5)

        if "db" in name_l:
            if "threshold" in name_l: return (-70.0, 20.0, 0.5)
            if "makeup" in name_l: return (0.0, 24.0, 0.5)
            if "knee" in name_l: return (1.0, 10.0, 0.1)
            if "range" in name_l: return (-90.0, 0.0, 0.5)
            if "gain" in name_l: return (-24.0, 24.0, 0.5) # EQ band gains
            return (-60.0, 10.0, 0.5)

        if "hz" in name_l:
            if "filter" in name_l: return (0.0, 20000.0, 10.0)  # Gate key filters
            # EQ band gains (mbeq_1197 plugin range: -70 to +30)
            return (-70.0, 30.0, 0.1)

        if "ms" in name_l:
            if "attack" in name_l: return (0.01, 1000.0, 0.1)
            if "release" in name_l: return (2.0, 4000.0, 1.0)
            if "hold" in name_l: return (2.0, 2000.0, 1.0)
            if "decay" in name_l: return (2.0, 4000.0, 1.0)

        if "ratio" in name_l:
            return (1.0, 20.0, 0.5)

        # Gate output select (-1 = key listen, 0 = gate, 1 = bypass)
        if "output select" in name_l:
            return (-1.0, 1.0, 1.0)

        return (0.0, 10.0, 0.1) # Default

    def _on_value_changed(self, param, value):
        # Mutate the shared dict in place. The signal that the parent listens
        # to (`effect_params_changed`) is emitted from the widget *once* on
        # dialog close, not on every slider tick — see strip_widget.py.
        self.current_params[param] = value