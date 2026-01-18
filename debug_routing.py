import subprocess
import json

def get_lines(cmd):
    try:
        res = subprocess.run(cmd, capture_output=True, text=True)
        return [l.strip() for l in res.stdout.splitlines() if l.strip()]
    except:
        return []

def run_debug():
    print("=== DIAGNOSTIC ROUTAGE HOLAF ===")
    
    # 1. Identifier les noeuds FX
    print("\n--- 1. Recherche des Noeuds FX ---")
    nodes = get_lines(['pw-link', '-o', '-l'])
    fx_nodes = [n for n in nodes if "Holaf_FX" in n]
    
    if not fx_nodes:
        print("❌ AUCUN PORT DE SORTIE FX TROUVÉ !")
        print("   Cela veut dire que le module d'effet ne tourne pas ou a crashé.")
    else:
        print(f"✅ Ports FX trouvés :")
        for p in fx_nodes:
            print(f"   - {p}")

    # 2. Analyser les liens
    print("\n--- 2. Analyse des Liens (Qui va où ?) ---")
    # On récupère tous les liens
    links = get_lines(['pw-cli', 'info', 'all']) 
    # C'est trop verbeux, on va utiliser pw-link qui liste les liens graphiquement si on ne met pas d'args, 
    # mais pw-link liste les ports disponibles.
    # Pour voir les liens : pw-dump est mieux.
    
    dump_res = subprocess.run(['pw-dump'], capture_output=True, text=True)
    try:
        data = json.loads(dump_res.stdout)
    except:
        print("Erreur parsing JSON pw-dump")
        return

    # On cherche les objets de type PipeWire:Interface:Link
    links_found = []
    
    # Construire un map ID -> Name pour la lisibilité
    id_to_name = {}
    for obj in data:
        oid = obj['id']
        props = obj.get('info', {}).get('props', {})
        name = props.get('node.name') or props.get('port.name') or props.get('object.path') or str(oid)
        # Si c'est un port, on veut souvent "NodeName:PortName"
        if obj['type'] == 'PipeWire:Interface:Port':
             # On doit trouver le noeud parent, compliqué via dump simple sans map
             pass
        id_to_name[oid] = name

    # Une méthode plus simple : pw-link (sans args) ne liste pas les liens.
    # On va parser la sortie de `pw-dot` ou simplement regarder les ports FX spécifiques.
    
    # Approche directe : Regarder les liens sur le FX
    for fx_port in fx_nodes:
        # fx_port ressemble à "output.Holaf_FX_UID:output_0"
        # On demande à qui il est lié
        print(f"\nLiens pour {fx_port} :")
        # pw-link ne donne pas les connections inversées facilement en CLI simple.
        # On va lister TOUS les liens du système et filtrer.
        # Format pw-link -L : "OutputPort -> InputPort"
        all_links = get_lines(['pw-link', '-L'])
        related = [l for l in all_links if fx_port in l]
        if related:
            for r in related:
                print(f"   🔗 {r}")
        else:
            print("   ⚠️ NON CONNECTÉ (Le son ne sort pas d'ici)")

    print("\n--- 3. Vérification Entrée Micro ---")
    # On cherche les ports d'entrée du FX
    input_nodes = get_lines(['pw-link', '-i', '-l'])
    fx_inputs = [n for n in input_nodes if "Holaf_FX" in n]
    for fx_in in fx_inputs:
        print(f"\nLiens pour {fx_in} :")
        all_links = get_lines(['pw-link', '-L'])
        related = [l for l in all_links if fx_in in l]
        if related:
            for r in related:
                print(f"   🔗 {r}")
        else:
            print("   ⚠️ NON CONNECTÉ (Le micro n'entre pas dans l'effet)")

if __name__ == "__main__":
    run_debug()
