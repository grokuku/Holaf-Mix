# Suppression du Python — inventaire de ce qui a été retiré et de ce qui reste

> **Statut : FAIT (2026-11-15).**  Holaf-Mix est désormais un produit
> **100 % Rust**.  Il ne reste **aucun fichier Python** dans le dépôt
> (`find . -name '*.py' -not -path './rust/target/*'` → vide), aucune
> dépendance Python, aucun PyInstaller, aucun venv.

Demande utilisateur : « j'aimerais que vires tout ce qu'il reste de python ».
La bascule B1 (l'UI Rust est la fenêtre de l'application) avait déjà eu lieu et
l'application fonctionnait sans Python ; ce document liste le nettoyage.

---

## 1. Ce qui a été supprimé

### 1.1 Point d'entrée et lanceur

| Fichier | Lignes | Remplacé par |
|---|---:|---|
| `main.py` | 404 | plus de lanceur : le point d'entrée **est** `holaf-ui` (binaire Rust).  Le tarball contient aussi `holaf-mix`, un script **shell** de 30 lignes qui ne fait que `exec ./holaf-ui "$@"` |
| `src/launcher.py` | 323 | la résolution du `config.json` est passée dans `holaf_core::config::default_config_path()` (`rust/crates/holaf-core/src/config.rs`) + `--config` dans `holaf-ui` |

### 1.2 Interface Qt (PySide6)

| Fichier | Lignes | Remplacé par |
|---|---:|---|
| `src/ui/main_window.py` | 1046 | `holaf-ui/src/views/` + `app.rs` |
| `src/ui/widgets/strip_widget.py` | 641 | `holaf-ui/src/views/strip.rs` |
| `src/ui/diagnostics_dialog.py` | 512 | `holaf-ui/src/diagnostics.rs` + `dialogs.rs` |
| `src/ui/dialogs/effect_settings_dialog.py` | 191 | `holaf-ui/src/dialogs.rs` + `fx_ui.rs` |

### 1.3 Backend / IPC / modèles / config / diagnostics

| Fichier | Lignes | Remplacé par |
|---|---:|---|
| `src/backend/audio_engine.py` | 1467 | `holaf-pw::PipeWireBackend`, utilisé **en direct** par `holaf-ui` (in-process) |
| `src/backend/ipc_audio_engine.py` | 788 | supprimé : l'UI Rust n'utilise ni socket ni démon |
| `src/backend/rust_launcher.py` | 813 | supprimé : `holafd` reste un binaire du workspace, mais l'UI ne le lance pas |
| `src/backend/signal_shutdown.py` | 374 | inutile : `AppState::shutdown()` dans `on_exit` |
| `src/backend/metering.py` | 399 | `holaf-ui/src/capture.rs` + `meters.rs` (`pw-cat`, RMS/peak) |
| `src/backend/midi_engine.py` | 159 | `holaf-ui/src/midi.rs` (feature `midi`, `midir`) |
| `src/backend/pipewire_utils.py` | 169 | `holaf-pw/src/sink_inputs.rs` (`pactl -f json`) + discovery du graphe |
| `src/models/strip_model.py` | 344 | `holaf-core/src/model.rs` (+ `holaf-ui/src/config.rs` pour les clés hors modèle) |
| `src/config/settings.py` | 130 | `holaf-core/src/config.rs` + `holaf-ui/src/config.rs` |
| `src/diagnostics/collector.py` | 2235 | `holaf-ui/src/diagnostics.rs` |
| `src/diagnostics/logbuffer.py` | 139 | `holaf-ui/src/logbuf.rs` |
| `src/diagnostics/__init__.py` | 34 | — |
| `src/**/__init__.py` (5 fichiers vides) | 4 | — |

### 1.4 Tests Python et outils de développement

