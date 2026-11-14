# Migration vers un produit 100 % Rust — inventaire et plan de suppression

> **Statut : PRÉPARATION. Rien n'est supprimé.**
> Ce document décrit *ce qui* sera supprimé, *dans quel ordre*, *ce qui casse*
> si on le fait maintenant, et *à quelle condition* on peut le faire.  Il est
> écrit pour être exécuté plus tard, bloc par bloc, sans relecture du code.

La bascule est faite (bloc **B1**) : `main.py` est un **lanceur** qui démarre le
binaire Rust `holaf-ui`, et l'UI PySide6 n'est plus qu'une échappatoire
(`HOLAF_UI=python`).  La suppression du Python est donc une opération de
*nettoyage*, pas de portage : tout ce qui disparaît a déjà un équivalent Rust en
production.

---

## 0. Prérequis avant de supprimer quoi que ce soit

| # | Prérequis | Pourquoi | Comment le vérifier |
|---|---|---|---|
| 1 | **B1 validé par l'utilisateur sur sa machine** (pas seulement en CI) | une régression de layout, de routage ou de MIDI ne se voit pas dans un conteneur sans écran ni carte son | une session complète : 7 strips, faders, FX, routage, apps, MIDI Mix, diagnostic, config conservée |
| 2 | **Le tray est tranché** (B2) | c'était la seule fonction Qt encore sans équivalent ; supprimer `src/ui/**` supprime aussi la seule implémentation de tray existante | **fait (2026-11-14)** : tray Rust (GTK3 + libappindicator, icône générée en RGBA donc **sans dépendance fichier**), politique (menu, « fermer = cacher seulement si un tray existe ») toujours compilée et testée ; **activé par défaut au build** (`scripts/build_rust.sh` ; `HOLAF_NO_TRAY=1` pour l'enlever, repli automatique sans la feature si GTK3 manque), la pile GTK3/GLib restant celle du système (retirée du bundle PyInstaller).  Le repli (pas de display, pas d'hôte appindicator) est testé : `available() == false` ⇒ fermer quitte | `cargo test -p holaf-ui --features tray` (dont `install_without_a_display_degrades_to_no_tray_instead_of_panicking`) + `cargo build --features tray` en CI |
| 3 | **Le bundle packagé démarre l'UI Rust** | le lanceur se rabat sur Qt quand `holaf-ui` manque, donc un bundle cassé *fonctionne* — en Qt | `dist/holaf-mix` sans `HOLAF_UI=python` doit afficher la fenêtre Rust (`smoke test` CI + un lancement réel) |
| 4 | **`config.json` partagé vérifié** | le lanceur passe `--config` avec le fichier de l'UI Qt ; si ce chemin change, l'utilisateur perd ses réglages | lancer l'UI Rust puis l'UI Qt : même fichier, mêmes valeurs |
| 5 | **Le MIDI réel validé** | `midir` (Rust) remplace `mido`/`rtmidi` ; en conteneur il n'y a pas de séquenceur ALSA | AKAI MIDI Mix : learn volume/mute/mono, LEDs, reconnexion |
| 6 | **Décision sur `tools/`** | `tools/audio_bench.py` et `tools/export_fixtures.py` sont des outils de *développement* (bench audio, génération de fixtures pour les tests Rust) | garder (hors produit) ou réécrire en Rust ; les supprimer casserait les procédures de mesure |

Tant que 1, 2 et 3 ne sont pas cochés, **ne rien supprimer** : le coût de garder
le Python est de ~10 k lignes ; le coût d'une régression non détectée est la
confiance de l'utilisateur.

---

## 1. Inventaire de ce qui sera supprimé

### 1.1 Moteur et IPC Python — bloc **B10** (~4 400 lignes)

| Fichier | Lignes | Remplacé par | Remarques |
|---|---:|---|---|
| `src/backend/audio_engine.py` | 1 467 | `holaf-pw::PipeWireBackend` (utilisé **en direct** par `holaf-ui`) | moteur historique (`pw-cli`/`pactl`), déjà hors du chemin par défaut |
| `src/backend/ipc_audio_engine.py` | 788 | idem (l'UI Rust n'a pas besoin de socket) | client NDJSON du démon ; l'UI Rust ne lance jamais `holafd` |
| `src/backend/rust_launcher.py` | 813 | supprimé avec le démon côté UI ; `holafd` reste un binaire du workspace | **seul consommateur** : `main.py` (chemin Qt) et ses tests |
| `src/backend/signal_shutdown.py` | 374 | inutile : l'UI Rust fait `AppState::shutdown()` dans `on_exit` | il n'y a plus de démon enfant à récolter |
| `src/backend/metering.py` | 399 | `holaf-ui/src/capture.rs` + `meters.rs` (capture `pw-cat`, RMS/peak) | `sounddevice`/PortAudio disparaît des dépendances |
| `src/backend/midi_engine.py` | 159 | `holaf-ui/src/midi.rs` + feature `midi` (`midir`) | learn, LED, reconnexion déjà portés |
| `src/backend/pipewire_utils.py` | 169 | `holaf-pw/src/sink_inputs.rs` (`app_list`/`app_move`) + `device_list` | **fait** : `app_list` lit `pactl -f json list sink-inputs` (+ `list sinks` pour le `node.name`), exclut les flux du démon (pid, enfants = `pw-cat` de métrage, nœuds `Holaf_*`, `application.name == "Holaf-Mix"`) ; `app_move` déplace via `pactl move-sink-input` **puis relit** (read-back). Preuve live : `holaf-pw/tests/app_routing_live.rs` |

### 1.2 Interface et modèles Python — bloc **B11** (~2 300 lignes)

| Fichier | Lignes | Remplacé par |
|---|---:|---|
| `src/ui/main_window.py` | 1 046 | `holaf-ui/src/views/main_window.rs` + `app.rs` |
| `src/ui/widgets/strip_widget.py` | 641 | `holaf-ui/src/views/strip.rs` |
| `src/ui/diagnostics_dialog.py` | 512 | `holaf-ui/src/diagnostics.rs` + `dialogs.rs` (onglets, dont « Journal » depuis B7) |
| `src/ui/dialogs/effect_settings_dialog.py` | 191 | `holaf-ui/src/dialogs.rs` + `fx_ui.rs` |
| `src/models/strip_model.py` | 344 | `holaf-core/src/model.rs` (+ `holaf-ui/src/config.rs` pour les clés hors modèle) |
| `src/config/settings.py` | 130 | `holaf-core/src/config.rs` + `holaf-ui/src/config.rs` |
| `src/diagnostics/*` (`collector.py` 2 235, `logbuffer.py` 139, `__init__.py` 34) | 2 408 | `holaf-ui/src/diagnostics.rs` (rapport pur) + `logbuf.rs` (ring buffer) |

### 1.3 Lanceur et empaquetage — bloc **B12** (ce qui restera de `main.py`)

| Fichier | Lignes | Action |
|---|---:|---|
| `main.py` | 404 | devient inutile quand l'UI Rust est le seul point d'entrée → remplacé par `packaging/holaf-mix.desktop` (`Exec=holaf-ui`) et par l'installation de `/usr/bin/holaf-ui`.  Décision à prendre : garder un lanceur minimal (résolution `$PATH` + `--config`) ou pas de lanceur du tout |
| `src/launcher.py` | 323 | **reste** tant que `main.py` existe ; à fusionner dans `main.rs` (résolution du `config.json` + `exec`) si on supprime le lanceur Python |
| `packaging/holaf-mix.spec` + PyInstaller | — | un bundle PyInstaller n'a plus de sens sans Python : le produit devient `tar.gz` de binaires + paquet distro (`PKGBUILD` déjà prêt) |
| `requirements.txt` | — | plus de PySide6 / sounddevice / mido / rtmidi / numpy (⚠ `tools/` en dépend peut-être : vérifier) |

### 1.4 Ce qui **reste** (côté Rust, hors périmètre de suppression)

`rust/crates/holaf-ui` (UI), `holaf-pw` (backend PipeWire), `holaf-core`
(modèle + config), `holafd`/`holafctl` (démon + CLI : **utiles même sans UI Qt**,
ce sont les outils de diagnostic/automatisation), les tests Rust (dont
`engine_live.rs`, `midi_live.rs`), `scripts/build_rust.sh`, `build.sh`
(simplifiable : plus de PyInstaller).

---

## 2. Ordre d'exécution recommandé

Chaque étape est indépendante et laisse le dépôt vert.

| Ordre | Bloc | Contenu | Critère de sortie |
|---:|---|---|---|
| 1 | **B10a** | supprimer `audio_engine.py`, `ipc_audio_engine.py`, `metering.py`, `midi_engine.py`, `pipewire_utils.py` **et** le chemin Qt qui les utilise | `python3 -m unittest discover -s tests` vert après adaptation ; `HOLAF_UI=python` n'existe plus → `main.py` ne fait plus que lancer `holaf-ui` |
| 2 | **B10b** | supprimer `rust_launcher.py`, `signal_shutdown.py` | plus aucun spawn de démon côté Python ; `holafd` reste testé par `cargo test -p holafd` |
| 3 | **B11** | supprimer `src/ui/**`, `src/models/**`, `src/config/**`, `src/diagnostics/**` | `tests/test_app_menu.py`, `test_diagnostics.py` supprimés ; `test_imports_complete.py` réduit/supprimé |
| 4 | **B12** | `main.py` + `src/launcher.py` : soit fusionnés dans `main.rs`, soit réduits au strict lanceur ; `packaging/holaf-mix.spec` et PyInstaller retirés | `build.sh` ne produit plus que des binaires Rust ; CI sans étape Python |
| 5 | **B13** | nettoyage final : `docs/` (statuts obsolètes), `requirements.txt`, CI | `grep -r "PySide6" .` ne renvoie plus rien hors historique |

> **Ne pas** fusionner les étapes : après B10a il reste ~3 k lignes de Qt qui
> servent encore de référence de comparaison visuelle ; après B11 il ne reste
> plus rien pour comparer.

---

## 3. Ce qui casse si on supprime maintenant

### 3.1 Tests Python (4 864 lignes)

| Fichier de test | Lignes | Devient | Action |
|---|---:|---|---|
| `tests/test_app_menu.py` | 566 | teste le menu contextuel **Qt** (`QMenu`), supprimé avec `src/ui/**` | **supprimer** (l'équivalent Rust est `holaf-ui/src/menus.rs` + ses tests) |
| `tests/test_diagnostics.py` | 1 030 | teste `src/diagnostics/collector.py` | **supprimer** (l'équivalent Rust est `holaf-ui/src/diagnostics.rs`) — ⚠ vérifier d'abord que chaque constat (`strip_bus_missing`, `mainloop_pumps_*`, `multiple_daemons`…) existe côté Rust |
| `tests/test_engine_selection.py` | 494 | teste `main.create_audio_engine()` (choix IpcAudioEngine / AudioEngine) | **supprimer** avec le chemin Qt ; garder la partie « launcher » (déjà couverte par `test_launcher.py`) |
| `tests/test_ipc_audio_engine.py` | 866 | teste le client NDJSON Python | **supprimer** (le protocole est testé côté Rust : `holaf-ipc`, `holafd`) |
| `tests/test_daemon_lifecycle.py` | 870 | teste `rust_launcher` (PDEATHSIG, socket, arrêt) | **supprimer** : plus de démon enfant côté UI |
| `tests/test_metering.py` | 226 | teste `src/backend/metering.py` (`sounddevice`) | **supprimer** (métrage Rust : `capture.rs`/`meters.rs`) |
| `tests/test_imports_complete.py` | 498 | garde-fou du **bundle PyInstaller** (excludes/hiddenimports) | **supprimer** avec le bundle |
| `tests/test_launcher.py` | 314 | teste la résolution de `holaf-ui` | **reste** tant que `main.py` est un lanceur ; sinon porter les cas dans `holaf-ui` (`--config`/`--midi` sont déjà testés dans `main.rs`) |

Conséquence : `python3 -m unittest discover -s tests` passerait de ~200 tests à
~19 (ceux de `test_launcher.py`).  **La couverture ne s'effondre pas** : elle
est dans `cargo test --workspace` (569 tests), mais il faut le dire dans le
rapport de migration, sinon la baisse du nombre de tests ressemble à une
régression.

### 3.2 Ce qui casse *fonctionnellement* (le vrai risque)

| Rupture | Déclencheur | Effet | Parade |
|---|---|---|---|
| **Tray** | ~~suppression de `src/ui/main_window.py`~~ | **résolu** (2026-11-14) : l'icône Rust existe derrière la feature `tray` (`holaf-ui/src/tray.rs`, GTK3 + libappindicator, icône **générée** en code) ; menu `Show/Hide`/`Restart audio engine`/`Quit`, et « fermer = cacher » uniquement quand un tray est réellement là.  Sans la feature (défaut) ou sans hôte appindicator, fermer quitte | — (reste à activer la feature dans le packaging si on veut l'icône par défaut) |
| **Routage applicatif** | ~~`holaf-pw::app_list`/`app_move` encore en **stub**~~ | **résolu** (2026-11-14) : `app_list` liste les flux externes, `app_move` les déplace sur le bus du strip avec read-back ; l'UI Rust (`apps.rs` plan + `enforce_applications` 5 s) fonctionne de bout en bout | — |
| **Diagnostic : collecte** | `src/diagnostics/collector.py` est la seule collecte « live » testée (`pw-dump`, `pactl`, `holafctl`) | onglets vides/partiels côté Rust si un constat n'a pas été porté | comparer les deux rapports sur la même machine avant suppression |
| **Résolution du `config.json`** | `holaf_core::config::default_config_path()` part du **répertoire courant**, l'UI Qt de son propre chemin | lancé depuis un `.desktop` (cwd = `$HOME`), le Rust créerait `$HOME/config.json` et paraîtrait « avoir perdu » les réglages | déjà réglé par le lanceur (`--config`) : **ne pas** supprimer `src/launcher.py` avant d'avoir porté cette règle dans `main.rs` |
| **Raccourcis / comportements Qt** | suppression de `src/ui/**` | plus de référence pour comparer (thème, géométrie) | garder `docs/ui-inventory.md` + les captures (`docs/ui-rust-snapshot*.png`) et `holaf-ui-snapshot` |
| **Outils de mesure** | suppression de `sounddevice`/`numpy` de `requirements.txt` | `tools/audio_bench.py` (banc audio) ne tourne plus | garder un `requirements-dev.txt` pour `tools/` |
| **CI** | suppression des étapes PyInstaller | l'artefact `holaf-mix-*.tar.gz` disparaît ; les utilisateurs qui le téléchargeaient doivent recevoir un `tar.gz` de binaires Rust | produire `holaf-ui-{version}-{arch}.tar.gz` (binaires + `.desktop` + icônes) avant de retirer l'ancien artefact |

### 3.3 Dépendances à retirer (une fois B10/B11 faits)

```
PySide6, sounddevice, numpy, mido, python-rtmidi, pyinstaller
```
⚠ `numpy` est aussi utilisé par `tools/` et par d'anciens scripts de bench :
vérifier avant de le sortir de `requirements.txt`.

---

## 4. Ce qui a été fait dans cette session (pour mémoire)

* **B1 — bascule + packaging** : `main.py` est un lanceur (`src/launcher.py`
  résout `holaf-ui`, transmet les arguments, épingle `--config`, se rabat sur Qt
  avec un message clair) ; `holaf-ui` est construit par
  `scripts/build_rust.sh`, embarqué par `packaging/holaf-mix.spec` (onefile **et**
  onedir), inclus dans le tarball, installé par `packaging/PKGBUILD`, lancé par
  `packaging/holaf-mix.desktop` et construit par la CI ;
* **B3 — combo device inline** : la liste des devices s'ouvre en pop-up sous le
  bouton (non modal, fermeture par clic extérieur / Échap / sélection) ;
* **B7 — journal dans le diagnostic** : ring buffer de 500 lignes
  (`holaf-ui/src/logbuf.rs`) alimenté par un `log::Log` qui relaie vers
  `env_logger`, et 5ᵉ onglet « Journal » (monospace, auto-scroll) ;
* **B2 — tray** : `holaf-ui/src/tray.rs` porte désormais la vraie icône
  (`tray-icon 0.19` + `gtk 0.18`, donc GTK3 + libappindicator au build).  Elle
  est **activée par défaut au build** (`scripts/build_rust.sh` passe
  `--features tray` ; `HOLAF_NO_TRAY=1` pour l'enlever, et le script retombe
  sans la feature si GTK3 manque), donc le PKGBUILD, le tarball et la CI
  livrent l'icône ; la feature reste hors des `default` de la crate pour que
  `cargo test --workspace` reste hermitique.  Le menu est celui de Qt
  (`Show/Hide`, `Restart audio engine`, `Quit`) ; l'icône est **générée en RGBA
  dans le code** (`tray::icon_rgba(size)`), jamais lue depuis `dist/icons/` qui
  est gitignoré — sinon un checkout CI propre n'aurait pas le PNG.  `install()`
  ne panique jamais : sans display ou sans hôte appindicator il journalise un
  avertissement et `available()` reste `false`, donc fermer la fenêtre
  **quitte** au lieu de cacher une fenêtre inatteignable.  La pile GTK3/GLib
  n'est **pas** embarquée par PyInstaller (le spec la retire de `a.binaries`) :
  l'app utilise celle du système, comme pour libpipewire ;
* **Routage applicatif** : `holaf-pw::app_list`/`app_move` ne sont plus des
  stubs (déplacement réel + read-back, prouvé par
  `holaf-pw/tests/app_routing_live.rs`) ;
* **B0 — nettoyage** : suppression de 7 scripts de manipulation jetables
  (`check_silent_load.py`, `debug_fx.py`, `debug_routing.py`, `debug_verbose.py`,
  `test_keepalive.py`, `test_interactive_keepalive.py`, `run_holaf_mix.sh`),
  aucun référencé par le build, la CI ou la doc versionnée.
