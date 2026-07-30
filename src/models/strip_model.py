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
        # gate_1410 LADSPA plugin — control port names must match exactly.
        # Defaults chosen so the gate is transparent (open) by default:
        #   Threshold = -70 dB (very low → gate always open for normal signals)
        #   Range     = -90 dB (max reduction when gate closes)
        "LF key filter (Hz)": 33.6,
        "HF key filter (Hz)": 23520.0,
        "Threshold (dB)": -70.0,
        "Attack (ms)": 250.008,
        "Hold (ms)": 1500.5,
        "Decay (ms)": 2001.0,
        "Range (dB)": -90.0,
        "Output select (-1 = key listen, 0 = gate, 1 = bypass)": 0.0
    },
    "compressor": {
        # sc4_1882 LADSPA plugin — control port names must match exactly.
        # Defaults are the plugin's own defaults (compressor transparent).
        "RMS/peak": 0.0,
        "Attack time (ms)": 101.125,
        "Release time (ms)": 401.0,
        "Threshold level (dB)": 0.0,
        "Ratio (1:n)": 1.0,
        "Knee radius (dB)": 3.25,
        "Makeup gain (dB)": 0.0
    },
    "eq": {
        # MBEQ_1197 15 bands - Default to FLAT (0.0) to prevent saturation
        # Port names must match the LADSPA plugin's control port names EXACTLY.
        "50Hz gain (low shelving)": 0.0,
        "100Hz gain": 0.0,
        "156Hz gain": 0.0,
        "220Hz gain": 0.0,
        "311Hz gain": 0.0,
        "440Hz gain": 0.0,
        "622Hz gain": 0.0,
        "880Hz gain": 0.0,
        "1250Hz gain": 0.0,
        "1750Hz gain": 0.0,
        "2500Hz gain": 0.0,
        "3500Hz gain": 0.0,
        "5000Hz gain": 0.0,
        "10000Hz gain": 0.0,
        "20000Hz gain": 0.0
    },
    "noise_cancel": {
        # librnnoise_ladspa (noise_suppressor_stereo) — control port names.
        # These are the real LADSPA ports (verified via analyseplugin):
        #   VAD Threshold (%)          — 0-99, default 74.25, integer
        #   VAD Grace Period (ms)      — 0-1000, default 500, integer
        #   Retroactive VAD Grace (ms) — 0-200, default 100, integer
        #   Placeholder                — unused, no range
        #   Dry Mix                   — 0-1, default 0 (0=full denoise, 1=dry)
        "VAD Threshold (%)": 74.25,
        "VAD Grace Period (ms)": 500,
        "Retroactive VAD Grace (ms)": 100,
        "Dry Mix": 0.0,
    },
    "tube": {
        # Valve_1209 LADSPA plugin - tube saturation
        "Distortion level": 0.0,
        "Distortion character": 0.5,
    }
}

