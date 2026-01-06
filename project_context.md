## 1. Project Overview
    - **Goal**: Linux audio mixer (Voicemeeter-like) using PipeWire and PySide6.
    - **Current Phase**: **STABILIZATION & FEATURE COMPLETION**.
    - **Status**: 
        - Functional Audio & MIDI Engine.
        - Reliable App Routing & Hardware Volume control.
        - **STABLE**: VU Meters work for Inputs, Virtual strips, and Physical Outputs.
        - **NEW**: Implementation of "Virtual Output Busses" (B1/B2 style).
        - **NEW**: Mono Mode implemented (UI button + Audio Downmix + MIDI mapping).

    ## 2. Architecture (Hybrid MVC)
    - **Model**: `Strip` class. Single source of truth. Now includes `is_mono` state.
    - **View (UI)**: `MainWindow` and `StripWidget`.
    - **Controller (Backend)**: 
        - `AudioEngine`: Manages PipeWire Graph, Routing, Mute/Volume logic, and **Mono Downmixing**.
        - `MidiEngine`: Handles Hardware MIDI events (Notes & CC).
        - `MeteringEngine`: **Async & Thread-Safe**.
    - **Data Flow**: 
        - **UI Input**: Slider/Button -> Update Model -> Signal -> `MainWindow` -> `AudioEngine` -> PipeWire.
        - **Meters**: PipeWire -> `sounddevice` -> Threaded Callback -> UI Timer (25FPS).

    ## 3. Project File Structure (Map & Responsibilities)

    ```text
    Holaf_Mix/
    ├── main.py                     # [ENTRY POINT] Bootstrapper.
    ├── pipewire_utils.py           # [LOW-LEVEL] Wrapper for `pw-cli`, `pw-dump`.
    ├── config.json                 # [PERSISTENCE] Stores Strips state (including Mono/MIDI).
    ├── project_context.md          # [MEMORY] Project state and rules.
    ├── requirements.txt            # [DEPENDENCIES] PySide6, mido, python-rtmidi, sounddevice, numpy.
    │
    ├── src/
    │   ├── backend/
    │   │   ├── audio_engine.py     # [CONTROLLER] Manages Nodes & Links. Handles Mono mixing logic.
    │   │   ├── midi_engine.py      # [CONTROLLER] MIDI Listener (Threaded).
    │   │   └── metering.py         # [CONTROLLER] Async Sounddevice engine.
    │   │
    │   ├── config/
    │   │   └── settings.py         # [IO] JSON Serialization.
    │   │
    │   ├── models/
    │   │   └── strip_model.py      # [MODEL] Data structure (added is_mono, midi_mono).
    │   │
    │   └── ui/
    │       ├── main_window.py      # [VIEW] Orchestrates UI updates & Signals.
    │       └── widgets/
    │           └── strip_widget.py # [VIEW] Visual VUMeterWidget. Includes Mono/Mute buttons.
    ```

    ## 4. Technical Implementation Details (State: Jan 06, 2026)

    ### Audio Engine Logic
    - **Virtual Bus Creation**: Uses `pactl load-module module-null-sink`.
    - **Routing & Mono**: 
        - Standard Stereo: FL->FL, FR->FR.
        - Mono Mode: Cross-links all source channels to all destination channels (FL->FL, FL->FR, FR->FL, FR->FR).
    - **Mute Logic**: Muting a Sink also mutes its Monitor.

    ### MIDI Logic
    - **Learning Mode**: Can learn Volume (Axis/Fader), Mute (Button), and Mono (Button).
    - **Differentiation**: 
        - Volume uses raw value (0-127).
        - Buttons (Mute/Mono) toggle on NoteOn/ControlChange with velocity > 0 (Press only, ignores Release).

    ### UI Logic
    - **Virtual vs Physical**: 
        - Physical Outputs (Red).
        - Virtual Outputs (Purple).
        - Inputs (Blue).
    - **Robust Device Selection**: Handles missing devices gracefully.

    ## 5. Features Status
    - [x] **Core Audio**: Create/Delete Virtual Strips, Detect Physical Devices.
    - [x] **Hardware Routing**: Mics to Inputs, Outputs to Speakers.
    - [x] **Volume/Mute Control**: Syncs UI, Sink, and Hardware Monitor.
    - [x] **App Routing**: Apps strictly follow their assigned strip.
    - [x] **Default Sink**: Checkbox to define system-wide default output.
    - [x] **Virtual Busses**: Creation logic & UI feedback.
    - [x] **Mono Mode**:
        - [x] Button added to UI.
        - [x] Audio Engine supports Downmix.
        - [x] MIDI Mapping support.
    - [ ] **Exposure to Apps**: The bus "Monitor" is created but not visible in Discord/TeamSpeak.

    ## 6. Known Considerations & Issues
    - **Virtual Bus Visibility**: Although the Null Sink is created, its `monitor` source is often hidden by PipeWire/PulseAudio in the device lists of "Legacy" applications (Discord, TeamSpeak). Requires `module-remap-source`.
    - **ALSA Latency**: On system load, opening a stream might take seconds (Handled by async metering).

    ## 7. Roadmap (Next Steps)
    1.  **CRITICAL FIX**: Make Virtual Bus Monitors visible in Discord/TeamSpeak (Implement `module-remap-source` logic discussed).
    2.  **Visual Polish**: Improve Routing buttons styling.
    3.  **Refactoring**: Clean up redundant code in `audio_engine.py`.
    4.  **Packaging**: Prepare for standalone distribution (PyInstaller).