| Fichier | Lignes | Décision |
|---|---:|---|
| `tests/test_*.py` (9 fichiers) | 5 438 | **supprimés** : les 207 tests Python testaient une couche qui n'existe plus.  La suite Rust (≈ 600 tests, `cargo test --workspace`) est la seule suite |
| `tools/export_fixtures.py` | 384 | **supprimé** : il générait `rust/crates/holaf-core/tests/fixtures/spa_builders_golden.json`, qui est **committé** (`git ls-files` le confirme).  La fixture golden reste la référence ; l'outil n'était nécessaire qu'une fois |
| `tools/audio_bench.py` | 1012 | **supprimé** : banc de mesure A/B en Python (`numpy`) pour comparer les moteurs.  Le diagnostic vit maintenant dans l'UI Rust (*Debug window*) et `holafctl` ; le banc n'était plus utilisable sans l'ancien moteur Python |
| `tools/probe_node_state.py` | 193 | **supprimé** : son rôle (état des nœuds/links dans le temps) est couvert par la fenêtre *Debug window* (onglets Graph/Engine) et `pw-dump`/`pw-top` |
| `requirements.txt` | 38 | plus aucune dépendance Python |
| `packaging/holaf-mix.spec` | 411 | plus de PyInstaller |
| `packaging/hooks-runtime/hook-email.py` | 133 | hook de runtime PyInstaller, sans objet |

Total : **~16 800 lignes de Python** retirées.

### 1.5 Empaquetage / CI

* `build.sh` : plus de PyInstaller (onefile/onedir), plus de venv, plus de
  `pip`, plus de `check_deps` Python.  Il fait icônes + `cargo build` +
  tarball (`dist/holaf-mix-<version>-<arch>.tar.gz`).
* `.github/workflows/build.yml` : plus de `python`, `python-pip`, venv,
  `pip install PySide6/sounddevice/numpy/mido/rtmidi/pyinstaller`, plus de
  steps « PyInstaller onefile/onedir », « bundled email modules » ni de smoke
  test du bootloader.  Un seul artefact : le tarball Rust.
* `packaging/PKGBUILD` : plus de `depends` Python (`python`, `pyside6`,
  `python-numpy`, `python-mido`, `python-rtmidi`), plus de `pyinstaller` dans
  `build()`.  Il installe `holaf-ui`, `holafd`, `holafctl`, le lanceur shell
  `holaf-mix`, le `.desktop` et les icônes.
* `packaging/holaf-mix.desktop` : `Exec=__EXEC_PATH__/holaf-ui`, plus de repli
  `HOLAF_UI=python`.

---

## 2. Ce qui a été conservé, et pourquoi

| Élément | Pourquoi il reste |
|---|---|
| `rust/crates/holaf-core/tests/fixtures/spa_builders_golden.json` | Fixture **golden** committée : elle fige les chaînes SPA-JSON exactes que produisait le moteur historique.  `cargo test` l'utilise (`tests/spa_builders_golden.rs`).  L'outil qui l'a générée (`tools/export_fixtures.py`) est supprimé ; la donnée reste. |
| `rust/crates/holafd`, `rust/crates/holafctl` | Le démon audio et sa CLI restent utiles (automatisation, diagnostic, `holafctl status`/`meter-sources`).  L'UI n'en dépend pas : elle pilote PipeWire in-process. |
| `rust/crates/holaf-ipc` | Protocole NDJSON du démon : toujours utilisé par `holafd`/`holafctl`. |
| `scripts/build_rust.sh` | Compile et publie les trois binaires ; appelé par `build.sh` et le PKGBUILD. |
| `scripts/check-wayland-guard.sh` | Garde-fou CI/local : pas de backend X11 dans le binaire livré. |
| `scripts/start-pipewire-headless.sh` | Harnais des tests *live* Rust (`HOLAF_PW_LIVE=1`) : sans lui, aucun test audio live reproductible. |
| `packaging/holaf-mix.sh` | Script **shell** (pas Python) qui préserve la commande `holaf-mix` ; le tarball et le PKGBUILD l'installent. |
| `config.json` (racine) | Configuration de référence des tests Rust (round-trip, fixtures) et du développement. |
| `docs/j1-*.md`, `docs/j2-*.md`, `docs/ui-inventory.md`, `docs/rust-rewrite-*.md`, `docs/fx-host.md`, `docs/dependency-audit.md` | **Documents historiques** de la migration : ils mentionnent le Python parce qu'ils racontent comment on en est sorti.  Ils ne décrivent plus le produit ; `README.md`/`BUILD.md` le font. |

