import uuid
import copy

class StripType:
    """Constants to define if the strip is an Input or an Output."""
    INPUT = "input"
    OUTPUT = "output"

class StripMode:
    """
    Constants to define the nature of the strip.
    - PHYSICAL: Connects to real hardware (Microphone, Speakers).
    - VIRTUAL: Creates a virtual device for software (Apps, Discord, etc.).
    """
    PHYSICAL = "physical"
    VIRTUAL = "virtual"

# Default parameters to avoid saturation (EQ) and provide usable starting points
DEFAULT_EFFECT_PARAMS = {
    "gate": {
        "Threshold (dB)": -30.0,
        "Attack (ms)": 5.0,
        "Release (ms)": 200.0,
        "Hold (ms)": 50.0
    },
    "compressor": {
        "Threshold (dB)": -15.0, # Less aggressive start
        "Ratio (1:n)": 4.0,
        "Attack (ms)": 20.0,
        "Release (ms)": 300.0,
        "Makeup Gain (dB)": 0.0  # Crucial: 0dB default to avoid blasting volume
    },
    "eq": {
        # MBEQ_1197 15 bands - Default to FLAT (0.0) to prevent saturation
        "50Hz": 0.0, "100Hz": 0.0, "156Hz": 0.0, "220Hz": 0.0, "311Hz": 0.0,
        "440Hz": 0.0, "622Hz": 0.0, "880Hz": 0.0, "1250Hz": 0.0, "1750Hz": 0.0,
        "2500Hz": 0.0, "3500Hz": 0.0, "5000Hz": 0.0, "10000Hz": 0.0, "20000Hz": 0.0
    },
    "noise_cancel": {
        "Model": 0.0 # Placeholder if we add VAD threshold later
    }
}

class Strip:
    def __init__(self, label, kind, mode=StripMode.VIRTUAL, uid=None):
        # Unique Identifier (persistent across restarts)
        self.uid = uid if uid else str(uuid.uuid4())
        
        # User-facing properties
        self.label = label      # Ex: "Discord", "Micro", "Speakers"
        self.kind = kind        # StripType.INPUT or StripType.OUTPUT
        self.mode = mode        # StripMode.PHYSICAL or StripMode.VIRTUAL
        
        # Audio State
        self.volume = 1.0       # 0.0 to 1.0 (can go higher)
        self.mute = False       # True = Muted
        self.is_mono = False    # True = Downmix Stereo to Mono
        
        # Routing Matrix (Only relevant for Input strips)
        # List of Output UIDs this strip sends audio to.
        self.routes = [] 
        
        # Hardware/PipeWire connection details
        self.device_name = None 

        # Software Assignment (For Inputs)
        self.assigned_apps = []

        # System Default Sink Flag
        self.is_default = False

        # MIDI Mapping configuration
        self.midi_volume = None 
        self.midi_mute = None
        self.midi_mono = None

        # Effects Configuration (Only for Inputs)
        # Structure: { "effect_name": { "active": bool, "params": { ... } } }
        self.effects = {
            "gate": {"active": False, "params": copy.deepcopy(DEFAULT_EFFECT_PARAMS["gate"])},
            "noise_cancel": {"active": False, "params": copy.deepcopy(DEFAULT_EFFECT_PARAMS["noise_cancel"])},
            "eq": {"active": False, "params": copy.deepcopy(DEFAULT_EFFECT_PARAMS["eq"])},
            "compressor": {"active": False, "params": copy.deepcopy(DEFAULT_EFFECT_PARAMS["compressor"])}
        }

    def to_dict(self):
        """Serialize the object to a dictionary for JSON saving."""
        return {
            'uid': self.uid,
            'label': self.label,
            'kind': self.kind,
            'mode': self.mode,
            'volume': self.volume,
            'mute': self.mute,
            'is_mono': self.is_mono,
            'routes': self.routes,
            'device_name': self.device_name,
            'assigned_apps': self.assigned_apps,
            'is_default': self.is_default,
            'midi_volume': self.midi_volume,
            'midi_mute': self.midi_mute,
            'midi_mono': self.midi_mono,
            'effects': self.effects
        }

    @classmethod
    def from_dict(cls, data):
        """Create a Strip object from a dictionary (loading from JSON)."""
        strip = cls(
            label=data['label'],
            kind=data['kind'],
            mode=data.get('mode', StripMode.VIRTUAL),
            uid=data.get('uid')
        )
        strip.volume = data.get('volume', 1.0)
        strip.mute = data.get('mute', False)
        strip.is_mono = data.get('is_mono', False)
        strip.routes = data.get('routes', [])
        strip.device_name = data.get('device_name')
        strip.assigned_apps = data.get('assigned_apps', [])
        strip.is_default = data.get('is_default', False)
        strip.midi_volume = data.get('midi_volume')
        strip.midi_mute = data.get('midi_mute')
        strip.midi_mono = data.get('midi_mono')
        
        # Migration Logic for Effects (Boolean -> Object)
        raw_effects = data.get('effects', {})
        normalized_effects = {}
        
        # Defined keys to look for
        known_keys = ["gate", "noise_cancel", "eq", "compressor"]
        
        for key in known_keys:
            val = raw_effects.get(key, False)
            default_p = copy.deepcopy(DEFAULT_EFFECT_PARAMS.get(key, {}))
            
            if isinstance(val, bool):
                # OLD FORMAT: Convert boolean to object
                normalized_effects[key] = {
                    "active": val,
                    "params": default_p
                }
            elif isinstance(val, dict):
                # NEW FORMAT: Validate structure
                active = val.get("active", False)
                params = val.get("params", default_p)
                # Ensure missing params are filled with defaults (e.g. if we added new controls)
                for p_key, p_val in default_p.items():
                    if p_key not in params:
                        params[p_key] = p_val
                        
                normalized_effects[key] = {
                    "active": active,
                    "params": params
                }
            else:
                # Fallback
                normalized_effects[key] = {"active": False, "params": default_p}
                
        strip.effects = normalized_effects
        
        return strip

    def __repr__(self):
        return f"<Strip '{self.label}' ({self.kind}) Vol:{self.volume} Mono:{self.is_mono}>"