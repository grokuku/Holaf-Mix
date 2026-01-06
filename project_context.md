## 1. Project Overview
    - **Goal**: Linux audio mixer (Voicemeeter-like) using PipeWire and PySide6.
    - **Current Phase**: **STABILIZATION & POLISH**.
    - **Status**: 
        - Functional Audio & MIDI Engine.
        - Reliable App Routing & Hardware Volume control.
        - UI Layout fixed & Cleaned.
        - **STABLE**: VU Meters work for Inputs, Virtual strips, and Physical Outputs (Race conditions fixed).
        - **STABLE**: Mute/Volume logic handles both Sources (Mics) and Sinks (Speakers) correctly.

    ## 2. Architecture (Hybrid MVC)
    - **Model**: `Strip` class. Single source of truth.
    - **View (UI)**: `MainWindow` and `StripWidget`.
    - **Controller (Backend)**: 
        - `AudioEngine`: Manages PipeWire Graph, Routing, and Mute/Volume logic (distinguishes Source vs Sink).
        - `MidiEngine`: Handles Hardware MIDI events.
        - `MeteringEngine`: **Async & Thread-Safe**. Uses `sounddevice` with strictly locked `os.environ` manipulation to prevent thread collisions.
    - **Data Flow**: 
        - **UI Input**: Slider Move -> Update Model -> Timer (10Hz) -> `AudioEngine` -> PipeWire.
        - **Meters**: PipeWire -> `sounddevice` (ALSA/Pulse plugin) -> Threaded Callback -> UI Timer (25FPS).

    ## 3. Project File Structure (Map & Responsibilities)

    ```text
    Holaf_Mix/
    ├── main.py                     # [ENTRY POINT] Bootstrapper.
    ├── pipewire_utils.py           # [LOW-LEVEL] Wrapper for `pw-cli`, `pw-dump`. Extracts `monitor_source_name`.
    ├── config.json                 # [PERSISTENCE] Stores Strips state.
    ├── project_context.md          # [MEMORY] Project state and rules.
    ├── requirements.txt            # [DEPENDENCIES] PySide6, mido, python-rtmidi, sounddevice, numpy.
    │
    ├── src/
    │   ├── backend/
    │   │   ├── audio_engine.py     # [CONTROLLER] Manages Nodes. Uses `monitor_cache` and `is_source_registry`.
    │   │   ├── midi_engine.py      # [CONTROLLER] MIDI Listener.
    │   │   └── metering.py         # [CONTROLLER] Async Sounddevice engine. Prevents env var collisions.
    │   │
    │   ├── config/
    │   │   └── settings.py         # [IO] JSON Serialization.
    │   │
    │   ├── models/
    │   │   └── strip_model.py      # [MODEL] Data structure.
    │   │
    │   └── ui/
    │       ├── main_window.py      # [VIEW] Orchestrates UI updates & filtering.
    │       └── widgets/
    │           └── strip_widget.py # [VIEW] Visual VUMeterWidget & Control logic.
    ```

    ## 4. Technical Implementation Details (State: Jan 06, 2026)

    ### Audio Engine Logic
    - **Discovery Strategy**: Instead of guessing `.monitor` names, the engine now queries `pactl` for the explicit `monitor_source_name` property of Sinks.
    - **Source vs Sink**: The engine maintains an `is_source_registry` to differentiate Physical Inputs (Sources) from Outputs (Sinks).
        - **Mute Source**: Uses `pactl set-source-mute`.
        - **Mute Sink**: Uses `pactl set-sink-mute` AND `set-source-mute` (for the monitor) to ensure visual sync.

    ### Metering Logic (Stabilized)
    - **Library**: `sounddevice` targeting `pulse` ALSA device.
    - **Thread Safety (Critical)**: A dedicated `creation_lock` protects the `os.environ["PULSE_SOURCE"]` assignment block. This prevents race conditions during the mass-startup of metering threads.
    - **Retry Logic**: Failed streams (due to hardware latency) are added to a `pending_retries` queue and re-attempted every 2 seconds.

    ### UX / Filtering
    - `pipewire_utils.get_audio_nodes` filters out internal nodes for the UI but retrieves full metadata (including monitors) for the backend.
    - **UI Mode**: Hides `Holaf_Strip_*`, `Monitor of...`, and `(null)` devices.

    ## 5. Features Status
    - [x] **Core Audio**: Create/Delete Virtual Strips, Detect Physical Devices.
    - [x] **Hardware Routing**: Mics to Inputs, Outputs to Speakers.
    - [x] **Volume/Mute Control**: 
        - [x] Syncs UI, Sink, and Hardware Monitor.
        - [x] Correctly handles Physical Inputs (Microphones) vs Outputs.
    - [x] **App Routing**: Apps strictly follow their assigned strip.
    - [x] **Default Sink**: Checkbox to define the system-wide default output.
    - [x] **UI Layout**: Dynamic resizing, no empty spaces, sorted device lists.
    - [x] **VU Meters**: 
        - [x] **Virtual Strips**: Working.
        - [x] **Physical Inputs**: Working.
        - [x] **Physical Outputs**: Working (via Monitor Discovery & Thread Safety).

    ## 6. Known Considerations & Issues
    - **ALSA Latency**: On system load, opening a stream might take seconds. The `MeteringEngine` hides this latency via threading.
    - **Environment Variables**: Modifying `os.environ` in a threaded environment requires strict locking (handled by `creation_lock`).

    ## 7. Roadmap (Next Steps)
    1.  **Visual Polish**: Improve Routing buttons styling (make them more distinct/modern).
    2.  **Refactoring**: Clean up redundant code in `audio_engine.py` (e.g., consolidate link logic).
    3.  **Packaging**: Prepare for standalone distribution (PyInstaller).