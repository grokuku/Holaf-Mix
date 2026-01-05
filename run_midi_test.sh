#!/bin/bash

# Get the directory where the script is located
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )"

# Activate the virtual environment
source "$SCRIPT_DIR/venv/bin/activate"

# Run the Python MIDI test script
python "$SCRIPT_DIR/midi_test.py"

# Deactivate the virtual environment (optional, but good practice)
deactivate
