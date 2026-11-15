# Holaf-Mix

**A Voicemeeter-like Audio Mixer for Linux (PipeWire) + MIDI Support.**

> ⚠️ **DISCLAIMER: EXPERIMENTAL / PERSONAL PROJECT**
> 
> This entire codebase was **generated 100% by an AI Assistant**. 
> It was built to solve a specific need on a specific hardware configuration. 
> 
> *   **NO SUPPORT** will be provided.
> *   **NO GUARANTEE** that it will work on your machine.
> *   Use it at your own risk.

## Overview

Holaf-Mix is an audio mixer for Linux systems running **PipeWire**, written in
Rust (interface + audio core); the original Python/PySide6 interface is still
shipped as an escape hatch (`HOLAF_UI=python`). It mimics the workflow of software like Voicemeeter, allowing you to route applications to virtual strips, mix them, and send them to physical outputs or communication apps (Discord, TeamSpeak, etc.).

It features a persistent UI, robust routing logic, and full **MIDI Bidirectional Support** (specifically tested with Akai MIDImix).

## Features

*   **Virtual Strips**: Create unlimited Input/Output strips.
*   **PipeWire Routing**: 
    *   Route specific apps (Firefox, Spotify, Games) to specific strips.
    *   Route virtual strips to physical hardware (Speakers, Headphones).
    *   Create Virtual Busses (B1, B2) that appear as Microphones in other apps.
*   **Mono Toggle**: Downmix stereo sources to mono with a single click.
*   **MIDI Control & Feedback**:
    *   "MIDI Learn" mode for Volume faders, Mute buttons, and Mono toggles.
    *   **LED Feedback**: Updates your controller's LEDs (Mute/Mono) when changed in the UI.
*   **System Integration**:
    *   Minimizes to System Tray.
    *   "Always on Top" mode.
    *   Saves window position and strip configuration on exit.
*   **Visuals**: Real-time VU Meters (20fps).

## Tech Stack

*   **Language**: Rust (UI + audio core) + Python 3.10+ (fallback UI only)
*   **GUI**: **Rust/egui** (`rust/crates/holaf-ui`, binary `holaf-ui`) —
    PySide6 (Qt) remains as the `HOLAF_UI=python` escape hatch
*   **Audio Backend**: the Rust UI drives PipeWire **in-process** through
    `holaf-pw` (`PipeWireBackend`) — no daemon, no IPC.  The Qt fallback talks
    to the Rust `holafd` daemon over a Unix-socket NDJSON protocol, or uses the
    classic Python engine (`HOLAF_USE_PYTHON=1`).
*   **MIDI**: `midir` (Rust UI, LED feedback included) / `mido` + `python-rtmidi`
    (Qt fallback)

## Interface Rust (mode par défaut)

Depuis la bascule **B1**, la fenêtre de Holaf-Mix est le binaire Rust
**`holaf-ui`** (`rust/crates/holaf-ui`, egui/eframe).  `main.py` n'ouvre plus de
fenêtre Qt : c'est un **lanceur** qui localise `holaf-ui` et l'exécute avec les
mêmes arguments (`--config`, `--midi`, `--midi-list`, `--always-on-top`).

```bash
# build de dev : publie dist/{holaf-ui,holafd,holafctl}
# (installation normale : télécharger l'artefact de la CI, voir §Installation)
./scripts/build_rust.sh
python main.py                     # → UI Rust (défaut)
```

*   **Échappatoire** — `HOLAF_UI=python python main.py` (ou
    `HOLAF_USE_PYTHON=1`) : ancienne UI PySide6, comportement inchangé.  Elle
    reste dans le dépôt et testée tant que la bascule n'est pas validée.
*   **Repli** — si `holaf-ui` est introuvable ou ne peut pas s'exécuter, le
    lanceur **logue un WARNING** avec la raison et démarre l'UI Qt : jamais
    d'échec silencieux.  Un binaire qui *démarre* n'est jamais doublé : son code
    de sortie est propagé (c'est ce qui fait qu'un lancement sans écran affiche
    le message d'eframe au lieu d'ouvrir Qt).
