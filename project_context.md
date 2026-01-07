## 1. Project Overview
    - **Goal**: Linux audio mixer (Voicemeeter-like) using PipeWire and PySide6.
    - **Current Phase**: **STABILIZATION & FEATURE COMPLETION**.
    - **Status**: 
        - Functional Audio & MIDI Engine.
        - Reliable App Routing & Hardware Volume control.
        - **STABLE**: VU Meters work for Inputs, Virtual strips, and Physical Outputs.
        - **SOLVED**: Virtual Output Busses (B1/B2) are now exposed as proper Input Devices to third-party apps (Discord/TeamSpeak) via `module-remap-source`.
        - **POLISHED**: Window geometry persistence, "Always on Top" mode, and System Tray integration.
        - **NEW**: MIDI Feedback (Controller LEDs light up for Mute/Mono states).

    ## 2. Architecture (Hybrid MVC)
    - **Model**: `Strip` class. Single source of truth. Includes `is_mono` state and persistence data.
    - **View (UI)**: `MainWindow` (System Tray, Geometry Management) and `StripWidget`.
    - **Controller (Backend)**: 
        - `AudioEngine`: Manages PipeWire Graph, Routing, Mute/Volume logic, Mono Downmixing, and **Remapping Sources**.
        - `MidiEngine`: Handles Hardware MIDI events (Notes & CC) AND **Feedback (Output)**.
        - `MeteringEngine`: **Async & Thread-Safe**.
    - **Data Flow**: 
        - **UI Input**: Slider/Button -> Update Model -> Signal -> `MainWindow` -> `AudioEngine` -> PipeWire.
        - **MIDI Input**: Controller -> `MidiEngine` -> Signal -> `MainWindow` -> UI & AudioEngine.
        - **MIDI Output**: UI Change -> `MainWindow` -> `MidiEngine.send_feedback()` -> Controller LED.
        - **Meters**: PipeWire -> `sounddevice` -> Threaded Callback -> UI Timer (20FPS).

    ## 3. Project File Structure (Map & Responsibilities)

    ```text
    Holaf_Mix/
    ├── main.py                     # [ENTRY POINT] Bootstrapper.
    ├── pipewire_utils.py           # [LOW-LEVEL] Wrapper for `pw-cli`, `pw-dump`.
    ├── config.json                 # [PERSISTENCE] Stores Strips state & Window Geometry.
    ├── project_context.md          # [MEMORY] Project state and rules.
    ├── requirements.txt            # [DEPENDENCIES] PySide6, mido, python-rtmidi, sounddevice, numpy.
    │
    ├── src/
    │   ├── backend/
    │   │   ├── audio_engine.py     # [CONTROLLER] Manages Nodes, Links, Mono logic & Source Remapping.
    │   │   ├── midi_engine.py      # [CONTROLLER] MIDI I/O. Handles Learn Mode & LED Feedback.
    │   │   └── metering.py         # [CONTROLLER] Async Sounddevice engine.
    │   │
    │   ├── config/
    │   │   └── settings.py         # [IO] JSON Serialization (Strips + Window State).
    │   │
    │   ├── models/
    │   │   └── strip_model.py      # [MODEL] Data structure (added is_mono, midi_mono).
    │   │
    │   └── ui/
    │       ├── main_window.py      # [VIEW] Orchestrates UI, System Tray, Window Flags, MIDI Sync.
    │       └── widgets/
    │           └── strip_widget.py # [VIEW] Visual VUMeterWidget. Includes Throttling & Alignment logic.
    ```

    ## 4. Technical Implementation Details (State: Jan 07, 2026)

    ### Audio Engine Logic
    - **Virtual Bus Creation**: 
        1. Creates `module-null-sink`.
        2. Identifies the auto-generated monitor.
        3. Creates a **`module-remap-source`** pointing to that monitor. This "blanches" the stream so apps see it as a microphone, not a monitor.
    - **Routing & Mono**: 
        - Standard Stereo: FL->FL, FR->FR.
        - Mono Mode: Cross-links all source channels to all destination channels.
    - **Mute Logic**: Muting a Sink also mutes its Monitor.

    ### MIDI Logic
    - **Learning Mode**: Can learn Volume (Axis/Fader), Mute (Button), and Mono (Button).
    - **Differentiation**: 
        - Volume uses raw value (0-127).
        - Buttons (Mute/Mono) toggle on NoteOn/ControlChange with velocity > 0.
    - **Feedback (LEDs)**:
        - Uses `mido.open_output` on the same port name.
        - Sends `NoteOn` with Velocity 127 (ON) or 0 (OFF).
        - Synced on App Startup & UI interaction.

    ### UI Logic
    - **Window Behavior**: 
        - Always on Top.
        - Close button (X) minimizes to System Tray.
        - Position and Size are saved/restored on restart.
    - **Visuals**:
        - VU Meters refresh at 20Hz.
        - Mono Mode forces VU meters to display the max signal on both bars.
        - Buttons are properly aligned using `RetainSizeWhenHidden` policies.

    ## 5. Features Status
    - [x] **Core Audio**: Create/Delete Virtual Strips, Detect Physical Devices.
    - [x] **Hardware Routing**: Mics to Inputs, Outputs to Speakers.
    - [x] **Volume/Mute Control**: Syncs UI, Sink, and Hardware Monitor.
    - [x] **App Routing**: Apps strictly follow their assigned strip.
    - [x] **Default Sink**: Checkbox to define system-wide default output.
    - [x] **Virtual Busses**: Creation logic & UI feedback.
    - [x] **Mono Mode**: UI, Audio Engine, and MIDI support.
    - [x] **Exposure to Apps**: Virtual Bus Monitors are remapped and visible in Discord/TeamSpeak.
    - [x] **Persistence**: Strips state and Window geometry saved.
    - [x] **System Tray**: App runs in background, minimizes to tray.
    - [x] **MIDI Feedback**: Buttons on controller light up to reflect Mute/Mono state.

    ## 6. Known Considerations & Issues
    - **ALSA Latency**: On system load, opening a stream might take seconds (Handled by async metering).
    - **System Tray**: Requires a desktop environment that supports Status Notifiers (Gnome, KDE, XFCE support this well).

    ## 7. Roadmap (Next Steps)
    1.  **Refactoring**: Clean up redundant code in `audio_engine.py` (optimize node lookup).
    2.  **Visual Polish**: Improve Routing buttons styling (make them more distinct).
    3.  **Packaging**: Prepare for standalone distribution (PyInstaller).

    ## 8. Project Identity & Disclaimer
    - **AI Generated**: 100% of this codebase was generated by an AI assistant.
    - **Personal Tool**: Built for a specific hardware configuration.
    - **No Support**: No guarantee of compatibility with other systems. Use at your own risk.