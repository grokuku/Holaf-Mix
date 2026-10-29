# J1 — Phase 2 : spike pipewire-rs fonctionnel (holaf-spike)

Date : suite de `docs/j1-spike-recon.md`. Environnement : conteneur headless
(PipeWire **0.3.65** + WirePlumber 0.4.13), plugins LADSPA `/usr/lib/ladspa/`.

Livrable : crate `rust/crates/holaf-pw` (brique de la couche PipeWire, évolue
vers J2) + binaire `holaf-spike` qui prouve de bout en bout le scénario cœur du
mixer contre un vrai daemon.

## Ce qui a été tranché / découvert (à lire AVANT J2)

### A. Format `Props` du filter-chain = **plat**, PAS imbriqué par nœud
C'est la divergence la plus importante avec le Python actuel.

- `_build_fx_props` (Python) produit `{ "eq_L" = { <port> = <val> … } "eq_R" = { … } }`
  (imbriqué par nœud). **Sur 0.3.65 c'est un no-op silencieux** : `spa_json_to_pod`
  génère un objet `Props` vide (les clés `eq_L`/`eq_R` ne sont pas des clés SPA),
  la taille du pod imprimé est `size 8` (en-tête seul). Aucun contrôle n'est changé.
- Le code `module-filter-chain.c` (0.3.65 **et** 1.2.5, sources comparées)
  n'écoute que `Props.params`, une **struct plate** `[ "node:port" = valeur … ]`
  (`get_props_param` émet `"<node>:<port>"`, `parse_params`/`set_control_value`
  résolvent `node:port`). Exemple qui marche (validé) :
  `{ "params" = [ "eq_L:50Hz gain (low shelving)" 4.1 … ] }`.
- Le spike Rust envoie donc la forme plate. La forme Python est conservée dans
  le code (`spa_config::build_props_python_style`) uniquement pour documenter
  l'écart. Hypothèse à vérifier sur la machine réelle (PipeWire ~1.2) : le
  parseur `module-filter-chain` 1.2.5 étant identique, la forme plate reste la
  bonne → le hot-reload Python actuel est probablement inefficace (à corriger).

### B. Cible du `set-param` Props = le **nœud stream `input.Holaf_FX_<uid>`**
- Le module filter-chain ne crée **pas** de nœud « principal » exporté : il crée
  deux streams (`capture`/`playback`) → seuls `input.Holaf_FX_<uid>` /
  `output.Holaf_FX_<uid>` apparaissent côté daemon. `_find_node_id_by_name(
  "Holaf_FX_uid")` (Python) ne peut donc pas trouver de nœud distant sur 0.3.65.
- Le `set-param Props` doit viser le nœud **stream** (`input.…`), pas un nœud
  principal inexistant. Vérifié : `set-param` sur `input.…` + `enum-params`
  renvoie la nouvelle struct (les 15 bandes × 2 canaux).
- Lecture de contrôle possible : `pw-cli enum-params <id> Props` (struct
  `params`) ou log module INFO
  `set_control_value(): control N ('<port>') from X to Y` (PIPEWIRE_DEBUG≥3).

### C. Architecture d'hébergement : contexte local, comme pw-cli (Tranché)
- holafd doit charger `libpipewire-module-filter-chain` **dans son propre
  `pw_context`** via `pw_context_load_module` (shim FFI `holaf-pw/src/sys.rs`,
  seule API manquante de pipewire-rs). Le graphe LADSPA vit dans holafd ; seuls
  les streams sont exportés.
- Sans `pw_context_set_object(Core)` le module ouvre **sa propre 2e connexion**
  au daemon (comportement pw-cli) → éprouvé de bout en bout, recommandé.
