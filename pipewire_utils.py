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
        print(f"Error running '{' '.join(command_args)}': {e}")
        # print(f"Stderr: {e.stderr}") # SILENCED
        return None

def get_pw_objects():
    """
    Runs pw-dump and returns the parsed JSON output.
    """
    output = _run_pw_command(['pw-dump'])
    if output:
        try:
            return json.loads(output)
        except json.JSONDecodeError as e:
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
    
    props = obj.get('info', {}).get('props', {})
    if 'volume' in props:
        volume = props['volume']
    if 'mute' in props:
        mute = props['mute']

    # Check info.params for volume/mute (more common for nodes)
    if 'info' in obj and 'params' in obj['info']:
        for param_type, param_list in obj['info']['params'].items():
            if isinstance(param_list, list):
                for param_item in param_list:
                    if isinstance(param_item, dict):
                        if 'volume' in param_item and volume is None:
                            volume = param_item['volume']
                        if 'mute' in param_item and mute is None:
                            mute = param_item['mute']
                        if 'Props' in param_item and isinstance(param_item['Props'], list):
                            for p in param_item['Props']:
                                if isinstance(p, dict):
                                    if 'volume' in p and volume is None:
                                        volume = p['volume']
                                    if 'mute' in p and mute is None:
                                        mute = p['mute']
            elif isinstance(param_list, dict) and param_type == 'Props': 
                 if 'volume' in param_list and volume is None:
                    volume = param_list['volume']
                 if 'mute' in param_list and mute is None:
                    mute = param_list['mute']

    return volume, mute


def get_audio_nodes():
    """
    Retrieves a list of PipeWire audio nodes.
    """
    pw_objects = get_pw_objects()
    if not pw_objects:
        return []

    audio_nodes = []
    for obj in pw_objects:
        if obj.get('type') == 'PipeWire:Interface:Node' and 'info' in obj:
            props = obj['info'].get('props', {})
            media_class = props.get('media.class')

            if media_class in ["Audio/Sink", "Audio/Source", "Stream/Output/Audio", "Stream/Input/Audio"]:
                node_info = {
                    'id': obj['id'],
                    'name': props.get('node.name', 'N/A'),
                    'description': props.get('node.description', props.get('media.name', 'N/A')),
                    'media_class': media_class,
                    'application_name': props.get('application.name', 'N/A'),
                    'channels': props.get('audio.channels', 2), 
                }
                
                volume, mute = _extract_volume_mute(obj)
                node_info['volume'] = volume
                node_info['mute'] = mute
                
                audio_nodes.append(node_info)
    return audio_nodes

def set_node_volume(node_id: int, volume: float):
    """
    Sets the volume of a specific PipeWire node.
    """
    volume = max(0.0, min(1.0, volume))

    node_detail = get_node_info(node_id)
    if not node_detail:
        return

    num_channels = node_detail['info']['props'].get('audio.channels', 2)
    
    channel_volumes = [volume] * num_channels
    volume_json = json.dumps({"channelVolumes": channel_volumes})
    command = ['pw-cli', 'set-param', str(node_id), 'Props', volume_json]
    
    result_stdout = _run_pw_command(command)
    if result_stdout is None:
        volume_json = json.dumps({"volume": volume})
        command = ['pw-cli', 'set-param', str(node_id), 'Props', volume_json]
        _run_pw_command(command)
    
    # SILENCED: print(f"Set volume for node {node_id} to {volume}")

def toggle_node_mute(node_id: int, mute: bool):
    """
    Sets the mute state of a specific PipeWire node.
    """
    mute_json = json.dumps({"mute": mute})
    command = ['pw-cli', 'set-param', str(node_id), 'Props', mute_json]
    _run_pw_command(command)
    # SILENCED: print(f"Set mute for node {node_id} to {mute}")