Les commentaires Rust qui citent `src/backend/*.py` (`//! the Rust twin of …`)
sont conservés : ils documentent **l'origine** de chaque portage (parité de
comportement), ce qui reste utile pour comprendre une décision.  Aucun code ne
lit ces fichiers.

---

## 3. Nouveau point d'entrée et résolution du `config.json`

Le tarball contient `holaf-ui`, `holafd`, `holafctl`, `holaf-mix` (script
shell), `icons/`, `holaf-mix.desktop`, `USAGE.txt`, `README.md`, `BUILD.md`,
`ROADMAP.md` et `docs/`.  L'utilisateur lance **`./holaf-ui`** (ou
`./holaf-mix`).

La résolution du `config.json` est dans
`holaf_core::config::default_config_path()` et documentée dans `BUILD.md` §2 :

1. `--config FICHIER` ;
2. `$HOLAF_CONFIG` ;
3. à côté de l'exécutable, s'il existe ;
4. `./config.json` du répertoire courant, s'il existe ;
5. `$XDG_CONFIG_HOME/holaf-mix/config.json` (défaut `~/.config/holaf-mix/`),
   créé au premier lancement (l'écriture atomique crée le dossier manquant).

Différence assumée avec l'ancien lanceur Python : celui-ci épinglait
`--config <dossier de l'exécutable>/config.json` dans tous les cas.  Le
résolveur Rust retombe maintenant sur XDG pour une installation système (où
`/usr/bin` n'est pas inscriptible), ce qui évite un `config.json` dans
`/usr/bin` ou dans `$HOME`.  Pour un tarball décompressé, le fichier reste dans
le dossier décompressé (règle 3), donc rien ne change pour l'utilisateur.

---

## 4. Preuves

* `cargo test --workspace` : la suite Rust complète passe sans aucun fichier
  Python (les tests Rust n'invoquent jamais `python3`, vérifié par
  `grep -rn "python3\|python " rust --include='*.rs'` : seulement des
  commentaires/docstrings historiques).
* `./build.sh --tarball` produit `dist/holaf-mix-<version>-<arch>.tar.gz` ;
  extrait dans `/tmp`, il contient les quatre exécutables, `holaf-ui --help`
  répond, `holaf-mix --help` relaie, `ldd` ne signale aucune bibliothèque
  manquante, et un lancement sans écran affiche le message eframe
  « no display server » en ayant résolu la config vers
  `~/.config/holaf-mix/config.json`.
* La CI (`build.yml`) fait ces vérifications à chaque push, dont un contrôle
  explicite : **aucun `.py`/`.pyc`/`__pycache__` dans le tarball**.

---

## 5. Ce qui a changé pour l'utilisateur

| Avant | Après |
|---|---|
| `./holaf-mix` (binaire PyInstaller, ~100 Mo) | `./holaf-ui` (binaire Rust, ~15 Mo) ou `./holaf-mix` (script shell) |
| `HOLAF_UI=python` pour l'ancienne UI | n'existe plus |
| config épinglée à côté de l'exécutable | ordre à 5 niveaux, XDG en dernier recours (`HOLAF_CONFIG` pour forcer) |
| artefact CI = bundle Python + binaires Rust | artefact CI = tarball des binaires Rust |
| `pip install -r requirements.txt` | rien : aucune dépendance Python |
