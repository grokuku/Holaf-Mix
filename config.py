import json
import os

CONFIG_FILE = 'config.json'

DEFAULT_CONFIG = {
    "midi_input_device": None,
    "mappings": {
        # 'node_id': { 'volume': {'type': 'control_change', 'channel': 1, 'control': 22}, 'mute': {'type': 'note_on', 'channel': 1, 'note': 60} }
    }
}

def load_config():
    """
    Loads the configuration from config.json.
    If the file doesn't exist, it creates a default one.
    """
    if not os.path.exists(CONFIG_FILE):
        save_config(DEFAULT_CONFIG)
        return DEFAULT_CONFIG
    
    try:
        with open(CONFIG_FILE, 'r') as f:
            return json.load(f)
    except (json.JSONDecodeError, IOError) as e:
        print(f"Error loading {CONFIG_FILE}: {e}. Loading default config.")
        # In case of a corrupted file, load defaults
        return DEFAULT_CONFIG

def save_config(config_data):
    """
    Saves the provided configuration data to config.json.
    """
    try:
        with open(CONFIG_FILE, 'w') as f:
            json.dump(config_data, f, indent=4)
    except IOError as e:
        print(f"Error saving to {CONFIG_FILE}: {e}")

if __name__ == "__main__":
    # Test functions
    print("Loading configuration...")
    config = load_config()
    print("Current config:", config)

    # Example of modifying and saving
    config['midi_input_device'] = "MIDI Mix:MIDI Mix MIDI 1 32:0"
    config['mappings']['97'] = {
        'volume': {'type': 'control_change', 'channel': 0, 'control': 21}
    }
    print("\nSaving new example configuration...")
    save_config(config)

    print("Re-loading configuration to verify...")
    new_config = load_config()
    print("New config:", new_config)

    # Clean up by restoring default
    print("\nRestoring default configuration...")
    save_config(DEFAULT_CONFIG)
    print("Default config restored.")
