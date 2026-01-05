# Project Context: Holaf-Mix

    ## 1. Project Overview
    - **Goal**: Linux audio mixer (Voicemeeter-like) using PipeWire and PySide6.
    - **Current Phase**: **MIDI INTEGRATION & OPTIMIZATION**.
    - **Status**: Functional Audio & MIDI Engine. Stable release candidate for core features (Mixing, Routing, MIDI Control).

    ## 2. Architecture (Hybrid MVC)
    - **Model**: `Strip` class. It is the single source of truth for the state (volume, mute, name).
    - **View (UI)**: `MainWindow` and `StripWidget`. They reflect the Model state.
    - **Controller (Backend)**: 
        - `AudioEngine`: Translates Model state changes into PipeWire commands.
        - `MidiEngine`: Translates Hardware events into Model state changes.
    - **Data Flow (Crucial)**: 
        - **UI Input**: Slider Move -> Update Model -> Timer (10Hz) -> `AudioEngine` -> PipeWire.
        - **MIDI Input**: MIDI Msg -> `MidiEngine` -> Signal -> Update Model -> Update UI -> Timer (10Hz) -> `AudioEngine` -> PipeWire.
        - *Note*: MIDI never calls `AudioEngine` directly to prevent lag.

    ## 3. Project File Structure (Map & Responsibilities)

    ```text
    Holaf_Mix/
    ├── main.py                     # [ENTRY POINT] Bootstrapper. Initializes Audio/Midi engines, UI, and connects Shutdown signals.
    ├── pipewire_utils.py           # [LOW-LEVEL] Wrapper for subprocess calls to `pw-cli`, `pw-dump`, `pw-link`. No logic, just execution.
    ├── config.json                 # [PERSISTENCE] Stores the list of Strips, their volume/mute state, routing, and MIDI mappings.
    ├── project_context.md          # [MEMORY] This file. The absolute reference for project state and rules.
    ├── requirements.txt            # [DEPENDENCIES] PySide6, mido, python-rtmidi.
    │
    ├── src/
    │   ├── backend/
    │   │   ├── audio_engine.py     # [CONTROLLER] High-level logic. Manages Nodes creation/destruction, Linking, Volume/Mute logic.
    │   │   └── midi_engine.py      # [CONTROLLER] Runs a daemon thread listening to MIDI ports. Emits signals on valid messages.
    │   │
    │   ├── config/
    │   │   └── settings.py         # [IO] Handles loading/saving `config.json` and deserializing into `Strip` objects.
    │   │
    │   ├── models/
    │   │   └── strip_model.py      # [MODEL] Data class defining a Strip (UID, Label, Volume, Mute, Routes, MIDI Mappings).
    │   │
    │   └── ui/
    │       ├── main_window.py      # [VIEW] Main container. Manages the global layout (Inputs/Outputs columns) and MIDI signal dispatching.
    │       └── widgets/
    │           └── strip_widget.py # [VIEW] The channel strip UI. Contains the Volume Slider, Mute Btn, Routing Btns.
    │                               # *CRITICAL*: Contains the `QTimer` logic for throttling backend calls (10Hz).
    ```

    ## 4. Technical Implementation Details (State: Jan 06, 2026)

    ### Audio Engine Logic
    - **Virtual Nodes**: Created using `pactl load-module module-null-sink` to ensure they appear correctly in system mixers (KDE/Gnome) with "Post-Fader" behavior (Monitor output reflects sink volume).
    - **Naming**: Nodes are named `Holaf_Strip_[UID]`. Descriptions are quoted (`"{description}"`) to handle spaces correctly.
    - **Mute Handling**: 
        - Uses `pactl set-sink-mute [name] [0|1]` instead of `pw-cli` props. 
        - This ensures the mute state is visible and synced with the Desktop Environment (OSD).
    - **Default Sink**: On startup, the engine scans for an Input Strip named "Desktop" and enforces it as the system default sink (`pactl set-default-sink`).

    ### MIDI & Performance Logic
    - **Decoupling**: The MIDI thread puts data into the Main Thread (via Signals). The Main Thread updates the UI/Model.
    - **Throttling**: The `StripWidget` has a `QTimer` running at 100ms (10Hz). It only sends a volume command to PipeWire if the Model's volume has changed since the last tick. This absorbs the flood of MIDI messages (often >100/sec).
    - **MIDI Learn**: Implemented via context menu on the "MIDI" button. Saves mapping (CC or Note) into `config.json`.

    ## 5. Features Status
    - [x] **Core Audio**: Create/Delete Virtual Strips, Detect Physical Devices.
    - [x] **Routing**: Matrix routing (Inputs -> Outputs) works reliably.
    - [x] **Persistence**: State is saved/restored on restart.
    - [x] **MIDI Control**: Full bidirectional control (UI updates on MIDI input).
    - [x] **UX/UI**: 
        - Visual feedback for Routing (Green buttons).
        - Visual feedback for "Learning Mode" (Orange border).
        - System Tray / Desktop Integration (Correct names).

    ## 6. Environment & Dependencies
    - **Python**: 3.10+
    - **Libraries**: 
        - `PySide6` (GUI)
        - `mido` + `python-rtmidi` (MIDI)
    - **System Tools**: 
        - `pipewire` (Core audio server)
        - `pipewire-pulse` (Provides `pactl` compatibility - Required)
        - `pw-cli` & `pw-link` (WirePlumber/PipeWire tools)

    ## 7. Known Considerations
    - **Hot-Plugging**: Adding/Removing a strip currently triggers a full engine restart (`start_engine`) to ensure the PipeWire graph and internal registries stay in sync.
    - **Latency**: There is an intentional max delay of 100ms on volume changes reaching the audio engine (due to throttling), which is imperceptible to the user but vital for CPU stability.