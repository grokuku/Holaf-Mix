# 🗺️ Holaf-Mix — Roadmap & Analyse

> **Légende des statuts :**
> - 🔴 **Non validé** — en attente de décision
> - 🟢 **Validé** — à implémenter
> - ⚪ **Rejeté** — ne sera pas fait
> - ✅ **Fait** — implémenté

---

## 🔧 Améliorations de Code

| # | Description | Statut |
|---|-------------|--------|
| 1 | **`main.py` — Pas de gestion d'erreur au démarrage** : aucun `try/except` autour de l'init des engines ou du chargement de config. Un crash silencieux est possible. | 🔴 Non validé |
| 2 | **`audio_engine.py` — `logging.basicConfig()` au niveau module** : modifie la config globale de logging. Devrait être fait une seule fois dans `main.py`. | 🔴 Non validé |
| 3 | **`audio_engine.py` — `start_engine()` trop monolithique** : ~150 lignes qui font tout (cleanup, création nodes, FX, metering, routing, default sink). Mérite d'être découpé en plusieurs méthodes. | 🔴 Non validé |
| 4 | **`audio_engine.py` — `update_fx_params()` est un stub vide** : la méthode existe, documente l'intention, mais ne fait rien. Changer un paramètre d'effet nécessite un redémarrage complet de l'engine. | 🔴 Non validé |
| 5 | **`audio_engine.py` — `_create_fx_chain()` définit `build_graph()` comme nested function** : redéfinie à chaque appel, rend le code dur à lire et tester. | 🔴 Non validé |
| 6 | **`audio_engine.py` — `_start_fx_host()` pas de monitoring** : si le process `pw-cli` meurt en cours de session, rien ne le redémarre automatiquement. | 🔴 Non validé |
| 7 | **`metering.py` — thread safety via `os.environ` + lock** : hack fragile documenté comme « CRITICAL ». Une évolution de `sounddevice` pourrait casser ce comportement. | 🔴 Non validé |
| 8 | **`pipewire_utils.py` — `_run_command()` retourne `None` sur erreur** : les appelants ne gèrent pas ce cas, les erreurs sont silencieusement ignorées. | 🔴 Non validé |
| 9 | **`midi_engine.py` — `_listen_loop()` utilise un polling actif** : `threading.Event().wait(0.005)` consomme du CPU inutilement. `mido` supporte le blocking `receive()`. | 🔴 Non validé |
| 10 | **`strip_widget.py` — `_check_and_send_volume()` tourne en continu** : le QTimer à 50ms est actif même quand le fader ne bouge pas. | 🔴 Non validé |
| 11 | **`settings.py` — `save_config()` lit puis écrase le fichier** : pas de lock, une double instance causerait une perte de données (mineur pour un usage perso). | 🔴 Non validé |
| 12 | **Zéro test unitaire** : les scripts `debug_*.py` et `test_*.py` sont des outils de diagnostic ponctuels, pas une suite de tests. | 🔴 Non validé |
| 13 | **Pas d'abstraction sur PipeWire** : `audio_engine.py` appelle `subprocess.run()` directement partout, rendant le code impossible à tester sans vrai PipeWire. | 🔴 Non validé |
| 14 | **Magic numbers éparpillés** : 20 retries, 50 cycles de compteur, 5 secondes de polling, 0.3s de sleep… Aucune constante nommée. | 🔴 Non validé |
| 15 | **Pas de `requirements.txt` versionné** : les dépendances (`mido`, `PySide6`, etc.) n'ont pas de versions pinées, rendant l'installation non reproductible. | 🔴 Non validé |

---

## 🐛 Bugs à corriger

