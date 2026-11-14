// The port itself only exists with the `midi` feature (`midir`).
#![cfg(feature = "midi")]

//! Live harness for the **device side** of the MIDI port: `MidiPort::open`,
//! `ports()`, the reconnection state machine against a real ALSA sequencer.
//!
//! # Why every test here may skip
//!
//! `midir` talks to the ALSA **sequencer** (`/dev/snd/seq`).  A container
//! without `/dev/snd/seq` — and without `CAP_SYS_MODULE` to load `snd-seq` — has
//! no sequencer at all, and *no* virtual port can be created either (`midir`'s
//! `create_virtual`, `snd-virmidi` and `snd-aloop` all need the kernel side).
//! That is the case of the container this work was done in, so each test asks
//! [`sequencer_available`] first and returns early with an explicit
//! `eprintln!` when the answer is no.  On a desktop the same tests run for real:
//!
//! ```bash
//! cd rust && cargo test -p holaf-ui --test midi_live -- --nocapture
//! ```
//!
//! # What it proves when it can run
//!
//! 1. `ports()` lists inputs and outputs by exact name (what `--midi-list`
//!    prints and what the port picker shows);
//! 2. opening a name that does not exist fails with a message naming it — the
//!    "my controller is unplugged" path, which must be a status line, not a
//!    crash;
//! 3. a **virtual** port created by this test (midir's `VirtualOutput`, i.e. the
//!    same trick as `snd-virmidi`, without the kernel module) is opened through
//!    `MidiPort`, a CC 19 / value 100 is written into it, and `drain()` returns
//!    exactly that [`MidiMessage::Control`] — the whole device path, minus the
//!    hardware;
//! 4. the reconnection policy then reflects a real port: `MidiRuntime` opens it,
//!    is not due again while it is open, and retries after `on_lost`.

use holaf_ui::midi::{MidiMessage, MidiPort, MidiRuntime, MidiVirtualSource, RETRY_INTERVAL};

/// Is there an ALSA sequencer to talk to?  `MidiPort::ports()` is the only honest
/// answer: it is the same call the UI makes at startup.
fn sequencer_available() -> bool {
    match MidiPort::ports() {
        Ok(_) => true,
        Err(err) => {
            eprintln!("SKIP: no ALSA sequencer on this machine ({err})");
            eprintln!("      (/dev/snd/seq missing: load `snd-seq`, or run outside the container)");
            false
        }
    }
}

#[test]
fn the_port_list_is_readable_and_names_are_exact() {
    if !sequencer_available() {
        return;
    }
    let ports = MidiPort::ports().expect("the sequencer answers");
    // A desktop always exposes at least `Midi Through`; a bare container with a
    // sequencer may expose nothing at all, which is not a failure of this crate.
    println!("inputs:  {:?}", ports.inputs);
    println!("outputs: {:?}", ports.outputs);
    for name in ports.inputs.iter().chain(ports.outputs.iter()) {
        assert!(!name.is_empty(), "a port name is never empty");
        assert_eq!(name.trim(), name, "names are returned verbatim: {name:?}");
    }
    // The listing is what the picker shows, so it must not include duplicates of
    // the same direction.
    let mut inputs = ports.inputs.clone();
    inputs.sort();
    inputs.dedup();
    assert_eq!(inputs.len(), ports.inputs.len(), "{:?}", ports.inputs);
}

#[test]
fn opening_a_port_that_does_not_exist_is_an_error_not_a_panic() {
    if !sequencer_available() {
        return;
    }
    let error = MidiPort::open("holaf-ui:no-such-controller 255:0")
        .expect_err("a missing port cannot be opened");
    println!("open error: {error}");
    assert!(
        error.contains("holaf-ui") || error.contains("no MIDI input"),
        "the message names the port or says why: {error}"
    );
    // And the runtime keeps the wish, so the retry loop will pick it up when the
    // controller comes back.
    let mut runtime = MidiRuntime::new(Some("holaf-ui:no-such-controller 255:0".to_string()));
    let now = std::time::Instant::now();
    assert!(runtime.attempt(now).is_some());
    runtime.on_failed(error);
    assert!(runtime.status_line().unwrap().contains("retrying"));
}

/// The full path with a **virtual** port: `MidiVirtualSource` creates an output
/// that other clients see as an input, `MidiPort::open` connects to it, and a CC
/// written into it comes out of `drain()`.
#[test]
fn a_virtual_controller_drives_the_port() {
    if !sequencer_available() {
        return;
    }
    let short_name = format!("holaf-ui-test-{}", std::process::id());
    let mut source = match MidiVirtualSource::create(&short_name) {
        Ok(source) => source,
        // A sequencer that refuses virtual ports (a locked-down container) is not
        // a bug in this crate.
        Err(err) => {
            eprintln!("SKIP: cannot create a virtual port ({err})");
            return;
        }
    };
    let name = source.alsa_name().to_string();
    println!("virtual port visible as {name:?}");

    let port = MidiPort::open(&name).expect("the virtual port can be opened");
    assert_eq!(port.name(), name);
    // Let the ALSA subscription settle, then play a MIDI Mix fader.
    std::thread::sleep(std::time::Duration::from_millis(50));
    source
        .send(MidiMessage::Control {
            channel: 0,
            control: 19,
            value: 100,
        })
        .expect("the virtual write");

    let mut received = Vec::new();
    for _ in 0..40 {
        received.extend(
            port.drain()
                .into_iter()
                .filter_map(|event| match event {
                    holaf_ui::midi::MidiEvent::Message(message) => Some(message),
                    _ => None,
                }),
        );
        if !received.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(
        received.contains(&MidiMessage::Control {
            channel: 0,
            control: 19,
            value: 100
        }),
        "CC 19 / 100 must come out of the port, got {received:?}"
    );
    // Clock and sysex are filtered out by `MidiMessage::parse`: a controller that
    // sends them must not drown the mapping table.
    source.send_bytes(&[0xF8]).expect("a clock message");
    source.send_bytes(&[0xF0, 0x7E, 0x00, 0xF7]).expect("a sysex");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        port.drain().is_empty(),
        "clock/sysex must not surface as messages"
    );
    // The LED path accepts a write (the virtual port has an output side).
    port.send(MidiMessage::Note {
        channel: 0,
        note: 24,
        velocity: 127,
    });

    // The runtime reflects the real port.
    let start = std::time::Instant::now();
    let mut runtime = MidiRuntime::new(Some(name.clone()));
    assert_eq!(runtime.attempt(start), Some(name.clone()));
    runtime.on_opened(&name);
    assert!(runtime.is_open());
    assert_eq!(runtime.attempt(start + RETRY_INTERVAL), None, "still open");
    runtime.on_lost("unplugged");
    assert!(runtime.attempt(start + RETRY_INTERVAL * 2).is_some());
}
