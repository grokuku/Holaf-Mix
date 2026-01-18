from PySide6.QtWidgets import (QDialog, QVBoxLayout, QHBoxLayout, QLabel, 
                               QSlider, QDoubleSpinBox, QPushButton, QWidget, QScrollArea)
from PySide6.QtCore import Qt, Signal

class EffectSettingsDialog(QDialog):
    """
    Dynamic dialog to configure LADSPA effect parameters.
    """
    # Emits (param_name, new_value) when changed
    params_changed = Signal(str, float)

    def __init__(self, effect_name, current_params, parent=None):
        super().__init__(parent)
        self.setWindowTitle(f"Settings: {effect_name.upper()}")
        self.setFixedWidth(400)
        self.setModal(True)
        self.current_params = current_params
        self.effect_name = effect_name
        
        self._init_ui()

    def _init_ui(self):
        main_layout = QVBoxLayout(self)
        
        # Scroll Area for EQ which has many bands
        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setStyleSheet("background-color: #2b2b2b; border: none;")
        
        container = QWidget()
        self.form_layout = QVBoxLayout(container)
        self.form_layout.setSpacing(15)
        
        # Sort params to keep order (especially for EQ frequencies)
        # Simple heuristic: try to sort by numeric value in key if possible (for Hz), else alpha
        sorted_keys = sorted(self.current_params.keys(), key=self._sort_key)
        
        for param_key in sorted_keys:
            val = self.current_params[param_key]
            self._add_control(param_key, val)

        scroll.setWidget(container)
        main_layout.addWidget(scroll)

        # Close Button
        btn_close = QPushButton("Close")
        btn_close.clicked.connect(self.accept)
        btn_close.setStyleSheet("""
            QPushButton { background-color: #444; color: white; padding: 8px; border-radius: 4px; }
            QPushButton:hover { background-color: #555; }
        """)
        main_layout.addWidget(btn_close)

    def _sort_key(self, key):
        """Helper to sort EQ bands like 50Hz, 100Hz correctly."""
        if "Hz" in key:
            try:
                return int(key.replace("Hz", ""))
            except:
                return key
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
        if "db" in name_l:
            if "threshold" in name_l: return (-60.0, 0.0, 0.5)
            if "gain" in name_l: return (-24.0, 24.0, 0.5) # Makeup gain or EQ
            return (-60.0, 10.0, 0.5)
        
        if "hz" in name_l:
            # EQ Gains usually
            return (-30.0, 30.0, 0.1)
            
        if "ms" in name_l:
            if "attack" in name_l: return (0.1, 200.0, 1.0)
            if "release" in name_l: return (10.0, 2000.0, 10.0)
            if "hold" in name_l: return (0.0, 1000.0, 10.0)
            
        if "ratio" in name_l:
            return (1.0, 20.0, 0.5)
            
        return (0.0, 10.0, 0.1) # Default

    def _on_value_changed(self, param, value):
        self.current_params[param] = value
        self.params_changed.emit(param, value)