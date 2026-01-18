import subprocess
import os
import sys

PLUGIN_PATH = "/usr/lib/ladspa/gate_1410.so"

def run_command(name, cmd_list):
    print(f"\n--- {name} ---")
    print(f"CMD: {' '.join(cmd_list)}")
    try:
        # On utilise capture_output=True pour tout récupérer
        result = subprocess.run(cmd_list, capture_output=True, text=True)
        
        print(f"RETURN CODE: {result.returncode}")
        if result.stdout:
            print(f"STDOUT: {result.stdout.strip()}")
        if result.stderr:
            print(f"STDERR: {result.stderr.strip()}")
            
        return (result.returncode == 0)
    except Exception as e:
        print(f"EXCEPTION: {e}")
        return False

if __name__ == "__main__":
    if not os.path.exists(PLUGIN_PATH):
        print("❌ Plugin manquant (gate_1410.so)")
        sys.exit(1)

    # TEST A: Vérifier que pw-cli marche pour un truc simple
    # Equivalent à : pw-cli load-module libpipewire-module-null-sink '{ node.name = "Test_Simple" }'
    simple_conf = '{ node.name = "Test_Simple" }'
    run_command("TEST A: Simple Null Sink", ['pw-cli', 'load-module', 'libpipewire-module-null-sink', simple_conf])

    # TEST B: Filter Chain avec JSON "Aplatit" (Sans sauts de ligne)
    # C'est souvent les retours à la ligne qui cassent l'argument unique
    graph = (
        f'{{ '
        f'nodes = [ {{ type = ladspa name = "DEBUG_GATE" plugin = "{PLUGIN_PATH}" label = "gate" }} ] '
        f'inputs = [ "DEBUG_GATE:Input" ] '
        f'outputs = [ "DEBUG_GATE:Output" ] '
        f'}}'
    )
    
    # On construit la config finale et on supprime explicitement les \n
    config_raw = (
        f'{{ '
        f'node.name = "Holaf_Debug_FX_Flat" '
        f'filter.graph = {graph} '
        f'capture.props = {{ node.passive = true audio.channels = 1 audio.position = [ FL ] }} '
        f'playback.props = {{ media.class = Audio/Source audio.channels = 1 audio.position = [ FL ] }} '
        f'}}'
    )
    
    # .replace('\n', ' ') est la clé potentielle ici
    config_flat = config_raw.replace('\n', ' ')
    
    success = run_command("TEST B: Filter Chain (Flat JSON)", ['pw-cli', 'load-module', 'libpipewire-module-filter-chain', config_flat])
    
    if success:
        print("\n✅ VICTOIRE ! C'était les sauts de ligne ou le format.")
        # Nettoyage si ça a marché
        subprocess.run(['pw-cli', 'destroy', 'Holaf_Debug_FX_Flat'], capture_output=True)
