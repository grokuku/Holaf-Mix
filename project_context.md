## 1. Project Overview
    - **Goal**: Linux audio mixer (Voicemeeter-like) using PipeWire and PySide6.
    - **Current Phase**: **STABILIZATION & REFINEMENT**.
    - **Status**: 
        - Functional Audio & MIDI Engine.
        - Reliable App Routing & Hardware Volume control.
        - UI Layout fixed & Cleaned (Pollution removed from dropdowns).
        - **PARTIAL**: VU Meters work for Inputs/Virtual strips, but are unreliable for Physical Outputs.

    ## 2. Architecture (Hybrid MVC)
    - **Model**: `Strip` class. Single source of truth.
    - **View (UI)**: `MainWindow` and `StripWidget`.
    - **Controller (Backend)**: 
        - `AudioEngine`: Manages PipeWire Graph & lazy-loads metering.
        - `MidiEngine`: Handles Hardware MIDI events.
        - `MeteringEngine`: **NEW (Async)**. Uses `sounddevice` in separate threads with automatic retry logic to avoid blocking the UI.
    - **Data Flow**: 
        - **UI Input**: Slider Move -> Update Model -> Timer (10Hz) -> `AudioEngine` -> PipeWire.
        - **Meters**: PipeWire -> `sounddevice` (ALSA/Pulse plugin) -> Threaded Callback -> UI Timer (25FPS).

    ## 3. Project File Structure (Map & Responsibilities)

    ```text
    Holaf_Mix/
    ├── main.py                     # [ENTRY POINT] Bootstrapper.
    ├── pipewire_utils.py           # [LOW-LEVEL] Wrapper for `pw-cli`, `pw-dump`, `pactl`. Now supports filtering internal nodes.
    ├── config.json                 # [PERSISTENCE] Stores Strips state.
    ├── project_context.md          # [MEMORY] Project state and rules.
    ├── requirements.txt            # [DEPENDENCIES] PySide6, mido, python-rtmidi, sounddevice, numpy.
    │
    ├── src/
    │   ├── backend/
    │   │   ├── audio_engine.py     # [CONTROLLER] Manages Nodes. Retry logic for meters. Robust linking.
    │   │   ├── midi_engine.py      # [CONTROLLER] MIDI Listener.
    │   │   └── metering.py         # [CONTROLLER] Async Sounddevice engine (Threaded).
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
    - **Node Creation**: Mixed strategy (`pw-cli` preferred, `pactl` fallback).
    - **Linking**: Robust "Force Unlink" logic added to fix desynchronization issues (e.g., when toggling routes).
    - **Zombie Cleanup**: Uses `include_internal=True` to strictly identify and remove old Holaf nodes.

    ### Metering Logic (New Stable Arch)
    - **Library**: `sounddevice` (PortAudio wrapper) targeting the `pulse` ALSA device.
    - **Targeting**: Uses `os.environ["PULSE_SOURCE"]` to target specific monitors per stream.
    - **Concurrency**: `start_monitoring` spawns a non-blocking thread. If ALSA timeouts, it fails silently and adds the strip to a `pending_retries` queue.
    - **Retry**: `AudioEngine` triggers a retry every ~2 seconds for meters that failed to start immediately (Lazy Loading).

    ### UX / Filtering
    - `pipewire_utils.get_audio_nodes` supports an `include_internal` flag.
    - **UI Mode**: Hides `Holaf_Strip_*`, `Monitor of...`, and `(null)` devices to keep dropdowns clean.
    - **Backend Mode**: Sees everything to manage the graph correctly.

    ## 5. Features Status
    - [x] **Core Audio**: Create/Delete Virtual Strips, Detect Physical Devices.
    - [x] **Hardware Routing**: Mics to Inputs, Outputs to Speakers.
    - [x] **Volume Control**: Syncs UI, Sink, and Hardware Monitor.
    - [x] **App Routing**: Apps strictly follow their assigned strip.
    - [x] **Default Sink**: Checkbox to define the system-wide default output.
    - [x] **UI Layout**: Dynamic resizing, no empty spaces, sorted device lists.
    - [~] **VU Meters**: 
        - [x] **Virtual Strips**: Working (Async).
        - [x] **Physical Inputs**: Working (Mic).
        - [ ] **Physical Outputs**: **BROKEN**. The application guesses the monitor name (`{sink_name}.monitor`), which often fails for hardware sinks (e.g. `alsa_output...`).

    ## 6. Known Considerations & Issues
    - **Physical Output Metering**: Requires mapping a Sink ID to its Source ID via `pactl list sources` to get the *exact* monitor name. Simple string concatenation is unreliable.
    - **ALSA Timeouts**: If the system is under load, opening a stream might take seconds. The current Threaded implementation hides this latency from the user, which is good.

    ## 7. Roadmap (Next Steps)
    1.  **Fix Physical Output Metering**: Implement a safe lookup method to find the Monitor Source ID of a hardware Sink without destabilizing the graph or causing regressions.
    2.  **Visual Polish**: Improve Routing buttons styling.
    3.  **Refactoring**: Clean up `audio_engine.py` redundancy once stable.