- Variante mono-client (`pw_context_set_object(context, Core, notre_core)`) :
  le module réutilise notre connexion mais **un `set-param` synchronisé sur notre
  propre nœud stream deadlock le mainloop monothread sur 0.3.65** (test
  `--reuse-core` : blocage après l'envoi). À ré-évaluer sur PipeWire 1.x /
  thread_loop en J2. **Défaut = mode pw-cli (connexion module séparée)**.

### D. Liaison WP automatique (comportement attendu, pas un bug)
Dans l'environnement headless, WirePlumber adopte le null-sink `Holaf_Strip_uid`
comme `default.audio.sink` et connecte automatiquement le stream playback de la
filter-chain dessus (d'où des liens visibles en `pw-link`). Le spike détecte
l'existence du lien (parcours des globals `Link`) et ne le duplique pas ; si
WirePlumber ne l'a pas encore créé (timing), c'est le code `link-factory`
(`create_object::<Link>`) qui crée le lien (cas observé : log « created link …
via link-factory (2) »). → holafd gardera `target.object` + un reconcilieur qui
ne duplique pas (pattern Python `_auto_link_ports` « exists »).

## Structure créée (rust/)
```
crates/holaf-pw/
  Cargo.toml                      # dep: pipewire 0.8, holaf-core, log, thiserror, env_logger
  src/lib.rs                      # doc crate
  src/sys.rs                      # shim FFI "holaf-sys" : pw_context_load_module (+ set/get_context_core, destroy/schedule_destroy) — unsafe documenté + tests
  src/spa_config.rs               # SPA-JSON pur (fidèle à _build_fx_graph/_create_fx_chain/_format_params) EQ-only + Props (plate & python-style) + tests
  src/bin/holaf-spike.rs          # binaire demo 6 étapes + cleanup + vérifications externes
docs/j1-spike-phase2.md           # ce fichier
```
holaf-core (J0) **inchangé** : `spa::build_graph/build_props` restent stubbés
(J2 les implémentera sur fixtures ; le spike construit le JSON EQ via holaf-pw
`spa_config` en réutilisant les vrais noms de ports de `holaf_core::fx`).

## Preuves (exécution headless, daemon relancé proprement)
Commande :
```bash
bash /opt/holaf-headless/start-pipewire-headless.sh
export XDG_RUNTIME_DIR=/tmp/pw-runtime PIPEWIRE_RUNTIME_DIR=/tmp/pw-runtime \
       DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/pw-runtime/bus PIPEWIRE_DEBUG=3
cd rust && cargo run -q -p holaf-pw --bin holaf-spike -- --uid demo1
```
- STEP 1 : `discovered 35 globals — factory adapter: true, link-factory: true`
- STEP 2 : null-sink `Holaf_Strip_demo1` node id 35, ports `[36, 38]`
- STEP 3 : `pw_context_load_module(libpipewire-module-filter-chain)` OK ;
  `input.Holaf_FX_demo1` id 41, `output.Holaf_FX_demo1` id 42, ports `[47, 48]`
- STEP 4 : liens créés via `link-factory` (log « created link … (2) ») quand
  WirePlumber ne les a pas déjà posés (détection anti-doublon ; cas WP ci-après)
- STEP 5 : `Node::set_param(Props)` → module log :
  `control 0 ('eq_L:50Hz gain (low shelving)') from 0.000000 to 4.100000`,
  `control 13 ('eq_L:10000Hz gain') from 0.000000 to -8.500000` (+ eq_R) ;
  read-back `pw-cli enum-params` : `String "eq_L:50Hz …" Float 4.1` etc.
- STEP 6 : `pw-dump` (nodes Holaf + metadata default sink) et `pw-link -l`
  (Holaf_Strip:playback_FL ← output.Holaf_FX:output_FL, etc.)
- cleanup : `cleanup complete — remaining 0 … CLEAN: no debris left in the daemon.`

Logs de référence conservés en `/tmp` : `spike_canonical.log` (parcours complet
: **2 liens créés via link-factory** + set-param EQ vérifié + CLEAN), `spike_run4.log`
(variante où WirePlumber a pré-créé les liens → détection + skip, toujours CLEAN),
`spike_reuse.log` (mono-client : liens link-factory créés, puis deadlock set-param — voir §C).

## Reste pour J2 (runtime complet)
1. Graph/props **multi-plugins** dans holaf-core (`_build_fx_graph`/`_build_fx_props`
   complets : dual-mono ET stéréo, port names STEREO_PLUGINS, BYPASS fusion) sur
   fixtures Python ; retirer les stubs J0.
2. Trait `GraphBackend` + `PipeWireBackend` (holaf-pw) autour de ce spike ;
   topologie OUTPUT strip réelle (chaîne capture=Audio/Sink, playback→device,
   metering) et INPUT strip (chaîne Audio/Source entre monitor source et routes).
3. Reconcilieur de graphe : création/destruction/reload strip, hot-reload sur
   `input.…` (cible corrigée), cas « pas de chain → routage direct ».
4. Props **plat** : décider du correctif Python (ou bridge) — divergence Python
   vs module 0.3.65/1.2.
5. Vérifier `--reuse-core`/mono-client sur PipeWire 1.x (machine réelle) avant de
   choisir l'architecture de connexion définitive.
6. Routing exclusif + mono, metadata default sink, volume/mute (Props channelVolumes),
   discovery/cleanup zombies, tests FakeBackend.

Estimation heures J2 : ~14–20 h (sans surprises) — cf. spec §7 ; le spike retire
déjà les 5 incertitudes API, J2 est surtout du volume de code + fixtures + tests.
