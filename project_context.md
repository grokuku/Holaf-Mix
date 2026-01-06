# Project Context: Holaf-Mix

    ## 1. Project Overview
    - **Goal**: Linux audio mixer (Voicemeeter-like) using PipeWire and PySide6.
    - **Current Phase**: **HARDWARE INTEGRATION & APP ROUTING**.
    - **Status**: 
        - functional Audio & MIDI Engine.
        - Physical Input/Output selection working.
        - App assignment UI implemented (Backend routing currently under debugging).

    ## 2. Architecture (Hybrid MVC)
    - **Model**: `Strip` class. Single source of truth (UID, Label, Volume, Mute, Routes, Device Name, Assigned Apps, MIDI Mappings).
    - **View (UI)**: `MainWindow` and `StripWidget`.
    - **Controller (Backend)**: 
        - `AudioEngine`: Manages PipeWire Graph (Nodes, Links, Volumes).
        - `MidiEngine`: Handles Hardware MIDI events.
    - **Data Flow**: 
        - **UI Input**: Slider Move -> Update Model -> Timer (10Hz) -> `AudioEngine` -> PipeWire.
        - **MIDI Input**: MIDI Msg -> `MidiEngine` -> Signal -> Update Model -> Update UI -> Timer (10Hz) -> `AudioEngine` -> PipeWire.

    ## 3. Project File Structure (Map & Responsibilities)

    ```text
    Holaf_Mix/
    ├── main.py                     # [ENTRY POINT] Bootstrapper.
    ├── pipewire_utils.py           # [LOW-LEVEL] Wrapper for `pw-cli`, `pw-dump`, `pactl`.
    │                               # *UPDATED*: Added `get_sink_inputs`, `move_sink_input`.
    ├── config.json                 # [PERSISTENCE] Stores Strips state, routing, devices, and app assignments.
    ├── project_context.md          # [MEMORY] Project state and rules.
    ├── requirements.txt            # [DEPENDENCIES] PySide6, mido, python-rtmidi.
    │
    ├── src/
    │   ├── backend/
    │   │   ├── audio_engine.py     # [CONTROLLER] 
    │   │   │                       # - Creates Virtual Nodes (pw-cli preferred).
    │   │   │                       # - Links Physical Mics to Virtual Strips.
    │   │   │                       # - Manages Volume via pactl (Hardware compat).
    │   │   │                       # - Cleans "Zombie" nodes on startup.
    │   │   └── midi_engine.py      # [CONTROLLER] MIDI Listener thread.
    │   │
    │   ├── config/
    │   │   └── settings.py         # [IO] JSON Serialization.
    │   │
    │   ├── models/
    │   │   └── strip_model.py      # [MODEL] Added `device_name` (Hardware) and `assigned_apps` (List[str]).
    │   │
    │   └── ui/
    │       ├── main_window.py      # [VIEW] 
    │       │                       # - Manages `AppSelectionDialog`.
    │       │                       # - Orchestrates App Routing (move_sink_input).
    │       └── widgets/
    │           └── strip_widget.py # [VIEW] 
    │                               # - Added `QComboBox` for Device Selection.
    │                               # - Added "Select Apps" button (Input/Virtual mode).
    │                               # - Added Label Renaming (Double-click).
    ```

    ## 4. Technical Implementation Details (State: Jan 06, 2026)

    ### Audio Engine Logic
    - **Node Creation Strategy**:
        - **Primary**: `pw-cli create-node adapter ...` used to create Null Sinks. This ensures `node.description` is correctly quoted and visible in OS Mixers.
        - **Fallback**: `pactl load-module module-null-sink` (used previously, kept as backup).
    - **Cleanup**: On startup, `_clean_zombie_nodes()` scans for any node named `Holaf_Strip_*` and destroys it to prevent duplicates/ghosts.
    - **Naming**: Nodes are named `Holaf_Strip_[UID]`. Descriptions are formatted as `Holaf: [UserLabel]`.
    - **Default Sink**: Logic searches for Strip labeled "Desktop" -> then "Default" -> then First Input to set as System Default.

    ### Hardware & Volume Logic
    - **Volume Control**: moved to `pactl set-sink-volume [name] [N]%`. This ensures that changing volume in Holaf-Mix moves the hardware fader (and OSD) in KDE/Gnome.
    - **Physical Inputs**: If a Physical Source (Mic) is selected on an Input Strip, the engine creates a `pw-link` from the Mic's ports to the Virtual Strip's inputs.

    ### App Routing Logic
    - **Detection**: `pipewire_utils.get_sink_inputs()` scans for running audio streams (Firefox, Spotify).
    - **Assignment**: `Strip` model holds a list of app names (`assigned_apps`).
    - **Enforcement**: `MainWindow` attempts to move streams using `pactl move-sink-input` when assignments change or periodically.

    ## 5. Features Status
    - [x] **Core Audio**: Create/Delete Virtual Strips, Detect Physical Devices.
    - [x] **Renaming**: Double-click on strip label to rename.
    - [x] **Hardware Routing**: 
        - Output Strips can map to Physical Speakers.
        - Input Strips can accept Physical Microphones.
    - [x] **Persistence**: State (including Device selection & App lists) saved/restored.
    - [x] **MIDI Control**: Full bidirectional control + Learning.
    - [x] **UI/UX**: 
        - Device Selector (Combo Box).
        - App Selection Dialog (Checkboxes).
    - [ ] **App Routing**: Logic implemented but currently buggy (Apps do not switch sinks effectively).

    ## 6. Environment & Dependencies
    - **Python**: 3.10+
    - **Libraries**: `PySide6`, `mido`, `python-rtmidi`.
    - **System Tools**: `pipewire`, `pipewire-pulse`, `pw-cli`, `pactl`, `pw-link`.

    ## 7. Known Considerations & Issues
    - **Hot-Plugging**: Adding/Removing a strip triggers a full engine restart.
    - **Latency**: 100ms UI throttling on volume.
    - **App Routing Bug**: Selecting an app in the UI does not currently force it to the target strip; it remains on Default.

    ## 8. Roadmap (Next Steps)
    1.  **Fix App Routing**: Debug `pactl move-sink-input` logic (potential ID mismatch or timing issue).
    2.  **Default Strip Toggle**: Add a checkbox on Input Strips to explicitly set one as the "System Default".
        - *Logic*: If checked, disables "Select Apps" for this strip (it catches everything else).
    3.  **Window Persistence**: Save/Restore Window Size and Position in `config.json`.
    4.  **Auto-Resize**: 
        - Automatically adjust Window Width based on the number of active strips.
        - Disable horizontal manual resizing.