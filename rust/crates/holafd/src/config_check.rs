//! `holafd --check-config <path>` support (rewrite spec §8).
//!
//! Loads a `config.json`, runs every strip through the `holaf-core`
//! migrations and prints a human-readable graph plan **without** touching
//! PipeWire.  This doubles as a migration smoke-test for the real config.

use std::io::Write;
use std::path::{Path, PathBuf};

use holaf_core::config::{load_config, ConfigError};
use holaf_core::migration::strip_from_dict;
use holaf_core::model::{EffectKey, Strip};
use serde_json::Value;
use thiserror::Error;

/// Errors surfaced by a config check run.
#[derive(Debug, Error)]
pub enum CheckError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("config file '{0}' has no 'strips' array")]
    MissingStrips(PathBuf),
}

/// The migrated plan extracted from a config file.
#[derive(Debug)]
pub struct ConfigPlan {
    pub path: PathBuf,
    /// Fully-migrated strips (all five effects present, ports renamed,
    /// defaults filled).
    pub strips: Vec<Strip>,
}

/// Load `path` and migrate every strip entry.
pub fn load_plan(path: &Path) -> Result<ConfigPlan, CheckError> {
    let value = load_config(path)?;
    let strips_value = value.get("strips").ok_or_else(|| {
        CheckError::MissingStrips(path.to_path_buf())
    })?;
    let raw_strips: &[Value] = strips_value
        .as_array()
        .ok_or_else(|| CheckError::MissingStrips(path.to_path_buf()))?;
    let strips: Vec<Strip> = raw_strips.iter().map(strip_from_dict).collect();
    Ok(ConfigPlan {
        path: path.to_path_buf(),
        strips,
    })
}

/// Render the graph plan for `plan` into `out` (stdout for the CLI, a buffer
/// for tests).
pub fn print_plan(plan: &ConfigPlan, out: &mut dyn Write) -> std::io::Result<()> {
    let mut total_active = 0usize;
    for strip in &plan.strips {
        for effect in applicable_effects(strip) {
            if strip
                .effects
                .get(&effect)
                .map(|state| state.active)
                .unwrap_or(false)
            {
                total_active += 1;
            }
        }
    }
    writeln!(out, "Holaf-Mix config check (no PipeWire connection)")?;
    writeln!(out, "Config file: {}", plan.path.display())?;
    writeln!(out, "Strips: {}", plan.strips.len())?;
    for (i, strip) in plan.strips.iter().enumerate() {
        writeln!(
            out,
            "  {}. [{}] \"{}\" (uid {})",
            i + 1,
            strip.kind,
            strip.label,
            strip.uid
        )?;
        writeln!(
            out,
            "     volume={:.2} mute={} mono={} routes=[{}] default={} device={}",
            strip.volume,
            strip.mute,
            strip.is_mono,
            strip.routes.join(","),
            strip.is_default,
            strip
                .device_name
                .as_deref()
                .map(|d| d.to_string())
                .unwrap_or_else(|| "-".into()),
        )?;
        let applicable = applicable_effects(strip);
        writeln!(
            out,
            "     effects ({} applicable):",
            applicable.len()
        )?;
        for effect in applicable {
            match strip.effects.get(&effect) {
                Some(state) if state.active => {
                    let params = state
                        .params
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    writeln!(out, "       {effect}: active {params}")?;
                }
                _ => writeln!(out, "       {effect}: inactive")?,
            }
        }
    }
    writeln!(out, "Active effects: {total_active}")?;
    Ok(())
}

