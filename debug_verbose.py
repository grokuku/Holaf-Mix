import subprocess
import os
import sys

# On force le flush pour voir les logs en temps réel
sys.stdout.reconfigure(line_buffering=True)

PLUGIN_PATH = "/usr/lib/ladspa/gate_1410.so"

def run_debug_test():
    print("--- 1. TEST DE BASE (Null Sink avec Props) ---")
    # On teste si on peut passer des arguments JSON simples
    base_conf = '{ node.name = "Test_Base_Args" node.description = "Test Args" }'
    cmd_base = ['pw-cli', 'load-module', 'libpipewire-module-null-sink', base_conf]
    
    res = subprocess.run(cmd_base, capture_output=True, text=True)
    print(f"CMD: {' '.join(cmd_base)}")
    print(f"RET: {res.returncode}")
    print(f"OUT: '{res.stdout.strip()}'")
    print(f"ERR: '{res.stderr.strip()}'")
    
    if res.stdout.strip().isdigit():
        subprocess.run(['pw-cli', 'destroy', res.stdout.strip()])

    print("\n--- 2. TEST FILTER CHAIN (DEBUG MODE) ---")
    
    graph = (
        f'{{ '
        f'nodes = [ {{ type = ladspa name = "DBG_GATE" plugin = "{PLUGIN_PATH}" label = "gate" }} ] '
        f'inputs = [ "DBG_GATE:Input" ] '
        f'outputs = [ "DBG_GATE:Output" ] '
        f'}}'
    )
    
    config = (
        f'{{ '
        f'node.name = "Test_FX_Debug" '
        f'filter.graph = {graph} '
        f'capture.props = {{ node.passive = true audio.channels = 1 audio.position = [ FL ] }} '
        f'playback.props = {{ media.class = Audio/Source audio.channels = 1 audio.position = [ FL ] }} '
        f'}}'
    )
    
    # On met tout à plat
    config_flat = config.replace('\n', ' ')
    
    # On prépare l'environnement avec DEBUG activé
    env = os.environ.copy()
    env["PIPEWIRE_DEBUG"] = "4" # Niveau très bavard
    env["LADSPA_PATH"] = "/usr/lib/ladspa"
    
    cmd_fx = ['pw-cli', 'load-module', 'libpipewire-module-filter-chain', config_flat]
    
    print("Lancement de la commande avec PIPEWIRE_DEBUG=4...")
    # On ne capture pas l'output cette fois, on laisse tout sortir sur la console
    # pour être sûr de ne rien rater
    try:
        subprocess.run(cmd_fx, env=env, check=False)
    except Exception as e:
        print(f"Exception Python: {e}")

if __name__ == "__main__":
    if not os.path.exists(PLUGIN_PATH):
        print(f"❌ Plugin {PLUGIN_PATH} introuvable!")
    else:
        run_debug_test()
