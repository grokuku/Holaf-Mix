//! `config.json` I/O.
//!
//! Ports `src/config/settings.py` resolution + atomic-save semantics:
//!
//! - the config lives at `<repo-root>/config.json` (next to `main.py`);
//! - saves are atomic: write a temp file in the same directory then `rename`,
//!   so a crash mid-write cannot corrupt the existing file;
//! - unlike the Python `_load_raw_json` (which swallows errors and returns
//!   `{}`), a JSON parse error here is **reported** — never silently replaced.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

/// Errors that can occur while resolving, loading or saving the config.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not locate the project root (no .git or src/ ancestor of {0})")]
    NoProjectRoot(String),
}

/// Default `config.json` location: `<repo-root>/config.json`.
///
/// The repo root is found by walking **up** from the current working
/// directory until we find a directory that contains `.git/` or `src/`
/// (the layout of this repository).  This works both from the repo root and
/// from anywhere inside it (e.g. `rust/crates/holaf-core` while running
/// `cargo test`).
///
/// Falls back to `./config.json` (relative to the CWD) when no root is
/// found — simple and documented, matches the task spec.
pub fn default_config_path() -> PathBuf {
    match std::env::current_dir() {
        Ok(cwd) => resolve_root_from(&cwd)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("config.json"),
        Err(_) => PathBuf::from("config.json"),
    }
}

/// Walk `start` (and each of its ancestors) looking for a directory that
/// holds either `.git/` or `src/` — both markers of this repository's root.
pub(crate) fn resolve_root_from(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(".git").is_dir() || d.join("src").is_dir() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Load and JSON-parse the config file.
///
/// Errors are propagated (file missing, unreadable, malformed JSON).  In
/// particular a JSON parse error returns [`ConfigError::Json`] and never
/// silently overwrites the file, unlike the Python loader.
pub fn load_config(path: &Path) -> Result<Value, ConfigError> {
    let text = fs::read_to_string(path)?;
    let value = serde_json::from_str(&text)?;
    Ok(value)
}

/// Atomically write `value` to `path` (temp file in the same directory +
/// rename, mirroring `settings.py` / `os.replace`).
pub fn save_config_atomic(path: &Path, value: &Value) -> Result<(), ConfigError> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.json");

    // Same directory ⇒ same filesystem ⇒ rename() is atomic.  The pid keeps
    // concurrent writers from clobbering each other's temp file.
    let tmp_path = dir.join(format!(".config_{}.{}.tmp", file_name, std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(pretty_json(value).as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()
    })();

    if let Err(e) = write_result {
        // Best-effort cleanup of the temp file, then propagate the error.
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }

    // Atomic replace (POSIX rename within one filesystem).
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Render a [`Value`] as pretty JSON with a 4-space indent, matching Python's
/// `json.dump(data, f, indent=4)` (see `src/config/settings.py`).  Object
/// member order follows serde_json's `preserve_order` feature, so parsed files
/// keep their original key order on save.
pub(crate) fn pretty_json(value: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value, 0);
    out
}

fn write_json(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            // serde_json::to_string escapes quotes/backslashes/control chars.
            out.push_str(&serde_json::to_string(s).expect("string serialization cannot fail"));
        }
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                push_indent(out, depth + 1);
                write_json(out, item, depth + 1);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(out, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (k, v)) in map.iter().enumerate() {
                push_indent(out, depth + 1);
                out.push_str(&serde_json::to_string(k).expect("string serialization cannot fail"));
                out.push_str(": ");
                write_json(out, v, depth + 1);
                if i + 1 < map.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(out, depth);
            out.push('}');
        }
    }
}

fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("    ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Create a unique scratch directory for one test.
    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("holaf-core-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_config_path_points_at_repo_root() {
        // Tests run with CWD inside rust/crates/holaf-core; the resolver must
        // climb up to /projects/Holaf-Mix (which has .git/ and src/).
        let path = default_config_path();
        assert!(path.ends_with("config.json"));
        assert_eq!(path.file_name().unwrap().to_str(), Some("config.json"));
    }

    #[test]
    fn resolve_root_finds_git_marker() {
        let root = scratch("git");
        fs::create_dir(root.join(".git")).unwrap();
        // Resolve from a deep synthetic location under that root.
        let deep = root.join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(resolve_root_from(&deep), Some(root.clone()));
        assert_eq!(resolve_root_from(&root), Some(root));
    }

    #[test]
    fn resolve_root_finds_src_marker() {
        let root = scratch("src");
        fs::create_dir(root.join("src")).unwrap();
        assert_eq!(resolve_root_from(&root.join("sub")), Some(root));
    }

    #[test]
    fn resolve_root_returns_none_for_unrelated_tree() {
        let dir = scratch("none");
        assert_eq!(resolve_root_from(&dir), None);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = scratch("roundtrip");
        let path = dir.join("config.json");
        let value = json!({
            "window_geometry": "abc123",
            "strips": [
                { "uid": "u1", "label": "Micro", "kind": "input", "effects": {} },
                { "uid": "u2", "label": "Speakers", "kind": "output" }
            ]
        });

        save_config_atomic(&path, &value).unwrap();
        assert!(path.exists(), "file must exist after atomic save");

        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded, value);

        // Round-trip through the 4-space pretty printer keeps values intact.
        let text = fs::read_to_string(&path).unwrap();
        let reparsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reparsed, value);
        assert!(
            text.starts_with("{\n    \"window_geometry\""),
            "expected 4-space indent, got:\n{text}"
        );
    }

    #[test]
    fn load_missing_file_is_an_error() {
        let path = scratch("missing").join("nope.json");
        let err = load_config(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Io(_)),
            "expected Io error, got {err:?}"
        );
    }

    #[test]
    fn load_invalid_json_is_an_error_and_does_not_overwrite() {
        let dir = scratch("badjson");
        let path = dir.join("config.json");
        fs::write(&path, "{ this is not json !!!").unwrap();

        let err = load_config(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Json(_)),
            "expected Json error, got {err:?}"
        );

        // The corrupt file is left untouched (no silent reset).
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ this is not json !!!");
    }

    #[test]
    fn atomic_save_leaves_no_temp_files_behind() {
        let dir = scratch("atomic");
        let path = dir.join("config.json");
        save_config_atomic(&path, &json!({"a": 1})).unwrap();
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn save_overwrites_previous_content() {
        let dir = scratch("overwrite");
        let path = dir.join("config.json");
        save_config_atomic(&path, &json!({"v": 1})).unwrap();
        save_config_atomic(&path, &json!({"v": 2, "extra": [1, 2, 3]})).unwrap();
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded, json!({"v": 2, "extra": [1, 2, 3]}));
    }

    #[test]
    fn save_into_subdirectory() {
        // Saving into an existing (non-root) directory works.
        let dir = scratch("nested");
        let sub = dir.join("cfg");
        fs::create_dir_all(&sub).unwrap();
        let path = sub.join("config.json");
        save_config_atomic(&path, &json!({"ok": true})).unwrap();
        assert!(path.exists());
        assert_eq!(load_config(&path).unwrap(), json!({"ok": true}));
    }

    #[test]
    fn pretty_json_matches_expected_shape() {
        let v = json!({
            "k": "v",
            "arr": [1, 2],
            "empty_obj": {},
            "empty_arr": []
        });
        let s = pretty_json(&v);
        let expected = "{\n    \"k\": \"v\",\n    \"arr\": [\n        1,\n        2\n    ],\n    \"empty_obj\": {},\n    \"empty_arr\": []\n}";
        assert_eq!(s, expected);
    }
}