/// Effects the engine actually builds for this strip (spec §5: input = 5,
/// output = eq/tube/compressor only).
fn applicable_effects(strip: &Strip) -> Vec<EffectKey> {
    EffectKey::for_strip(strip.kind)
        .into_iter()
        .filter(|k| strip.effects.contains_key(k))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch_config(name: &str, body: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "holafd-check-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn load_plan_migrates_real_world_config() {
        let path = scratch_config(
            "migrate",
            r#"{
                "window": {},
                "strips": [
                    {
                        "uid": "u1", "label": "Casque", "kind": "output",
                        "volume": 0.0, "mute": true,
                        "device_name": "alsa_output.usb-abc.analog-stereo",
                        "effects": {
                            "gate": { "active": false, "params": { "Release (ms)": 200.0 } },
                            "eq": { "active": false, "params": { "50Hz": 0.0 } },
                            "compressor": { "active": true, "params": { "Threshold (dB)": -12.0 } }
                        }
                    },
                    {
                        "uid": "u2", "label": "Micro", "kind": "input",
                        "effects": {
                            "gate": true,
                            "noise_cancel": { "active": false, "params": { "Model": 0.0 } }
                        }
                    }
                ]
            }"#,
        );
        let plan = load_plan(&path).unwrap();
        assert_eq!(plan.strips.len(), 2);

        let casque = &plan.strips[0];
        assert_eq!(casque.uid, "u1");
        assert_eq!(casque.kind.to_string(), "output");
        assert!(casque.mute);
        // Migrations: release→decay on the gate, short EQ band renamed.
        assert_eq!(casque.effects[&EffectKey::Gate].params["Decay (ms)"], 200.0);
        assert!(casque.effects[&EffectKey::Eq].params.contains_key("50Hz gain (low shelving)"));
        // Old compressor threshold mapped to its new port name.
        assert_eq!(
            casque.effects[&EffectKey::Compressor].params["Threshold level (dB)"],
            -12.0
        );

        let micro = &plan.strips[1];
        assert!(micro.effects[&EffectKey::Gate].active);
        assert!(!micro.effects[&EffectKey::NoiseCancel].params.contains_key("Model"));
        assert_eq!(micro.effects[&EffectKey::NoiseCancel].params.len(), 4);
    }

    #[test]
    fn missing_strips_is_an_error() {
        let path = scratch_config("nostrips", r#"{"window":{}}"#);
        match load_plan(&path) {
            Err(CheckError::MissingStrips(p)) => assert_eq!(p, path),
            other => panic!("expected MissingStrips, got {other:?}"),
        }
    }

    #[test]
    fn malformed_config_is_reported() {
        let path = scratch_config("malformed", "{ this is not json");
        assert!(matches!(
            load_plan(&path),
            Err(CheckError::Config(ConfigError::Json(_)))
        ));
    }

    #[test]
    fn print_plan_summary_shape() {
        let path = scratch_config(
            "print",
            r#"{
                "strips": [
                    {
                        "uid": "u1", "label": "Casque", "kind": "output",
                        "volume": 0.5,
                        "device_name": "alsa_output.x.analog-stereo",
                        "effects": { "tube": { "active": true, "params": { "Drive": 2.575 } } }
                    },
                    {
                        "uid": "u2", "label": "Micro", "kind": "input",
                        "effects": { "gate": true }
                    }
                ]
            }"#,
        );
        let plan = load_plan(&path).unwrap();
        let mut buf = Vec::new();
        print_plan(&plan, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();

        assert!(text.contains("Strips: 2"));
        assert!(text.contains("Active effects: 2"));
        assert!(text.contains("[output] \"Casque\""));
        assert!(text.contains("volume=0.50"));
        assert!(text.contains("device=alsa_output.x.analog-stereo"));
        // Output strips only build the tonal chain: their own block lists no
        // gate, even though the (input) Micro strip below legitimately has one
        // active.
        let out_start = text.find("[output] \"Casque\"").unwrap();
        let out_end = text.find("2. [input]").unwrap();
        let output_block = &text[out_start..out_end];
        assert!(output_block.contains("effects (3 applicable)"));
        assert!(output_block.contains("tube: active Drive=2.575, Tape--Tube Blend=10"));
        assert!(output_block.contains("compressor: inactive"));
        assert!(
            !output_block.contains("gate:"),
            "gate is not applicable to outputs; block was:\n{output_block}"
        );
        assert!(text.contains("gate: active"), "Micro input gate must show active");
    }
}