| # | Description | Statut |
|---|-------------|--------|
| 1 | **`main_window.py` — `effect_params_changed` n'est connecté à rien** : le signal est défini dans `StripWidget` et `EffectSettingsDialog`, mais `main_window` ne l'écoute pas. Changer un paramètre d'effet via le dialogue ne déclenche rien du tout. | ✅ Fait |
| 2 | **Clics rapides sur les boutons FX → chaos** : chaque toggle d'effet lance un `shutdown()` + `start_engine()` complet. Si l'utilisateur clique vite sur 2-3 effets, plusieurs restarts sont empilés dans le ThreadPool. | ✅ Fait |
| 3 | **`audio_engine.py` — `_clean_zombie_nodes()` utilise `communicate()` avec `quit`** : `pw-cli` en mode batch peut ne pas traiter toutes les commandes avant le `quit`, laissant des nœuds zombies. | ✅ Fait |
| 4 | **`pipewire_utils.py` — `get_sink_inputs()` filtre « python »** : toute application Python légitime (ex: un jeu en Python) serait filtrée à tort. | 🔴 Non validé |
| 5 | **`_create_fx_chain()` — double tentative avec/sans `controls`** : si la 1ʳᵉ tentative crée partiellement des nœuds, la 2ᵉ peut entrer en conflit (pas de cleanup entre les deux). | ✅ Fait |
| 6 | **`on_strip_device_changed` → shutdown + start en parallèle** : `_refresh_device_ui_state()` est appelé immédiatement, mais le restart engine est asynchrone. Risque d'état incohérent. | ✅ Fait |
| 7 | **`_unlink_nodes` utilise `check=False`** : les erreurs de `pw-link -d` sont silencieusement ignorées, masquant des vrais problèmes de déconnexion. | 🔴 Non validé |
| 8 | **`is_source_registry` mal peuplé pour les strips physiques** : dans `start_engine()`, si `node_id` est trouvé mais que le strip est une source physique, l'entrée dans `node_registry` n'est pas toujours faite. | 🔴 Non validé |
| 9 | **`strip_widget.py` — `_on_device_changed` ne met pas à jour `strip.mode`** : c'est fait dans `main_window.on_strip_device_changed`, mais si le widget est utilisé ailleurs, l'état serait désynchronisé. | 🔴 Non validé |
| 10 | **`audio_engine.py` — `_create_virtual_node` attend 0.3s sans vérification** : pas de confirmation que le nœud est vraiment créé avant de continuer. En cas de latence PipeWire, `_find_node_id_by_name` peut échouer. | 🔴 Non validé |
| 11 | **`audio_engine.py` — Input physique sans FX = aucun audio routé** : `_link_physical_source_to_strip()` était une méthode vide (`pass`). Pour un input physique (ex: micro USB) sans aucun effet activé, le signal n'était jamais routé vers les outputs, car `_create_link` utilisait `raw_src_name` (= `alsa_input.xxx`, qui n'a pas de ports output écoutables) au lieu du monitor (`alsa_input.xxx.monitor`). L'utilisateur n'entendait rien tant qu'il n'activait pas au moins un effet. **Fix** : `_link_physical_source_to_strip` enregistre maintenant le `monitor_name` dans `fx_source_names[uid]`, qui sert de source effective dans `_create_link` (mécanisme identique à celui d'un FX chain). | ✅ Fait |
| 12 | **`audio_engine.py` + `pipewire_utils.py` — `pw-dump` est parsé sans cache** : `_find_node_id_by_name`, `_find_physical_node`, `get_node_info` appellent tous `pipewire_utils.get_audio_nodes()` qui exécute `subprocess.run(['pw-dump'])` + parse JSON à chaque lookup. Au démarrage, ~20+ subprocess + parses JSON pour 10 strips. Pas de cache d'invalidation. | 🔴 Non validé |
| 13 | **`metering.py` — `retry_pending()` ne réessaie qu'**un seul** item par cycle** : `items_to_retry[0]` puis `start_monitoring`. Si 5 strips échouent en même temps, ils sont retentés un par un avec un délai de ~2.5s entre chaque (50 cycles × 50ms = 2.5s). Latence perceptible quand on lance l'app avec plusieurs devices pas encore initialisés. | 🔴 Non validé |
| 14 | **`audio_engine.py` — `_meter_retry_counter` réinitialise l'index `pending_retries` du mauvais endroit** : le compteur de 50 cycles est dans `AudioEngine`, pas dans `MeteringEngine`. Si le metering est désactivé temporairement (par ex. après un restart engine), la resynchronisation se fait à partir de zéro. Pas critique mais imprévisible. | 🔴 Non validé |
| 15 | **`audio_engine.py` — Code mort : `update_fx_params()` et `set_mono_registry()`** : deux méthodes définies mais jamais appelées. `update_fx_params()` était destinée au hot-reload FX, mais `main_window.on_strip_effect_params_changed` utilise `_schedule_engine_restart()` à la place. `set_mono_registry()` est un doublon de la logique dans `set_mono()`. Candidates à suppression pour clarifier le code. | ⚪ Rejeté — laissées en place comme stubs/sentinelles. La suppression sera faite dans un refactor dédié (voir item Code 3). |

---

## 👤 Améliorations UX

| # | Description | Statut |
|---|-------------|--------|
| 1 | **Aucun feedback lors d'un restart engine** : le changement d'effet provoque un freeze visuel de 1-2 secondes sans indication (spinner, message, etc.). | 🔴 Non validé |
| 2 | **Changement de paramètres FX = restart complet** : pas de hot-reload. Tourner un knob d'EQ force la reconstruction de toute la chaîne audio (coupure de son). | 🔴 Non validé |
| 3 | **Pas de vu-mètre de gain reduction** : aucun retour visuel sur ce que font le gate/compresseur (mentionné dans le TODO du projet). | 🔴 Non validé |
| 4 | **Suppression de strip sans undo** : une fois supprimée, la strip est définitivement perdue (la confirmation existe, mais pas de undo). | 🔴 Non validé |
| 5 | **MIDI Learn sans timeout ni annulation visible** : le bouton passe en orange « LEARNING... » mais aucune indication de comment annuler ou combien de temps ça dure. | 🔴 Non validé |
| 6 | **Fenêtre non redimensionnable** : `setFixedWidth()` empêche tout resize. Avec beaucoup de strips, ça déborde de l'écran. | ⚪ Rejeté — le resize horizontal automatique est volontaire pour garder l'interface propre. Le resize vertical fonctionne. |
| 7 | **Labels de routing tronqués à 4 caractères** : `out_strip.label[:4].upper()` rend illisible des noms comme « Casque » vs « Chat ». | 🔴 Non validé |
| 8 | **Pas de valeur numérique sur le fader** : pas d'affichage en dB ou %, difficile de reproduire un réglage précis. | 🔴 Non validé |
| 9 | **Liste des devices jamais rafraîchie en live** : si on branche/débranche une interface audio, il faut redémarrer l'application. | 🔴 Non validé |
| 10 | **« Always on Top » forcé, pas désactivable** : `WindowStaysOnTopHint` est hardcodé, pas d'option dans l'UI. | 🔴 Non validé |
| 11 | **Pas de presets / configurations multiples** : pas de système pour sauvegarder/charger des profils (ex: « Gaming », « Streaming », « Musique »). | 🔴 Non validé |
| 12 | **Pas de Solo / PFL (Pre-Fader Listen)** : fonction essentielle en mixage audio, totalement absente. | 🔴 Non validé |
| 13 | **Pas de raccourcis clavier** : tout est à la souris ; pas de `Ctrl+S`, `Espace` pour mute, etc. | 🔴 Non validé |
| 14 | **Double-clic pour renommer pas découvrable** : le curseur ne change pas au survol du label, aucun indice visuel que c'est interactif. | 🔴 Non validé |
| 15 | **Bouton « SELECT APPS » ne montre pas le nombre d'apps assignées** : l'utilisateur doit ouvrir le dialogue pour savoir ce qui est déjà routé. | 🔴 Non validé |
| 16 | **Pas de peak hold sur les VU-mètres** : les barres bougent sans retenir le pic, rendant la lecture difficile. | 🔴 Non validé |
| 17 | **Thème dark uniquement, couleurs hardcodées** : pas de thème clair, pas de customisation des couleurs. | 🔴 Non validé |
| 18 | **Pas d'ajustement fin du volume au clavier** : impossible d'utiliser les flèches ou la molette pour ajuster le volume précisément. | 🔴 Non validé |

---

## 🔬 R&D / Améliorations futures

| # | Description | Statut |
|---|-------------|--------|
| 1 | **Hot-reload des paramètres LADSPA sans restart** : étudier s'il est possible d'injecter à chaud de nouveaux paramètres dans un nœud `filter-chain` via `pw-cli set-param` ou l'API PipeWire native (`pw_stream`, `pw_filter`). Les sous-nœuds LADSPA exposent leurs paramètres comme des ports `control` — vérifier si `filter-chain` les répercute. Si cette brique fonctionne, cela permettrait de : supprimer tous les restarts engine (toggle d'effet ET changement de params), avec un vrai live tuning (sliders réactifs). | 🔴 Non validé |

---

## 📝 Notes

- Tous les points ci-dessus sont **en attente de validation**. Tant qu'un point n'est pas passé en 🟢 **Validé**, il ne doit **pas** être implémenté.
- Une fois un point implémenté, le passer en ✅ **Fait**.
- Si un point est jugé non pertinent, le passer en ⚪ **Rejeté** avec une courte justification.

