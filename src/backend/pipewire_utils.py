import subprocess
import json
import time

def _run_command(command_args):
    try:
        result = subprocess.run(
            command_args,
            capture_output=True,
            text=True,
            check=True
        )
        return result.stdout.strip()
    except Exception as e:
        return None

def find_monitor_id_by_name(target_name: str):
    """Finds a node ID by its monitor name using native discovery."""
    nodes = get_audio_nodes(include_internal=True)
    for node in nodes:
        if node.get('name') == target_name:
            return node['id']
        if node.get('monitor_source_name') == target_name:
            return node['id']
    return None

def get_audio_nodes(include_internal=False):
    """
    Retrieves list of Audio Nodes using native 'pw-dump'.
    Replaces pactl for instant discovery of new nodes (Effects/Virtual).
    """
    nodes = []
    dump_out = _run_command(['pw-dump'])
    if not dump_out:
        return []

    try:
        data = json.loads(dump_out)
        for obj in data:
            if obj.get('type') != "PipeWire:Interface:Node":
                continue
            
            props = obj.get('info', {}).get('props', {})
            media_class = props.get('media.class', '')
            
            # We only care about Audio Nodes (Sources, Sinks, and Streams)
            if "Audio" not in media_class:
                continue
                
            name = props.get('node.name')
            if not name:
                continue

            # FILTER: Internal nodes
            if not include_internal and "Holaf_" in name:
                continue

            # Logic to determine Source vs Sink in native PipeWire
            # Filter-chain input nodes have class 'Stream/Input/Audio'
            is_source = "Source" in media_class or "Stream/Input" in media_class
            
            # Filter out monitors from source list if not internal
            if not include_internal and is_source and "monitor" in name.lower():
                continue

            desc = props.get('node.description') or props.get('node.nick') or name
            
            nodes.append({
                'id': obj['id'], 
                'name': name,
                'description': desc,
                'media_class': 'Audio/Source' if is_source else 'Audio/Sink',
                'volume': 1.0, # Action commands like pactl set-volume handle this
                'mute': False,
                'monitor_source_name': f"{name}.monitor" if "Sink" in media_class else None
            })
    except json.JSONDecodeError:
        pass

    return nodes

def get_sink_inputs():
    try:
        result = subprocess.run(
            ['pactl', '-f', 'json', 'list', 'sink-inputs'],
            capture_output=True,
            text=True,
            check=True
        )
        if not result.stdout.strip(): return []
        sink_inputs = json.loads(result.stdout)
        apps = []
        for item in sink_inputs:
            if 'index' not in item: continue
            props = item.get('properties', {})
            app_name = props.get('application.name', 'Unknown App')
            
            if app_name == "Holaf-Mix": continue
            if "pw-record" in app_name: continue
            if "python" in app_name.lower(): continue 

            apps.append({
                'id': item['index'],
                'name': app_name,
                'icon': props.get('application.icon_name', ''),
                'target_node': item.get('sink'), 
            })
        return apps
    except Exception as e:
        print(f"Error fetching sink inputs: {e}")
        return []

def get_node_info(node_id: int):
    # For node lookup, we MUST include internal nodes to find our own strips
    all_nodes = get_audio_nodes(include_internal=True)
    for n in all_nodes:
        if n['id'] == node_id:
            return {'info': {'props': {'node.name': n['name']}}}
    return None

def set_node_volume(node_id: int, volume: float): pass 
def toggle_node_mute(node_id: int, mute: bool): pass

def move_sink_input(app_index: int, target_sink_name: str):
    try:
        subprocess.run(['pactl', 'move-sink-input', str(app_index), target_sink_name], check=True, capture_output=True)
        return True
    except subprocess.CalledProcessError:
        return False