import json
import os
import sys
import tempfile
from src.models.strip_model import Strip, StripType, StripMode


def _resolve_config_path() -> str:
    """
    Resolve the path to config.json, handling both source-tree and
    PyInstaller-frozen (onefile/onedir) execution modes.

    - Source / dev mode:  <repo-root>/config.json  (alongside main.py)
    - Frozen (PyInstaller --onefile):  next to sys.executable
      (writable, since onefile extracts _MEI* to a temp dir, so the
      cwd-relative config.json inside the bundle is read-only)
    - Frozen (PyInstaller --onedir):  same as onefile — next to the
      executable, so config survives across runs and upgrades.
    """
    if getattr(sys, "frozen", False):
        # Running inside a PyInstaller bundle
        base_dir = os.path.dirname(os.path.abspath(sys.executable))
    else:
        # Running as a normal Python script: <repo-root>/config.json
        # settings.py lives at <root>/src/config/settings.py, so the
        # repo root is THREE levels up from this file.
        base_dir = os.path.dirname(
            os.path.dirname(
                os.path.dirname(os.path.abspath(__file__))
            )
        )

    return os.path.join(base_dir, "config.json")


CONFIG_FILE = _resolve_config_path()

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
    """
    Atomic save: write to a temp file in the same directory, then rename.
    os.replace() is atomic on POSIX, so a crash mid-write cannot corrupt
    the existing config.json (it stays untouched until the rename).
    """
    config_dir = os.path.dirname(os.path.abspath(CONFIG_FILE)) or "."
    try:
        fd, tmp_path = tempfile.mkstemp(dir=config_dir, prefix=".config_", suffix=".tmp")
        try:
            with os.fdopen(fd, 'w') as f:
                json.dump(data, f, indent=4)
            os.replace(tmp_path, CONFIG_FILE)
        except Exception:
            # Clean up the temp file on any failure
            try:
                os.unlink(tmp_path)
            except OSError:
                pass
            raise
    except IOError as e:
        print(f"Error saving to {CONFIG_FILE}: {e}")

def load_config():
    """
    Loads the configuration from config.json.
    Returns a tuple: (list_of_strips, window_geometry_bytes_or_None)
    """
    data = _load_raw_json()
    
    # 1. Load Strips
    strips = []
    strips_data = data.get('strips', [])
    
    if not strips_data and 'strips' not in data: 
         strips = create_default_config()
         # We don't save immediately here to avoid partial file writes, 
         # but the app calls save eventually.
    else:
        for s_dict in strips_data:
            try:
                strip = Strip.from_dict(s_dict)
                strips.append(strip)
            except Exception as e:
                print(f"Error loading a strip: {e}. Skipping.")
    
    if not strips:
        strips = create_default_config()

    # 2. Load Window Geometry (hex string)
    window_geo = data.get('window_geometry')
    
    return strips, window_geo

def save_config(strips, window_geometry_hex=None):
    """
    Saves the list of Strip objects and window state to config.json.
    """
    data = _load_raw_json()
    
    data['strips'] = [s.to_dict() for s in strips]
    
    if window_geometry_hex:
        data['window_geometry'] = window_geometry_hex
        
    _save_raw_json(data)