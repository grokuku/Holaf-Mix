import subprocess
import json
import time

def _run_pw_command(command_args):
    """Helper to run pw-cli commands."""
    try:
        result = subprocess.run(
            command_args,
            capture_output=True,
            text=True,
            check=True
        )
        return result.stdout.strip()
    except FileNotFoundError:
        print(f"Error: '{command_args[0]}' command not found. Please ensure PipeWire tools are installed.")
        return None
    except subprocess.CalledProcessError as e:
        # print(f"Error running '{' '.join(command_args)}': {e}")
        return None

def get_pw_objects():
    """
    Runs pw-dump and returns the parsed JSON output.
    """
    output = _run_pw_command(['pw-dump'])
    if output:
        try:
            return json.loads(output)
        except json.JSONDecodeError:
            pass
    return None

def get_node_info(node_id: int):
    """
    Retrieves detailed information for a single PipeWire node by ID.
    """
    pw_objects = get_pw_objects()
    if not pw_objects:
        return None
    
    for obj in pw_objects:
        if obj.get('id') == node_id and obj.get('type') == 'PipeWire:Interface:Node':
            return obj
    return None

def _extract_volume_mute(obj):
    """Helper to extract volume and mute from an object's properties."""
    volume = None
    mute = None
    
    # Strategy 1: info.props
    props = obj.get('info', {}).get('props', {})
    if 'volume' in props: volume = props['volume']
    if 'mute' in props: mute = props['mute']

    # Strategy 2: info.params
    if 'info' in obj and 'params' in obj['info']:
        for param_list in obj['info']['params'].values():
            if isinstance(param_list, list):
                for p in param_list:
                    if isinstance(p, dict):
                        if 'volume' in p and volume is None: volume = p['volume']
                        if 'mute' in p and mute is None: mute = p['mute']
                        if 'Props' in p and isinstance(p['Props'], list):
                             # Nested Props handling could go here
                             pass
    return volume, mute

def get_audio_nodes():
    """
    Retrieves a list of PipeWire audio nodes (Sinks and Sources).
    """
    pw_objects = get_pw_objects()
    if not pw_objects:
        return []

    audio_nodes = []
    for obj in pw_objects:
        if obj.get('type') == 'PipeWire:Interface:Node' and 'info' in obj:
            props = obj['info'].get('props', {})
            media_class = props.get('media.class')

            # We are interested in Sinks (Outputs) and Sources (Inputs/Mics)
            if media_class in ["Audio/Sink", "Audio/Source"]:
                node_info = {
                    'id': obj['id'],
                    'name': props.get('node.name', 'N/A'),
                    'description': props.get('node.description', props.get('media.name', 'N/A')),
                    'media_class': media_class,
                    'application_name': props.get('application.name', 'N/A'),
                }
                
                volume, mute = _extract_volume_mute(obj)
                node_info['volume'] = volume
                node_info['mute'] = mute
                
                audio_nodes.append(node_info)
    return audio_nodes

def get_sink_inputs():
    """
    Retrieves a list of applications currently playing audio (Sink Inputs).
    Uses 'pactl list sink-inputs' parsing or pw-dump filtering.
    Using pw-dump is cleaner for consistency.
    """
    pw_objects = get_pw_objects()
    if not pw_objects:
        return []

    apps = []
    for obj in pw_objects:
        if obj.get('type') == 'PipeWire:Interface:Node' and 'info' in obj:
            props = obj['info'].get('props', {})
            media_class = props.get('media.class')
            
            # Stream/Output/Audio represents an App playing sound
            if media_class == "Stream/Output/Audio":
                # Find which node it is connected to (Target)
                # In PipeWire, this is often stored in 'node.target' prop, or we have to look at links.
                # However, for the UI list, we primarily need the App Name and ID.
                
                app_name = props.get('application.name') or props.get('node.name', 'Unknown App')
                # Filter out our own streams if any (Monitor)
                if app_name == "Holaf-Mix": continue

                apps.append({
                    'id': obj['id'],
                    'name': app_name,
                    'icon': props.get('application.icon_name'),
                    'target_node': props.get('node.target'), # ID of the sink it's on
                })
    return apps

def set_node_volume(node_id: int, volume: float):
    """Sets volume via pw-cli (Fallback)."""
    volume = max(0.0, min(1.0, volume))
    # Simple JSON for Props
    volume_json = json.dumps({"volume": volume})
    command = ['pw-cli', 'set-param', str(node_id), 'Props', volume_json]
    _run_pw_command(command)

def toggle_node_mute(node_id: int, mute: bool):
    """Sets mute via pw-cli (Fallback)."""
    mute_json = json.dumps({"mute": mute})
    command = ['pw-cli', 'set-param', str(node_id), 'Props', mute_json]
    _run_pw_command(command)

def move_sink_input(app_node_id: int, target_sink_name: str):
    """
    Moves a running application (sink-input) to a specific Sink.
    Uses pactl because it's the most reliable way to 'move' a stream 
    preserving the state.
    
    Note: 'pactl list sink-inputs' gives us Pulse IDs, but 'app_node_id' 
    from pw-dump is a PipeWire ID. They are usually mapped, but to be safe, 
    we might need to map PW ID to Pulse ID.
    
    Fortunatly, 'pactl move-sink-input' often accepts the PipeWire Node ID 
    if pipewire-pulse is handling things correctly. Let's try direct ID.
    """
    try:
        # Try moving using the Node ID directly
        subprocess.run(['pactl', 'move-sink-input', str(app_node_id), target_sink_name], check=True, capture_output=True)
        return True
    except subprocess.CalledProcessError:
        return False