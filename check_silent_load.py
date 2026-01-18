import subprocess
import json
import time
import os

PLUGIN_PATH = "/usr/lib/ladspa/gate_1410.so"

def get_node_id(name):
    # On cherche via pw-dump si le noeud existe
    try:
        res = subprocess.run(['pw-dump'], capture_output=True, text=True)
        data = json.loads(res.stdout)
        for obj in data:
            props = obj.get('info', {}).get('props', {})
            if props.get('node.name') == name:
                return obj['id']
    except Exception as e:
        print(f"Erreur dump: {e}")
    return None

def run_test():
    node_name = "Holaf_Silent_Test"
    print(f"--- Tentative de chargement de {node_name} ---")
    
    # Graphique simple (Gate)
    graph = (
        f'{{ '
        f'nodes = [ {{ type = ladspa name = "SILENT_GATE" plugin = "{PLUGIN_PATH}" label = "gate" }} ] '
        f'inputs = [ "SILENT_GATE:Input" ] '
        f'outputs = [ "SILENT_GATE:Output" ] '
        f'}}'
    )
    
    config_raw = (
        f'{{ '
        f'node.name = "{node_name}" '
        f'filter.graph = {graph} '
        f'capture.props = {{ node.passive = true audio.channels = 1 audio.position = [ FL ] }} '
        f'playback.props = {{ media.class = Audio/Source audio.channels = 1 audio.position = [ FL ] }} '
        f'}}'
    )
    
    config_flat = config_raw.replace('\n', ' ')
    
    cmd = ['pw-cli', 'load-module', 'libpipewire-module-filter-chain', config_flat]
    
    print("Exécution de pw-cli...")
    proc = subprocess.run(cmd, capture_output=True, text=True)
    
    print(f"Code retour: {proc.returncode}")
    print(f"STDOUT: '{proc.stdout.strip()}'")
    print(f"STDERR: '{proc.stderr.strip()}'")
    
    print("Vérification existence via pw-dump...")
    time.sleep(1.0) # Laisser une seconde à Pipewire
    found_id = get_node_id(node_name)
    
    if found_id:
        print(f"\n✅ LE NOEUD EXISTE ! ID: {found_id}")
        print("Conclusion: pw-cli est muet mais ça marche. Il faut adapter le code.")
        # Cleanup
        subprocess.run(['pw-cli', 'destroy', str(found_id)])
    else:
        print("\n❌ LE NOEUD N'EXISTE PAS.")
        print("Conclusion: Échec silencieux ou chargement différé.")

if __name__ == "__main__":
    if not os.path.exists(PLUGIN_PATH):
        print("Plugin manquant.")
    else:
        run_test()
