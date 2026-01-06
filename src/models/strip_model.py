import uuid

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
        
        # Routing Matrix (Only relevant for Input strips)
        # List of Output UIDs this strip sends audio to.
        self.routes = [] 
        
        # Hardware/PipeWire connection details
        # If kind=OUTPUT & mode=PHYSICAL: The name of the sink (Speakers)
        # If kind=INPUT & mode=PHYSICAL: The name of the source (Mic) to link FROM
        self.device_name = None 

        # Software Assignment (For Inputs)
        # List of application names (strings) assigned to this strip (e.g. ["Firefox", "Spotify"])
        self.assigned_apps = []

        # System Default Sink Flag
        self.is_default = False

        # MIDI Mapping configuration
        self.midi_volume = None 
        self.midi_mute = None

    def to_dict(self):
        """Serialize the object to a dictionary for JSON saving."""
        return {
            'uid': self.uid,
            'label': self.label,
            'kind': self.kind,
            'mode': self.mode,
            'volume': self.volume,
            'mute': self.mute,
            'routes': self.routes,
            'device_name': self.device_name,
            'assigned_apps': self.assigned_apps,
            'is_default': self.is_default,
            'midi_volume': self.midi_volume,
            'midi_mute': self.midi_mute
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
        strip.routes = data.get('routes', [])
        strip.device_name = data.get('device_name')
        strip.assigned_apps = data.get('assigned_apps', [])
        strip.is_default = data.get('is_default', False)
        strip.midi_volume = data.get('midi_volume')
        strip.midi_mute = data.get('midi_mute')
        
        return strip

    def __repr__(self):
        return f"<Strip '{self.label}' ({self.kind}) Vol:{self.volume}>"