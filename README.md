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

Holaf-Mix is a Python-based audio mixer designed for Linux systems running **PipeWire**. It mimics the workflow of software like Voicemeeter, allowing you to route applications to virtual strips, mix them, and send them to physical outputs or communication apps (Discord, TeamSpeak, etc.).

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

*   **Language**: Python 3.10+ (UI) + Rust (audio core daemon `holafd`)
*   **GUI**: PySide6 (Qt)
*   **Audio Backend**: Rust `holafd` daemon over a Unix-socket NDJSON protocol
    (PipeWire via `pipewire-rs`).  Classic Python engine available as opt-out
    (PipeWire via `pw-cli`/`pw-dump`) — `sounddevice` is used for VU meters in
    both modes.
*   **MIDI**: `mido` + `python-rtmidi`

## Rust audio core (mode par défaut)

Le cœur audio est un daemon **`holafd`** écrit en Rust (`rust/`, workspace
cargo).  **C'est le mode par défaut** : `main.py` le lance au démarrage
(`holafd --backend pipewire`) et lui parle en NDJSON sur un socket Unix.

```bash
# build de dev : publie dist/holafd (build.sh le fait aussi automatiquement)
./scripts/build_rust.sh
python main.py                     # → daemon Rust (défaut)
```

*   **Opt-out** — `HOLAF_USE_PYTHON=1 python main.py` : `AudioEngine` Python
    classique (aucun daemon lancé).  `HOLAF_USE_RUST=0` reste accepté.
*   **Repli** — si le daemon ne peut pas démarrer (binaire absent, socket
    occupé, daemon mort au boot), l'app **logue un WARNING**, affiche la raison
    dans la barre de statut et bascule sur le moteur Python : le repli n'est
    jamais silencieux.
*   **Debug sans audio** — `HOLAFD_BACKEND=fake python main.py` (moteur en
    mémoire) ; `$HOLAFD_BIN` / `$HOLAF_SOCKET` surchargent binaire et socket.
*   **Packagé** — les bundles PyInstaller embarquent `holafd` (résolu dans le
    dossier de l'app ou dans `sys._MEIPASS`) : le tarball fonctionne sans Rust
    installé.

Le MIDI, les VU-mètres et la découverte devices/apps (`pw-dump`/`pactl`) sont
encore côté Python.  État détaillé des jalons (J0–J5), commandes de test et
incertitudes API pipewire-rs : [`docs/rust-rewrite-status.md`](docs/rust-rewrite-status.md),
spec complète dans [`docs/rust-rewrite-spec.md`](docs/rust-rewrite-spec.md).

## Installation

**Note**: This assumes you are on Linux and have **PipeWire** installed and active.

1.  **Clone the repository**:
    ```bash
    git clone https://github.com/your-username/holaf-mix.git
    cd holaf-mix
    ```

2.  **Install dependencies**:
    ```bash
    pip install -r requirements.txt
    ```
    The Rust daemon also needs a Rust toolchain (and the PipeWire dev headers
    for `bindgen`) — or use a packaged build, which embeds `holafd`:
    ```bash
    ./build.sh --tarball        # builds holafd + the PyInstaller bundle
    ```

3.  **Run the application**:
    ```bash
    python main.py             # Rust backend by default
    # or, without a Rust daemon:
    HOLAF_USE_PYTHON=1 python main.py
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

## License

This project is provided "as-is" without any warranty. feel free to fork and modify it for your own needs.