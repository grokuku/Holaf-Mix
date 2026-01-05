import json
import os
from src.models.strip_model import Strip, StripType, StripMode

CONFIG_FILE = 'config.json'

def create_default_config():
    """
    Creates the default setup for a first run:
    1. Input Strip: 'Desktop' (Virtual input acting as default sink)
    2. Output Strip: 'Speakers' (Physical output)
    """
    # 1. Default Input (Captures all desktop audio)
    desktop_input = Strip(
        label="Desktop",
        kind=StripType.INPUT,
        mode=StripMode.VIRTUAL
    )
    # By default, we route this input to the first output (Speakers)
    # But since we don't know the Speaker's UID yet, we'll handle routing logic in the main controller.
    
    # 2. Default Output (Your physical speakers)
    main_speakers = Strip(
        label="Speakers",
        kind=StripType.OUTPUT,
        mode=StripMode.PHYSICAL
    )
    
    return [desktop_input, main_speakers]

def load_config():
    """
    Loads the configuration from config.json.
    Returns a list of Strip objects.
    """
    if not os.path.exists(CONFIG_FILE):
        print("Config file not found. Creating default setup.")
        defaults = create_default_config()
        save_config(defaults)
        return defaults
    
    try:
        with open(CONFIG_FILE, 'r') as f:
            data = json.load(f)
            
        strips_data = data.get('strips', [])
        strips = []
        for s_dict in strips_data:
            try:
                strip = Strip.from_dict(s_dict)
                strips.append(strip)
            except Exception as e:
                print(f"Error loading a strip: {e}. Skipping.")
                
        return strips
        
    except (json.JSONDecodeError, IOError) as e:
        print(f"Error loading {CONFIG_FILE}: {e}. Loading default config.")
        return create_default_config()

def save_config(strips):
    """
    Saves the list of Strip objects to config.json.
    """
    data = {
        'strips': [s.to_dict() for s in strips]
    }
    
    try:
        with open(CONFIG_FILE, 'w') as f:
            json.dump(data, f, indent=4)
        print(f"Configuration saved to {CONFIG_FILE}")
    except IOError as e:
        print(f"Error saving to {CONFIG_FILE}: {e}")