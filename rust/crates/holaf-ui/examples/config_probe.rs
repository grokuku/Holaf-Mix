//! `config_probe` — prove what the Rust UI actually loads from a `config.json`,
//! and what a save does to that file.
//!
//! Three modes, so each question is answered by one command and nothing is
//! implicit:
//!
//! ```text
//! cargo run -q -p holaf-ui --no-default-features --example config_probe -- <file>
//!     # 1. dump: every field of every strip exactly as `UiConfig` sees it
//!
//! cargo run -q -p holaf-ui --no-default-features --example config_probe -- <file> save-noop
//!     # 2. rewrite the file without changing a single field
//!
//! cargo run -q -p holaf-ui --no-default-features --example config_probe -- <file> save-volume
//!     # 3. rewrite the file after changing ONE field (strip #0 volume)
//! ```
//!
//! The point of (2) is the diff: a no-op save must not lose a key.  The point of
//! (3) is that a real edit touches only the edited value.
//!
//! Always point it at a **copy** of the real config — (2) and (3) overwrite the
//! file they are given:
//!
//! ```text
//! cp config.json /tmp/real.json
//! ```
//!
//! Volumes are printed with 17 decimals on purpose: `Strip::volume` is an `f32`
//! (the engine's type), so a value persisted as `f64` by the Qt UI is rounded on
//! load and re-printed differently.  Printing the full tail makes that visible
//! instead of hiding it behind a percentage.

use std::path::PathBuf;

use holaf_core::model::Strip;
use holaf_ui::config::UiConfig;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: config_probe <config.json> [dump|save-noop|save-volume]");
        std::process::exit(2);
    };
    let mode = args.next().unwrap_or_else(|| "dump".to_string());
    let path = PathBuf::from(path);

    let mut config = match UiConfig::open(&path) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("cannot load {}: {err}", path.display());
            std::process::exit(1);
        }
    };

    if mode == "dump" {
        dump(&config);
        return;
    }

    let mut strips = config.strips();
    match mode.as_str() {
        "save-noop" => {}
        "save-volume" => {
            let Some(first) = strips.first_mut() else {
                eprintln!("no strips to edit");
                std::process::exit(1);
            };
            // A visible, unambiguous change: 0.0 → 0.25 ("25 %" in the UI).
            first.volume = 0.25;
            println!("edited: strip #0 volume -> {}", first.volume);
        }
        other => {
            eprintln!("unknown mode {other:?} (dump|save-noop|save-volume)");
            std::process::exit(2);
        }
    }

    let geometry = config.geometry();
    match config.save_now(&strips, geometry) {
        Ok(()) => println!("saved {} ({} strips)", path.display(), strips.len()),
        Err(err) => {
            eprintln!("cannot save {}: {err}", path.display());
            std::process::exit(1);
        }
    }
}

fn dump(config: &UiConfig) {
    println!("file              : {}", config.path().display());
    println!("loaded            : {}", config.loaded());
    println!("theme             : {:?}", config.theme());
    match config.geometry() {
        Some(geometry) => println!(
            "geometry          : width={:?} height={}",
            geometry.width, geometry.height
        ),
        None => println!("geometry          : none"),
    }
    let strips = config.strips();
    println!("strips            : {}", strips.len());
    let assigned = config.assigned_apps();
    let midi = config.midi_bindings();
    println!("assigned_apps     : {} strip(s)", assigned.len());
    println!("midi bindings     : {} strip(s)", midi.len());
    for (index, strip) in strips.iter().enumerate() {
        print_strip(index, strip, config, &assigned, &midi);
    }
}

fn print_strip(
    index: usize,
    strip: &Strip,
    _config: &UiConfig,
    assigned: &std::collections::HashMap<String, Vec<String>>,
    midi: &std::collections::HashMap<
        String,
        Vec<(holaf_ui::midi::MidiTarget, holaf_ui::midi::Binding)>,
    >,
) {
    println!("--- strip #{index}");
    println!("  uid             : {}", strip.uid);
    println!("  label           : {}", strip.label);
    println!("  kind            : {}", strip.kind);
    println!("  volume          : {:.17}", strip.volume);
    println!("  mute            : {}", strip.mute);
    println!("  is_mono         : {}", strip.is_mono);
    println!("  is_default      : {}", strip.is_default);
    println!(
        "  device_name     : {}",
        match &strip.device_name {
            Some(name) => name.as_str(),
            None => "null",
        }
    );
    println!("  routes ({})     : {:?}", strip.routes.len(), strip.routes);
    println!(
        "  assigned_apps   : {:?}",
        assigned.get(&strip.uid).map(Vec::as_slice).unwrap_or(&[])
    );
    println!(
        "  midi bindings   : {}",
        midi.get(&strip.uid).map(Vec::len).unwrap_or(0)
    );
    println!("  effects ({})    :", strip.effects.len());
    for (key, effect) in &strip.effects {
        println!(
            "    {} active={} params={}",
            key.as_str(),
            effect.active,
            effect.params.len()
        );
        for (port, value) in &effect.params {
            println!("      {port} = {value}");
        }
    }
}
