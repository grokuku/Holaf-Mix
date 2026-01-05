# Holaf-Mix Project

## 1. Project Goal

The primary objective of this project is to create a user-friendly audio routing and mixing application for Linux, inspired by Voicemeeter. The application allows a user to control system audio sources (applications, microphones) and sinks using a MIDI controller, specifically the Akai Midimix.

## 2. Technical Approach

- **Audio Engine**: The application leverages the modern **PipeWire** audio server on Linux for all audio management tasks (detecting sources/sinks, controlling volume, routing). The application does not implement its own audio logic but rather sends commands to PipeWire.
- **Control Application**: A **Python** application serves as the bridge between the MIDI controller and PipeWire.
- **User Interface**: A native desktop GUI built with **PySide6 (the official Python bindings for the Qt framework)** provides an interface for configuration and monitoring. This choice was made to ensure the application integrates well with the user's KDE Plasma desktop environment.
- **MIDI Communication**: The `mido` and `python-rtmidi` libraries are used for MIDI communication.
- **Configuration**: Mappings between MIDI controls and PipeWire nodes are stored in a `config.json` file, which allows for dynamic and persistent user configurations.

## 3. Current Status (As of Jan 05, 2026)

The project has successfully moved from initial proof-of-concept to a functional core application. The following has been achieved:

- **Python Environment**: A virtual environment (`venv`) is set up with all necessary dependencies.
- **MIDI Communication**: Communication with MIDI devices is functional. The application can list devices and listen for messages.
- **PipeWire Integration**: A utility module, `pipewire_utils.py`, has been created to interface with PipeWire. It can:
  - List all relevant audio nodes (sinks, sources, and application streams) by parsing the output of `pw-dump`.
  - Programmatically set the volume and mute state of any given node using `pw-cli`.
- **GUI Application**:
  - A basic GUI window using PySide6 has been created (`main.py`).
  - The UI dynamically lists available MIDI input devices and PipeWire audio nodes.
- **Dynamic MIDI Mapping**:
  - A "MIDI Learn" feature has been implemented. The user can click a "Detect" button for a specific control (volume or mute) and the application will listen for the next MIDI message and save the mapping.
  - Mappings are correctly saved to and loaded from `config.json`.
- **Live Control**:
  - The application listens for MIDI messages in a background thread.
  - Mapped `control_change` messages (from faders/knobs) correctly control the volume of the corresponding PipeWire nodes in real-time.
  - Mapped `note_on` or `control_change` messages (from buttons) correctly toggle the mute state of the corresponding nodes.

## 4. Project Structure

- `main.py`: The main entry point for the application. Contains the PySide6 UI code, the MIDI listener thread, and the main application logic that ties everything together.
- `pipewire_utils.py`: A module for all interactions with PipeWire. It contains functions to get audio nodes and their properties, and to set volume/mute.
- `config.py`: A module to handle loading and saving the application's configuration from/to `config.json`.
- `config.json`: The configuration file where MIDI device selection and mappings are stored.
- `requirements.txt`: A list of all Python dependencies required for the project.
- `run_holaf_mix.sh`: An executable shell script to easily launch the main application.
- `midi_test.py` / `run_midi_test.sh`: Initial test scripts used to validate MIDI communication. Can be used for debugging.

## 5. How to Set Up and Run

To get the project running on a new machine (assuming Linux with PipeWire):

1.  **Clone/copy the project files.**
2.  **Navigate to the project directory:**
    ```bash
    cd /path/to/Holaf_Mix
    ```
3.  **Create a Python virtual environment:**
    ```bash
    python3 -m venv venv
    ```
4.  **Activate the virtual environment:**
    ```bash
    source venv/bin/activate
    ```
5.  **Install the required dependencies:**
    ```bash
    pip install -r requirements.txt
    ```
6.  **Run the application:**
    ```bash
    ./run_holaf_mix.sh
    ```

## 6. Next Steps for the Project

The core functionality is in place, but there are several areas for improvement and new features:

- **UI/UX Refinement**:
  - The current UI is functional but very basic. It could be redesigned to be more intuitive and visually appealing.
  - Improve the visual feedback during and after the "MIDI learn" process.
- **Robustness**:
  - Improve error handling (e.g., when a MIDI device is disconnected).
  - The mute toggle logic relies on reading the state before toggling. While functional, this could be optimized if PipeWire offers a direct "toggle" command (which is unlikely, so the current approach is standard).
- **New Features**:
  - **Master Volume**: Implement a master volume control that maps a fader to the main system output sink.
  - **Routing**: Add functionality to change the routing of audio streams (e.g., link an application's output to a different audio device). This would involve using `pw-cli create-link` and `pw-cli destroy <link_id>`.
  - **More Mappings**: Allow mapping of other MIDI controls (e.g., knobs) and other PipeWire parameters.

This document should provide a solid foundation for the next developer or LLM to continue the project.
