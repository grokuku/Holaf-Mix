//! `holaf-fxcfg` — print the `libpipewire-module-filter-chain` arguments that
//! `holafd` builds for one strip.
//!
//! This is the missing piece of the A/B bench: to compare "chain hosted by a
//! dedicated `pw-cli`" (what the Python engine does) against "chain hosted by
//! holafd", both sides must load the **exact same** SPA-JSON.  Rather than
//! duplicating the builder in Python, the bench asks this binary — which goes
//! through the very same [`holaf_pw::engine::ChainSpec`] the daemon uses.
//!
//! Usage:
//!
//! ```text
//! holaf-fxcfg <uid> [--label <text>] [--kind output|input] [--fx eq,tube]
//! holaf-fxcfg --help
//! ```
//!
//! Output: the module args on stdout, one line, nothing else (so the bench can
//! pipe it straight into `pw-cli load-module libpipewire-module-filter-chain`).
//!
//! `--fx` selects the active effects (comma separated, canonical chain order);
//! the default is `eq` — the configuration users report the chopping with.

use holaf_core::model::{EffectKey, Strip, StripType};
use holaf_pw::engine::ChainSpec;

const USAGE: &str = "\
holaf-fxcfg — print the filter-chain module args holafd would load for a strip

USAGE:
    holaf-fxcfg <uid> [OPTIONS]

OPTIONS:
    --label <text>      Strip label (default: the uid)
    --kind <kind>       'output' (default) or 'input'
    --fx <list>         Comma-separated active effects, in canonical chain order:
                        gate,noise_cancel,eq,tube,compressor (default: eq)
    --help              Show this help and exit
";

fn parse_fx(list: &str) -> Result<Vec<EffectKey>, String> {
    let mut out = Vec::new();
    for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let key = match item {
            "gate" => EffectKey::Gate,
            "noise_cancel" => EffectKey::NoiseCancel,
            "eq" => EffectKey::Eq,
            "tube" => EffectKey::Tube,
            "compressor" => EffectKey::Compressor,
            other => return Err(format!("unknown effect '{other}'")),
        };
        out.push(key);
    }
    Ok(out)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }

    let mut uid = String::new();
    let mut label: Option<String> = None;
    let mut kind = StripType::Output;
    let mut fx = vec![EffectKey::Eq];

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--label" => match iter.next() {
                Some(v) => label = Some(v.clone()),
                None => {
                    eprintln!("holaf-fxcfg: --label needs a value");
                    std::process::exit(2);
                }
            },
            "--kind" => match iter.next().map(String::as_str) {
                Some("output") => kind = StripType::Output,
                Some("input") => kind = StripType::Input,
                other => {
                    eprintln!("holaf-fxcfg: bad --kind {other:?}");
                    std::process::exit(2);
                }
            },
            "--fx" => match iter.next() {
                Some(v) => match parse_fx(v) {
                    Ok(keys) => fx = keys,
                    Err(e) => {
                        eprintln!("holaf-fxcfg: {e}");
                        std::process::exit(2);
                    }
                },
                None => {
                    eprintln!("holaf-fxcfg: --fx needs a value");
                    std::process::exit(2);
                }
            },
            other if other.starts_with("--") => {
                eprintln!("holaf-fxcfg: unknown option {other}");
                std::process::exit(2);
            }
            other => uid = other.to_string(),
        }
    }
    if uid.is_empty() {
        eprintln!("holaf-fxcfg: missing <uid>");
        std::process::exit(2);
    }

    let mut strip = Strip::with_uid(uid.clone(), label.unwrap_or_else(|| uid.clone()), kind);
    for key in fx {
        if let Some(state) = strip.effects.get_mut(&key) {
            state.active = true;
        }
    }

    match ChainSpec::for_strip(&strip) {
        Ok(spec) => {
            println!("{}", spec.config);
        }
        Err(e) => {
            eprintln!("holaf-fxcfg: {}", e.message);
            std::process::exit(1);
        }
    }
}
