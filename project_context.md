# PROJECT CONTEXT - HOLAF MIX

    ## 1. Project Overview
    - **Goal**: Linux audio mixer (Voicemeeter-like) using PipeWire and PySide6.
    - **Target Platform**: CachyOS / Arch Linux.
    - **Current Phase**: **STABILIZATION & TUNING**.
    - **Status**: 
        - **Core Audio**: Functional (Virtual Strips, Physical Routing, Exclusive Source Logic).
        - **UI**: Functional (VU Meters, Faders, Buttons, Right-Click FX Settings).
        - **FX Engine**: **FUNCTIONAL**. Node creation works, parameters are dynamic, specific effects (RNNoise) confirmed working.
        - **Discovery**: `pw-dump` via `pipewire_utils.py` (Stable).

    ## 2. Architecture (Hybrid MVC)
    - **Model**: `Strip` class.
        - **Update**: Effects are now stored as dictionaries `{'active': bool, 'params': {key: value}}` instead of simple booleans.
        - **Migration**: `from_dict` automatically converts old config files.
    - **Controller**: `AudioEngine` (Backend).
        - **FX Hosting**: Uses a persistent `subprocess.Popen(['pw-cli'], stdin=PIPE)`.
        - **Routing**: **Exclusive Source Strategy**. When switching between Raw and FX sources, the engine explicitly disconnects (`_unlink_nodes`) the unused source to prevent signal doubling (Anti-Gate issue).
        - **Param Parsing**: Converts Model parameters into SPA-JSON for `filter-chain`.
    - **View**: `MainWindow` -> `StripWidget`.
        - **New**: `EffectSettingsDialog` (Dynamic slider generation based on effect type).

    ## 3. Project File Structure
    ```text
    Holaf_Mix/
    ├── main.py                     # [ENTRY POINT]
    ├── config.json                 # [PERSISTENCE]
    ├── project_context.md          # [MEMORY]
    ├── requirements.txt            # [DEPENDENCIES]
    │
    ├── src/
    │   ├── backend/
    │   │   ├── audio_engine.py     # [CORE] Routing Logic, FX Graph Generation (SPA-JSON).
    │   │   ├── midi_engine.py      # [MIDI] Hardware integration.
    │   │   ├── metering.py         # [METERING] Threaded peak detection.
    │   │   └── pipewire_utils.py   # [LOW-LEVEL] `pw-dump` wrapper & regex parsing.
    │   │
    │   ├── config/
    │   │   └── settings.py         # [IO]
    │   │
    │   ├── models/
    │   │   └── strip_model.py      # [MODEL] Includes new Effect Parameter structure.
    │   │
    │   └── ui/
    │       ├── main_window.py      # [VIEW] Main container.
    │       ├── dialogs/
    │       │   └── effect_settings_dialog.py # [VIEW] Dynamic sliders for FX parameters.
    │       └── widgets/
    │           └── strip_widget.py # [VIEW] Strip controls & Right-click context menu.
    ```

    ## 4. Technical Implementation Details

    ### Native Effect Engine
    - **Method**: "Persistent Host" (pw-cli).
    - **Format**: SPA-JSON generated dynamically from Python dictionaries.
    - **Graph**: Stereo (Dual Mono). Creates `Node_L` and `Node_R` for each LADSPA plugin.
    - **Parameter Handling**:
        - **Gate**: Threshold, Attack, Release, Hold.
        - **Compressor**: Threshold, Ratio, Attack, Release, Makeup Gain.
        - **EQ**: 15-Band Graphic EQ (mbeq).
        - **RNNoise**: VAD Threshold (Placeholder).

    ### Audio Routing Strategy (Refined)
    - **Virtual Strips**: `module-null-sink`.
    - **Exclusive Linking**: 
        - Before connecting `FX_Output -> Bus`, the engine **MUST** disconnect `Physical_Source -> Bus`.
        - Before connecting `Physical_Source -> Bus`, the engine **MUST** disconnect `FX_Output -> Bus`.
    - **Mono**: 
        - Uses `_unlink_nodes` to clear Stereo links before applying Mono logic (`L->L, L->R`).
        - Robust Regex `r"(?:[\d]+:\s*)?(?:[\|\-><\s]+)?(" + re.escape(node_name) + r":\S+)"` handles `pw-link` tree output.

    ## 5. Resolved Issues (History)

    ### A. The "Anti-Gate" (RESOLVED)
    - **Symptom**: Dry signal persisted when Gate was ON.
    - **Fix**: Implemented `_unlink_nodes` in `_create_link` to enforce exclusive signal path (Dry OR Wet, never both).

    ### B. Mono Behavior (RESOLVED)
    - **Symptom**: "Left Only" sound when Mono enabled on FX.
    - **Fix**: Improved `_auto_link_ports` regex to detect non-standard port names and force cleanup of old links.

    ### C. Link Detection False Negatives (RESOLVED)
    - **Symptom**: Engine retrying links endlessly.
    - **Fix**: Added support for French locale error message ("Le fichier existe") in `_pw_link`.

    ### D. Effect Logic Crash (RESOLVED)
    - **Symptom**: All effects trying to load simultaneously even if disabled.
    - **Fix**: Corrected Python "Truthiness" check. `if strip.effects['gate']` became `if strip.effects['gate']['active']`.

    ## 6. Next Steps / TODO
    - **Parameter Tuning**: Default values for EQ and Compressor need testing to avoid saturation (Current defaults set to safe values: EQ Flat, Gain 0dB).
    - **Real-time Updates**: Currently, changing a parameter requires reloading the FX chain (Logic pending in `update_fx_params`).
    - **Visual Feedback**: Add Gain Reduction metering for Compressor/Gate (Advanced).