# Neutral (bypass) parameter values for each effect.
# When an effect is inactive, these values make the plugin transparent
# so it stays in the filter-chain graph without altering the audio.
# This enables hot-reload toggling via set-param instead of full restarts.
#
# NOTE: Only the KEY parameter that makes the effect transparent needs to
# be listed here.  The audio engine merges these values on top of
# DEFAULT_EFFECT_PARAMS at build time, so every control port receives an
# explicit value.  This is critical because pw-cli set-param REPLACES all
# control values (it does not merge), so partial specs would reset
# unmentioned ports to LADSPA defaults.
# NOTE: The gate's "Output select" port name contains '=' signs which
# are the same character SPA-JSON uses as key-value separator.  PipeWire's
# parser may reject the ENTIRE control block for a node whose key contains
# '=', discarding all control values → the gate falls back to LADSPA
# defaults (Range = -90 dB, gate mode) → silence.
#
# _format_params() now skips keys containing '=' (see its docstring).
# "Output select" is therefore NEVER sent to PipeWire.  The LADSPA default
# for that port is 0 (gate mode), which is correct for active gates.
# For inactive (bypassed) gates, Range = 0.0 and Threshold = -100.0 make
# the gate fully transparent regardless of the Output select value.
#
# We keep "Output select" in BYPASS_PARAMS for documentation purposes,
# but it is filtered out by _format_params at format time.
BYPASS_PARAMS = {
    "gate": {
        # "Output select" is filtered out by _format_params (= signs break SPA-JSON).
        # The gate stays in gate mode (LADSPA default 0), but the two values below
        # make it fully transparent (always open, no gain reduction).
        "Output select (-1 = key listen, 0 = gate, 1 = bypass)": 1.0,  # bypass mode (documented; filtered at format time)
        "Range (dB)": 0.0,       # no gain reduction even if gate closes
        "Threshold (dB)": -100.0,  # gate always open (signal always above -100 dB)
    },
    "noise_cancel": {"Dry Mix": 1.0},  # 1.0 = pure dry passthrough
    "eq": {  # All bands flat (0.0 dB)
        "50Hz gain (low shelving)": 0.0,
        "100Hz gain": 0.0,
        "156Hz gain": 0.0,
        "220Hz gain": 0.0,
        "311Hz gain": 0.0,
        "440Hz gain": 0.0,
        "622Hz gain": 0.0,
        "880Hz gain": 0.0,
        "1250Hz gain": 0.0,
        "1750Hz gain": 0.0,
        "2500Hz gain": 0.0,
        "3500Hz gain": 0.0,
        "5000Hz gain": 0.0,
        "10000Hz gain": 0.0,
        "20000Hz gain": 0.0,
    },
    "tube": {"Distortion level": 0.0},
    "compressor": {"Ratio (1:n)": 1.0},  # 1:1 = no compression
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

        # Effects Configuration (Inputs and Outputs)
        # Structure: { "effect_name": { "active": bool, "params": { ... } } }
        # Note: gate and noise_cancel are only meaningful for inputs, but the
        # dict is always populated so that from_dict / to_dict remain uniform.
        self.effects = {
            "gate": {"active": False, "params": copy.deepcopy(DEFAULT_EFFECT_PARAMS["gate"])},
            "noise_cancel": {"active": False, "params": copy.deepcopy(DEFAULT_EFFECT_PARAMS["noise_cancel"])},
            "eq": {"active": False, "params": copy.deepcopy(DEFAULT_EFFECT_PARAMS["eq"])},
            "tube": {"active": False, "params": copy.deepcopy(DEFAULT_EFFECT_PARAMS["tube"])},
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
        known_keys = ["gate", "noise_cancel", "eq", "tube", "compressor"]
        
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

                # --- EQ Port Name Migration (BEFORE default-filling) ---
                # Old config files used short names like "50Hz", "100Hz", etc.
                # The LADSPA mbeq_1197 plugin requires the full port names like
                # "50Hz gain (low shelving)", "100Hz gain", etc.
                # This must run BEFORE the default-filling loop below, otherwise
                # the new keys would be added with default values (0.0) and the
                # migration condition `new_key not in params` would be False,
                # causing old user values to be silently lost.
                if key == "eq":
                    EQ_KEY_MIGRATION = {
                        "50Hz": "50Hz gain (low shelving)",
                        "100Hz": "100Hz gain",
                        "156Hz": "156Hz gain",
                        "220Hz": "220Hz gain",
                        "311Hz": "311Hz gain",
                        "440Hz": "440Hz gain",
                        "622Hz": "622Hz gain",
                        "880Hz": "880Hz gain",
                        "1250Hz": "1250Hz gain",
                        "1750Hz": "1750Hz gain",
                        "2500Hz": "2500Hz gain",
                        "3500Hz": "3500Hz gain",
                        "5000Hz": "5000Hz gain",
                        "10000Hz": "10000Hz gain",
                        "20000Hz": "20000Hz gain",
                    }
                    for old_key, new_key in EQ_KEY_MIGRATION.items():
                        if old_key in params and new_key not in params:
                            params[new_key] = params.pop(old_key)

                # --- Compressor Port Name Migration (BEFORE default-filling) ---
                # Old config used short names; sc4_1882 requires the exact
                # LADSPA control port names.
                if key == "compressor":
                    COMPRESSOR_KEY_MIGRATION = {
                        "Threshold (dB)": "Threshold level (dB)",
                        "Attack (ms)": "Attack time (ms)",
                        "Release (ms)": "Release time (ms)",
                        "Makeup Gain (dB)": "Makeup gain (dB)",
                    }
                    for old_key, new_key in COMPRESSOR_KEY_MIGRATION.items():
                        if old_key in params and new_key not in params:
                            params[new_key] = params.pop(old_key)

                # --- Gate Port Name Migration (BEFORE default-filling) ---
                # Old config used "Release (ms)" but gate_1410 exposes
                # "Decay (ms)" instead.
                if key == "gate":
                    GATE_KEY_MIGRATION = {
                        "Release (ms)": "Decay (ms)",
                    }
                    for old_key, new_key in GATE_KEY_MIGRATION.items():
                        if old_key in params and new_key not in params:
                            params[new_key] = params.pop(old_key)

                # --- RNNoise Param Migration (BEFORE default-filling) ---
                # Old config used a single "Model" placeholder param. The real
                # librnnoise_ladspa plugin exposes VAD Threshold, VAD Grace
                # Period, Retroactive VAD Grace, and Dry Mix. Remove the stale
                # "Model" key so the default-filling loop below adds the real
                # port names with their proper defaults.
                if key == "noise_cancel":
                    if "Model" in params:
                        params.pop("Model")

                # Ensure missing params are filled with defaults (e.g. if we
                # added new controls or migration left gaps).
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