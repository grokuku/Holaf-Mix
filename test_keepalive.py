import subprocess
import time
import os
import signal

PLUGIN_PATH = "/usr/lib/ladspa/gate_1410.so"

def run_test():
    print("--- TEST KEEPALIVE ---")
    
    graph = (
        f'{{ '
        f'nodes = [ {{ type = ladspa name = "KEEPALIVE_GATE" plugin = "{PLUGIN_PATH}" label = "gate" }} ] '
        f'inputs = [ "KEEPALIVE_GATE:Input" ] '
        f'outputs = [ "KEEPALIVE_GATE:Output" ] '
        f'}}'
    )
    
    config = (
        f'{{ '
        f'node.name = "Holaf_KeepAlive_FX" '
        f'filter.graph = {graph} '
        f'capture.props = {{ node.passive = true audio.channels = 1 audio.position = [ FL ] }} '
        f'playback.props = {{ media.class = Audio/Source audio.channels = 1 audio.position = [ FL ] }} '
        f'}}'
    )
    
    config_flat = config.replace('\n', ' ')
    
    # ASTUCE : On ne charge pas un module via pw-cli load-module (qui quitte).
    # On lance une instance pipewire minimale qui charge juste ce module ? 
    # Non, trop compliqué.
    
    # On va utiliser "pipewire-filter-chain" (si installé) ou un trick.
    # Mais attendez, pw-cli PEUT charger des modules côté serveur s'ils sont faits pour.
    # Apparemment filter-chain ne l'est pas.
    
    # Essayons de lancer pw-cli et de NE PAS le fermer ?
    # pw-cli ne reste pas ouvert après une commande non-interactive.
    
    # Donc on lance un processus qui execute un script de filtre.
    # Le binaire pour ça est `pipewire -c <fichier_conf>`.
    # C'est la méthode officielle pour les filtres complexes persistants hors-daemon.
    
    # Créons un fichier temporaire de config
    conf_content = f"""
    context.modules = [
        {{ name = libpipewire-module-filter-chain
            args = {config_flat}
        }}
    ]
    """
    
    with open("temp_fx.conf", "w") as f:
        f.write(conf_content)
        
    print("Lancement du processus PipeWire dédié...")
    # On lance pipewire avec cette config
    proc = subprocess.Popen(['pipewire', '-c', 'temp_fx.conf'])
    
    print(f"Process PID: {proc.pid}")
    time.sleep(2)
    
    print("Vérification existence...")
    res = subprocess.run(['pw-dump'], capture_output=True, text=True)
    if "Holaf_KeepAlive_FX" in res.stdout:
        print("✅ VICTOIRE ! Le noeud est vivant tant que le process est vivant.")
    else:
        print("❌ ECHEC.")
        
    print("Fermeture du process...")
    proc.terminate()
    proc.wait()
    
    time.sleep(1)
    os.remove("temp_fx.conf")

if __name__ == "__main__":
    run_test()
