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

*   **Language**: Python 3.10+
*   **GUI**: PySide6 (Qt)
*   **Audio Backend**: PipeWire (via `pw-cli` and `pw-dump` wrappers) + `sounddevice` (for VU meters)
*   **MIDI**: `mido` + `python-rtmidi`

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

3.  **Run the application**:
    ```bash
    python main.py
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