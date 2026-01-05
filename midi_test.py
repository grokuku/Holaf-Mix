import mido
import time

def run_midi_test():
    """
    Lists available MIDI input ports and listens for messages from a selected port.
    """
    print("Listing available MIDI input ports:")
    try:
        input_ports = mido.get_input_names()
        if not input_ports:
            print("\nError: No MIDI input ports found.")
            print("Please ensure your Akai Midimix is connected and recognized by the system.")
            return

        for i, port_name in enumerate(input_ports):
            print(f"  {i}: {port_name}")

        # --- Port Selection ---
        port_to_open = None
        # Try to find a port that looks like Akai Midimix automatically
        for name in input_ports:
            if "midi mix" in name.lower(): # More flexible search
                print(f"\nFound 'MIDI Mix' port, attempting to open: {name}")
                port_to_open = name
                break
        
        # If no auto-match, fall back to the first port
        if not port_to_open:
            port_to_open = input_ports[0]
            print(f"\nCould not automatically find 'Midimix'. Opening first available port: {port_to_open}")
            print("If this is not the correct device, you may need to modify the script.")

        # --- Listen for Messages ---
        with mido.open_input(port_to_open) as inport:
            print(f"\nSuccessfully opened '{inport.name}'. Listening for MIDI messages...")
            print("Move a fader or press a button on your controller. Press Ctrl+C to stop.")
            while True:
                for msg in inport.iter_pending():
                    print(msg)
                time.sleep(0.01)

    except Exception as e:
        print(f"\nAn error occurred: {e}")
        print("Please ensure the port is not already in use and that you have the correct permissions.")
    except KeyboardInterrupt:
        print("\nStopping MIDI listener.")

if __name__ == "__main__":
    run_midi_test()
