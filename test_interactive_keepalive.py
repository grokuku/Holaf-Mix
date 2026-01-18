import subprocess
import time
import os
import json

PLUGIN_PATH = "/usr/lib/ladspa/gate_1410.so"

def run_test():
    print("--- TEST INTERACTIVE KEEPALIVE ---")
    
    # 1. Préparer la commande JSON (Aplatit)
    graph = (
        f'{{ '
        f'nodes = [ {{ type = ladspa name = "ALIVE_GATE" plugin = "{PLUGIN_PATH}" label = "gate" }} ] '
        f'inputs = [ "ALIVE_GATE:Input" ] '
        f'outputs = [ "ALIVE_GATE:Output" ] '
        f'}}'
    )
    
    config = (
        f'{{ '
        f'node.name = "Holaf_Alive_FX" '
        f'filter.graph = {graph} '
        f'capture.props = {{ node.passive = true audio.channels = 1 audio.position = [ FL ] }} '
        f'playback.props = {{ media.class = Audio/Source audio.channels = 1 audio.position = [ FL ] }} '
        f'}}'
    )
    
    config_flat = config.replace('\n', ' ')
    cmd_str = f"load-module libpipewire-module-filter-chain {config_flat}\n"

    print("Lancement de pw-cli (arrière-plan)...")
    # On ouvre pw-cli et on garde le pipe ouvert
    proc = subprocess.Popen(
        ['pw-cli'], 
        stdin=subprocess.PIPE, 
        stdout=subprocess.PIPE, 
        stderr=subprocess.PIPE, 
        text=True,
        bufsize=1 # Line buffered
    )
    
    print(f"PID: {proc.pid}")
    
    print("Envoi de la commande...")
    proc.stdin.write(cmd_str)
    proc.stdin.flush()
    
    # On ne fait PAS proc.communicate() ici car ça attendrait la fin !
    
    print("Attente de 2 secondes (laisser le module charger)...")
    time.sleep(2)
    
    print("Vérification existence via pw-dump...")
    res = subprocess.run(['pw-dump'], capture_output=True, text=True)
    if "Holaf_Alive_FX" in res.stdout:
        print("✅ VICTOIRE ! Le noeud est vivant car pw-cli est toujours ouvert.")
    else:
        print("❌ ECHEC. Regardons si pw-cli est mort...")
        if proc.poll() is not None:
             print(f"pw-cli est mort avec le code {proc.returncode}.")
             # Lire stderr pour comprendre
             print(f"STDERR: {proc.stderr.read()}")
        else:
             print("pw-cli est vivant mais le noeud n'est pas là. Problème de syntaxe ?")
             # On tente de lire un peu la sortie sans bloquer (compliqué en python simple, mais essayons)
    
    print("Fermeture propre...")
    proc.terminate()

if __name__ == "__main__":
    if not os.path.exists(PLUGIN_PATH):
        print("Plugin manquant.")
    else:
        run_test()
