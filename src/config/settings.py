import json
import os
from src.models.strip_model import Strip, StripType, StripMode

CONFIG_FILE = 'config.json'

def create_default_config():
    """
    Creates the default setup for a first run.
    """
    desktop_input = Strip(
        label="Desktop",
        kind=StripType.INPUT,
        mode=StripMode.VIRTUAL
    )
    
    main_speakers = Strip(
        label="Speakers",
        kind=StripType.OUTPUT,
        mode=StripMode.PHYSICAL
    )
    
    return [desktop_input, main_speakers]

def _load_raw_json():
    """Helper to load the raw JSON dict."""
    if not os.path.exists(CONFIG_FILE):
        return {}
    try:
        with open(CONFIG_FILE, 'r') as f:
            return json.load(f)
    except (json.JSONDecodeError, IOError):
        return {}

def _save_raw_json(data):
    """Helper to save the raw JSON dict."""
    try:
        with open(CONFIG_FILE, 'w') as f:
            json.dump(data, f, indent=4)
    except IOError as e:
        print(f"Error saving to {CONFIG_FILE}: {e}")

def load_config():
    """
    Loads the configuration from config.json.
    """
    data = _load_raw_json()
    if not data:
        defaults = create_default_config()
        save_config(defaults)
        return defaults
    
    strips_data = data.get('strips', [])
    if not strips_data and 'strips' not in data: 
         defaults = create_default_config()
         save_config(defaults)
         return defaults

    strips = []
    for s_dict in strips_data:
        try:
            strip = Strip.from_dict(s_dict)
            strips.append(strip)
        except Exception as e:
            print(f"Error loading a strip: {e}. Skipping.")
            
    return strips

def save_config(strips):
    """
    Saves the list of Strip objects to config.json.
    """
    data = _load_raw_json()
    data['strips'] = [s.to_dict() for s in strips]
    _save_raw_json(data)