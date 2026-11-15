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

Holaf-Mix is an audio mixer for Linux systems running **PipeWire**, written
**entirely in Rust** — the interface (`holaf-ui`, egui/eframe), the audio core
(`holaf-pw`) and the daemon/CLI (`holafd`, `holafctl`).  There is **no Python**
in the product: no interpreter, no PyInstaller bundle, no venv (see
[`docs/python-removal.md`](docs/python-removal.md)). It mimics the workflow of software like Voicemeeter, allowing you to route applications to virtual strips, mix them, and send them to physical outputs or communication apps (Discord, TeamSpeak, etc.).

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
    *   Minimizes to System Tray (D-Bus StatusNotifierItem, no GTK).
    *   "Always on Top" mode.
    *   Saves window position and strip configuration on exit.
*   **Visuals**: Real-time VU Meters (20fps).

## Tech Stack

*   **Language**: Rust only (UI + audio core + daemon + CLI).
*   **GUI**: **Rust/egui** (`rust/crates/holaf-ui`, binary `holaf-ui`).
*   **Audio Backend**: the UI drives PipeWire **in-process** through
    `holaf-pw` (`PipeWireBackend`) — no daemon, no IPC.  `holafd` is an
    alternative/automation backend, `holafctl` its CLI.
*   **MIDI**: `midir` (ALSA sequencer, LED feedback included).

## Lancer Holaf-Mix

```bash
./holaf-ui                 # l'application (c'est le point d'entrée)
./holaf-ui --help          # --config, --midi, --midi-list, --always-on-top
./holaf-mix                # même chose : petit script shell qui exec ./holaf-ui
```

Le chemin normal est l'**artefact CI** (§Installation) : rien n'est compilé sur
la machine de l'utilisateur.  Pour un build local :
[`BUILD.md`](BUILD.md) §5 (`./build.sh --run`).

### Configuration

`config.json` est cherché dans cet ordre (documenté dans
`holaf_core::config::default_config_path`) :

1.  `--config FICHIER` (priorité absolue) ;
2.  `$HOLAF_CONFIG` ;
3.  à côté du binaire (`<dossier de holaf-ui>/config.json`), s'il existe ;
4.  `./config.json` du répertoire courant, s'il existe ;
5.  `$XDG_CONFIG_HOME/holaf-mix/config.json` (défaut
    `~/.config/holaf-mix/config.json`) — créé au premier lancement.

Conséquence : lancer `./holaf-ui` depuis le dossier du tarball ouvre le
`config.json` de ce dossier s'il existe (sinon `~/.config/holaf-mix/`), et une
entrée de menu (dont le répertoire courant est `$HOME`) ne crée pas un fichier
au hasard : elle utilise celui installé/XDG.

## Installation

**Note** : suppose Linux avec **PipeWire** installé et actif.

### Chemin normal — télécharger le build de la CI (aucune compilation)

> Si vous lisez ce fichier **depuis le tarball**, les étapes 1 et 2 sont déjà
> faites : passez à l'étape 3.

1.  **Télécharger l'artefact** : GitHub → **Actions** → workflow
    **Build Holaf-Mix** → le run du commit voulu → **Artifacts** →
    `holaf-mix-<sha>-<arch>`.  Il contient
    `holaf-mix-<version>-<arch>.tar.gz` (les trois binaires Rust + icônes +
    docs) et `SHA256SUMS`.
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
    ~/apps/holaf-mix-<version>/holaf-ui
    ```

Le détail (contenu du tarball, `SHA256SUMS`, installation dans
`/usr/local/bin` + entrée de menu, dépendances optionnelles, dépannage) est dans
[`BUILD.md`](BUILD.md) §1 et §3.  `packaging/PKGBUILD` reste disponible comme
**option de distribution Arch** maintenue (paquet géré par `pacman`) : ce n'est
pas le chemin par lequel l'application est livrée, qui passe par l'artefact
ci-dessus — voir `BUILD.md` §4.

### Depuis les sources (développement)

1.  **Cloner le dépôt** :
    ```bash
    git clone https://github.com/your-username/holaf-mix.git
    cd holaf-mix
    ```

2.  **Installer les dépendances de build** (toolchain Rust + en-têtes
    PipeWire/ALSA — la liste exacte par distro est dans `BUILD.md` §5.1) :
    ```bash
    sudo pacman -S --needed base-devel pkgconf clang rust \
        librsvg imagemagick pipewire alsa-lib \
        wayland wayland-protocols libxkbcommon
    ```

3.  **Construire et lancer** :
    ```bash
    ./build.sh --run           # icônes + cargo build + lancement de dist/holaf-ui
    # ou :
    ./build.sh --tarball       # produit dist/holaf-mix-<version>-<arch>.tar.gz
    ./dist/holaf-ui
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
| **Restart audio engine** | Réinitialise complètement le moteur audio (arrêt du graphe puis reconstruction) **sans quitter l'application**. La barre de statut affiche « Audio engine restarting… » puis « Audio engine restarted » ; un second clic pendant l'opération est ignoré (un seul redémarrage à la fois). En cas d'échec, un message explicite s'affiche et l'app continue de tourner. |
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
| **System** | Versions (app, `holafd`, PipeWire, kernel), `config.json`, PID, CPU/mémoire, et **tous** les processus `holafd` vivants |
| **Journal** | Les 200 dernières lignes de log de l'app (buffer mémoire) |

Boutons :

*   **Refresh** (`F5` / `Ctrl+R`) — relance la collecte **en arrière-plan** (l'UI
    ne gèle jamais, même si PipeWire ne répond pas).
*   **Copy report** — copie un rapport texte complet dans le presse-papiers :
    c'est **ce bloc** qu'il faut coller pour demander de l'aide (il commence
    par les constats).
*   **Save…** — enregistre le même rapport dans un fichier `.txt`.
*   **Auto-refresh every 2 s** — rafraîchit uniquement la partie « live »
    (moteur + métrage), sans relancer `pw-dump`.

Ce que la fenêtre sait détecter toute seule : graphe dégradé ou `pending` non
vide, chaînes FX hébergées en mode `inprocess` (risque de hachage) ou FX host qui
redémarre en boucle, noms de bus legacy `Holaf_Strip_*` (binaire `holafd` périmé →
`scripts/build_rust.sh`), plusieurs daemons `holafd` vivants (orphelins d'une
session précédente), strip dont le bus ou le device a disparu du graphe, quantum
trop petit (< 256 → craquements), `ERR` dans `pw-top`, volumes non appliqués
(écart entre la config et le graphe), effets actifs sans chaîne FX, erreurs dans
le log.

État détaillé des jalons et commandes de test :
[`docs/rust-rewrite-status.md`](docs/rust-rewrite-status.md), spec dans
[`docs/rust-rewrite-spec.md`](docs/rust-rewrite-spec.md), et l'inventaire de la
suppression du Python dans [`docs/python-removal.md`](docs/python-removal.md).

## License

This project is provided "as-is" without any warranty. feel free to fork and modify it for your own needs.
