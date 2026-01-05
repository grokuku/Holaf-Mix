from src.config import settings
from src.models.strip_model import Strip, StripType

def run_test():
    print("--- TEST 1: Initial Load (Should create defaults) ---")
    strips = settings.load_config()
    for s in strips:
        print(f"Loaded: {s} - UID: {s.uid}")

    print("\n--- TEST 2: Modification & Addition ---")
    # Simulate moving a fader on the first strip
    if strips:
        strips[0].volume = 0.85
        print(f"Changed volume of {strips[0].label} to 0.85")

    # Simulate clicking "Add Input"
    new_strip = Strip("Spotify", StripType.INPUT)
    strips.append(new_strip)
    print(f"Added new strip: {new_strip}")

    print("\n--- TEST 3: Saving ---")
    settings.save_config(strips)

    print("\n--- TEST 4: Reloading (Verify persistence) ---")
    # Reload from disk to prove it was saved
    reloaded_strips = settings.load_config()
    for s in reloaded_strips:
        print(f"Reloaded: {s}")

    # Check if our changes stuck
    if len(reloaded_strips) == 3:
        print("\nSUCCESS: We have 3 strips (2 defaults + Spotify).")
    else:
        print("\nFAILURE: Strip count mismatch.")

    if reloaded_strips[0].volume == 0.85:
        print("SUCCESS: Volume change persisted.")
    else:
        print("FAILURE: Volume did not persist.")

if __name__ == "__main__":
    run_test()