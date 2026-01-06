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
    output = _run_command(['pactl', 'list', 'sources', 'short'])
    if not output: return None
    
    for line in output.splitlines():
        parts = line.split()
        if len(parts) >= 2:
            try:
                p_id = int(parts[0])
                p_name = parts[1]
                if p_name == target_name:
                    return p_id
            except ValueError:
                continue
    return None

def get_audio_nodes(include_internal=False):
    """
    Retrieves list of Audio Nodes.
    :param include_internal: If True, returns ALL nodes (including Holaf strips and monitors).
                             If False (default), filters them out for clean UI.
    """
    nodes = []
    
    # --- 1. GET SINKS (Outputs) ---
    sinks_out = _run_command(['pactl', '-f', 'json', 'list', 'sinks'])
    if sinks_out:
        try:
            sinks = json.loads(sinks_out)
            for s in sinks:
                name = s.get('name', 'Unknown')
                
                # FILTER: Ignore our own virtual strips ONLY if include_internal is False
                if not include_internal and "Holaf_Strip" in name: continue
                
                # CLEANUP: Get best description
                desc = s.get('description')
                props = s.get('properties', {})
                if not desc or desc == "(null)":
                    desc = props.get('device.description') or props.get('node.nick') or name

                pw_id = int(props.get('pipewire.node.id', 0))
                final_id = pw_id if pw_id > 0 else s.get('index')
                
                nodes.append({
                    'id': final_id, 
                    'name': name,
                    'description': desc,
                    'media_class': 'Audio/Sink',
                    'volume': s.get('volume'),
                    'mute': s.get('mute')
                })
        except json.JSONDecodeError:
            pass

    # --- 2. GET SOURCES (Inputs/Mics) ---
    sources_out = _run_command(['pactl', '-f', 'json', 'list', 'sources'])
    if sources_out:
        try:
            sources = json.loads(sources_out)
            for s in sources:
                name = s.get('name', 'Unknown')
                props = s.get('properties', {})
                
                if not include_internal:
                    # FILTER 1: Ignore our own virtual monitors
                    if "Holaf_Strip" in name: continue
                    
                    # FILTER 2: Ignore "Monitor of..."
                    if s.get('monitor_of_sink') is not None: continue
                    if "monitor" in name.lower() and "source" not in name.lower(): continue

                # CLEANUP description
                desc = s.get('description')
                if not desc or desc == "(null)":
                    desc = props.get('device.description') or props.get('node.nick') or name

                pw_id = int(props.get('pipewire.node.id', 0))
                final_id = pw_id if pw_id > 0 else s.get('index')
                
                nodes.append({
                    'id': final_id,
                    'name': name,
                    'description': desc,
                    'media_class': 'Audio/Source',
                    'volume': s.get('volume'),
                    'mute': s.get('mute')
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