*   **Un seul `config.json`** — le lanceur passe `--config` avec le fichier
    qu'utilisait l'UI Qt (à côté de l'exécutable en build packagé), donc les deux
    interfaces partagent la même configuration.
*   **Moteur audio** — l'UI Rust parle à PipeWire **dans son propre processus**
    (`holaf-pw::PipeWireBackend`) : ni `holafd`, ni socket, ni lanceur.
*   **Packagé** — les bundles PyInstaller embarquent `holaf-ui`, `holafd` et
    `holafctl` (résolus dans le dossier de l'app ou dans `sys._MEIPASS`) : le
    tarball fonctionne sans Rust installé.

Le cœur audio Rust (`holafd` + `holafctl`) reste le backend de l'UI Qt.  État
détaillé des jalons et commandes de test :
[`docs/rust-rewrite-status.md`](docs/rust-rewrite-status.md), spec dans
[`docs/rust-rewrite-spec.md`](docs/rust-rewrite-spec.md), plan de suppression du
Python dans [`docs/migration-to-pure-rust.md`](docs/migration-to-pure-rust.md).

## Installation

**Note** : suppose Linux avec **PipeWire** installé et actif.

### Chemin normal — télécharger le build de la CI (aucune compilation)

> Si vous lisez ce fichier **depuis le tarball**, les étapes 1 et 2 sont déjà
> faites : passez à l'étape 3.

1.  **Télécharger l'artefact** : GitHub → **Actions** → workflow
    **Build Holaf-Mix** → le run du commit voulu → **Artifacts** →
    `holaf-mix-<sha>-<arch>`.  Il contient
    `holaf-mix-<version>-<arch>.tar.gz` (le lanceur avec `holaf-ui`, `holafd` et
    `holafctl` embarqués) et `SHA256SUMS`.
2.  **Installer les dépendances runtime** — c'est tout ce que le binaire
    demande, rien n'est à compiler :
    ```bash
    # Arch / CachyOS
    sudo pacman -S pipewire pipewire-audio pipewire-pulse wireplumber libpulse \
                   alsa-lib swh-plugins
    # Debian / Ubuntu
    sudo apt install pipewire pipewire-bin pipewire-pulse wireplumber \
                     pulseaudio-utils libasound2 swh-plugins tap-plugins
    # Arch, only for the TUBE WARMTH strip (AUR-only on Arch):
    yay -S tap-plugins
    # No GTK3/appindicator package: the tray icon is a D-Bus
    # StatusNotifierItem (see docs/dependency-audit.md).
    ```
3.  **Décompresser et lancer** :
    ```bash
    tar xzf holaf-mix-<version>-<arch>.tar.gz -C ~/apps/
    ~/apps/holaf-mix-<version>/holaf-mix
    ```

Le détail (contenu du tarball, `SHA256SUMS`, installation dans
`/usr/local/bin` + entrée de menu, dépendances optionnelles, dépannage) est dans
[`BUILD.md`](BUILD.md) §1 et §3.  `packaging/PKGBUILD` reste disponible comme
**option de distribution Arch** maintenue (paquet géré par `pacman`) : ce n'est
pas le chemin par lequel l'application est livrée, qui passe par les artefacts
ci-dessus — voir `BUILD.md` §4.

### Depuis les sources (développement)

1.  **Cloner le dépôt** :
    ```bash
    git clone https://github.com/your-username/holaf-mix.git
    cd holaf-mix
    ```

2.  **Installer les dépendances** :
    ```bash
    pip install -r requirements.txt
    ```
    La compilation depuis les sources demande en plus un toolchain Rust (et les
    en-têtes PipeWire pour `bindgen`) :
    ```bash
    ./build.sh --tarball        # construit holaf-ui + holafd + le bundle PyInstaller
    ```

3.  **Lancer l'application** :
    ```bash
    python main.py             # UI Rust par défaut (lance holaf-ui)
    # ou l'ancienne interface Qt, inchangée :
    HOLAF_UI=python python main.py
    ```

## Usage

1.  **Add Strips**: Click the `+` button in the Inputs or Outputs section.
2.  **Assign Devices**:
    *   For **Inputs**: Select a physical source or "Apps / Virtual". If Virtual, click "Select Apps" to choose which running programs are routed here.
    *   For **Outputs**: Select your physical speakers/headphones.
3.  **Route**: Click the buttons (e.g., `OUT1`, `OUT2`) on an Input strip to send audio to that Output strip.
4.  **MIDI Learn**:
    *   Click the `MIDI` button on a strip.
    *   Select "Learn Volume/Mute/Mono".
    *   Move a fader or press a button on your controller.
    *   The mapping is saved automatically.
5.  **Right click anywhere** for the application menu (*Restart audio engine*,
    *Debug window*, *Open project page*) — see below.

## Menu contextuel (clic droit)

Un **clic droit n'importe où** dans Holaf-Mix ouvre le menu de l'application
(fond de fenêtre, strips, vumètres, en-têtes de section).  Il contient :

| Entrée | Effet |
| --- | --- |
| **Restart audio engine** | Réinitialise complètement le moteur audio (arrêt du graphe puis reconstruction, avec un daemon `holafd` neuf quand c'est Holaf-Mix qui l'a lancé) **sans quitter l'application**. La barre de statut affiche « Audio engine restarting… » puis « Audio engine restarted » ; un second clic pendant l'opération est ignoré (un seul redémarrage à la fois). En cas d'échec, un message explicite s'affiche et l'app continue de tourner. |
| **Debug window** | Ouvre la fenêtre de diagnostic (voir ci-dessous). Remplace l'ancien raccourci `Ctrl+Shift+D`, qui n'existe plus. |
| **Open project page** | Ouvre <https://github.com/grokuku/Holaf-Mix> dans le navigateur par défaut. |

Deux zones gardent en plus leur menu local, enrichi des entrées ci-dessus :
le **nom d'un strip** (clic droit → *Rename…* puis le menu de l'application) et
les boutons **FX** (clic droit → réglages de l'effet quand il en a).

## Dépannage (`clic droit → Debug window`)

Quand le son craque, qu'un strip ne route pas ou qu'une appli est muette,
il n'y a pas de menu « aide » : lancez Holaf-Mix puis faites un **clic droit
n'importe où → Debug window**.  Une fenêtre de diagnostic s'ouvre.

Elle rassemble, en un seul endroit, ce qu'il fallait sinon demander en
commandes shell (`pactl`, `pw-cli`, `pw-link`, `pw-top`, `holafctl status`) :

| Onglet | Contenu |
| --- | --- |
| **Summary** | Les constats automatiques, colorés par sévérité (erreur / avertissement / info), avec l'action suggérée sous chacun |
| **Engine** | État du moteur (`running`, `degraded`, `pending`, `fx_host`, strips avec leur bus et leurs effets actifs, routes) + métrage (niveaux, sources écoutées) |
| **Graph** | Nœuds `Holaf_*` et devices physiques (id, classe, état, quantum, rate, ports) et liens qui touchent le mix |
| **System** | Versions (app, `holafd`, PipeWire, kernel, Python), socket, `config.json`, PID, CPU/mémoire, et **tous** les processus `holafd` vivants |
| **Journal** | Les 200 dernières lignes de log de l'app (buffer mémoire) |

Boutons :

*   **Refresh** (`F5` / `Ctrl+R`) — relance la collecte **en arrière-plan** (l'UI
    ne gèle jamais, même si le daemon ou PipeWire ne répond pas).
*   **Copy report** — copie un rapport texte complet dans le presse-papiers :
    c'est **ce bloc** qu'il faut coller pour demander de l'aide (il commence
    par les constats).
*   **Save…** — enregistre le même rapport dans un fichier `.txt`.
*   **Auto-refresh every 2 s** — rafraîchit uniquement la partie « live »
    (moteur + métrage), sans relancer `pw-dump`.

Ce que la fenêtre sait détecter toute seule : graphe dégradé ou `pending` non
vide, daemon injoignable, chaînes FX hébergées en mode `inprocess` (risque de
hachage) ou FX host qui redémarre en boucle, noms de bus legacy
`Holaf_Strip_*` (binaire `holafd` périmé → `scripts/build_rust.sh`), plusieurs
daemons `holafd` vivants (orphelins d'une session précédente), strip dont le bus
ou le device a disparu du graphe, quantum trop petit (< 256 → craquements),
`ERR` dans `pw-top`, volumes non appliqués (écart entre la config et le graphe),
effets actifs sans chaîne FX, erreurs dans le log.

Fonctionne avec **les deux moteurs** (daemon Rust par défaut, moteur Python via
`HOLAF_USE_PYTHON=1`) : avec le moteur Python, l'état est reconstruit depuis ses
registres et la liste des strips de l'UI, et les champs indisponibles sont
signalés `?` au lieu d'être inventés.

## License

This project is provided "as-is" without any warranty. feel free to fork and modify it for your own needs.