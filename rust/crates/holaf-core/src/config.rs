//! `config.json` I/O.
//!
//! - the config path is resolved by [`default_config_path`] (see its docs for
//!   the priority order: `$HOLAF_CONFIG`, the source tree, next to the
//!   executable, the working directory, then XDG);
//! - saves are atomic: write a temp file in the same directory then `rename`,
//!   so a crash mid-write cannot corrupt the existing file;
//! - a missing parent directory is created on save (that is what makes a first
//!   run in `$XDG_CONFIG_HOME/holaf-mix/` work);
//! - a JSON parse error is **reported** — never silently replaced by `{}`.

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

/// Environment variable that overrides the config path outright.
pub const CONFIG_ENV: &str = "HOLAF_CONFIG";

/// Directory name used under `$XDG_CONFIG_HOME`.
pub const XDG_APP_DIR: &str = "holaf-mix";

/// Default `config.json` location, in priority order:
///
/// 1. **`$HOLAF_CONFIG`** — an explicit path, used as-is (the user asked for
///    it; it does not have to exist yet).
/// 2. **Source tree** — `<repo-root>/config.json` when the working directory is
///    inside this repository (an ancestor holds `.git/` or `src/`).  This keeps
///    `cargo run` / `cargo test` on the repo's own `config.json`.
/// 3. **Next to the executable**, when that `config.json` already exists — the
///    tarball case, and an installed `holaf-ui` started from a `.desktop` file
///    (whose working directory is `$HOME`, not the install directory).
/// 4. **`./config.json`** in the working directory, when it exists.
/// 5. **`$XDG_CONFIG_HOME/holaf-mix/config.json`** (default
///    `~/.config/holaf-mix/config.json`) — where a fresh system install
///    creates its config.
///
/// `--config FILE` on the command line has always priority over all of the
/// above: it is handled by the binary before this function is called.
pub fn default_config_path() -> PathBuf {
    let explicit = std::env::var_os(CONFIG_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let cwd = std::env::current_dir().ok();
    let exe_dir = executable_dir();
    let xdg_dir = xdg_config_home();
    resolve_config_path(explicit, cwd.as_deref(), exe_dir.as_deref(), xdg_dir.as_deref())
}

/// Pure resolution used by [`default_config_path`], so all five cases are
/// unit-testable without touching the process environment.
///
/// `xdg_dir` is the *base* XDG directory (`~/.config`), not the app dir.
pub fn resolve_config_path(
    explicit: Option<PathBuf>,
    cwd: Option<&Path>,
    exe_dir: Option<&Path>,
    xdg_dir: Option<&Path>,
) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }

    if let Some(cwd) = cwd {
        if let Some(root) = resolve_root_from(cwd) {
            return root.join("config.json");
        }
    }

    if let Some(dir) = exe_dir {
        let candidate = dir.join("config.json");
        if candidate.is_file() {
            return candidate;
        }
    }

    if let Some(cwd) = cwd {
        let candidate = cwd.join("config.json");
        if candidate.is_file() {
            return candidate;
        }
    }

    match xdg_dir {
        Some(dir) => dir.join(XDG_APP_DIR).join("config.json"),
        None => PathBuf::from(XDG_APP_DIR).join("config.json"),
    }
}

/// Directory holding the running executable, with symlinks resolved.
///
/// Resolving the symlink matters: `~/.local/bin/holaf-ui -> ~/apps/…/holaf-ui`
/// must look for its config next to the *real* file, not in `~/.local/bin`.
fn executable_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent().map(Path::to_path_buf)
}

/// `$XDG_CONFIG_HOME`, else `$HOME/.config`, else `None`.
fn xdg_config_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".config"))
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

    // A first run may resolve to `$XDG_CONFIG_HOME/holaf-mix/config.json`,
    // whose directory does not exist yet.  Creating it here (instead of in the
    // resolver) keeps `default_config_path` free of side effects.
    if !dir.as_os_str().is_empty() {
        fs::create_dir_all(dir)?;
    }

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
        // climb up to the repository root (which has .git/).
        let path = default_config_path();
        assert!(path.ends_with("config.json"));
        assert_eq!(path.file_name().unwrap().to_str(), Some("config.json"));
    }

    // --- config path resolution: the five documented cases -----------------

    /// A scratch directory with no `.git`/`src` marker (i.e. not a repo).
    fn plain_dir(tag: &str) -> PathBuf {
        scratch(tag)
    }

    #[test]
    fn resolved_path_case_1_explicit_override_wins() {
        let dir = scratch("explicit");
        let explicit = dir.join("chosen.json");
        let path = resolve_config_path(
            Some(explicit.clone()),
            Some(&dir),
            Some(&dir),
            Some(&dir.join("xdg")),
        );
        // Used as-is, even though it does not exist.
        assert_eq!(path, explicit);
    }

    #[test]
    fn resolved_path_case_2_source_tree_wins_over_existing_neighbours() {
        let root = scratch("tree");
        fs::create_dir(root.join(".git")).unwrap();
        let cwd = root.join("rust").join("crates");
        fs::create_dir_all(&cwd).unwrap();
        let exe_dir = plain_dir("tree-exe");
        // An exe-adjacent config exists: the source tree must still win.
        fs::write(exe_dir.join("config.json"), "{}").unwrap();

        let path = resolve_config_path(None, Some(&cwd), Some(&exe_dir), Some(&root));
        assert_eq!(path, root.join("config.json"));
    }

    #[test]
    fn resolved_path_case_3_exe_adjacent_file() {
        let cwd = plain_dir("exe-cwd");
        let exe_dir = plain_dir("exe-dir");
        fs::write(exe_dir.join("config.json"), "{}").unwrap();

        let path = resolve_config_path(None, Some(&cwd), Some(&exe_dir), Some(&cwd.join("xdg")));
        assert_eq!(path, exe_dir.join("config.json"));
    }

    #[test]
    fn resolved_path_case_4_working_directory_file() {
        let cwd = plain_dir("cwd");
        let exe_dir = plain_dir("cwd-exe");
        fs::write(cwd.join("config.json"), "{}").unwrap();

        let path = resolve_config_path(None, Some(&cwd), Some(&exe_dir), Some(&cwd.join("xdg")));
        assert_eq!(path, cwd.join("config.json"));
    }

    #[test]
    fn resolved_path_case_5_falls_back_to_xdg() {
        let cwd = plain_dir("xdg-cwd");
        let exe_dir = plain_dir("xdg-exe");
        let xdg = cwd.join("config-home");

        let path = resolve_config_path(None, Some(&cwd), Some(&exe_dir), Some(&xdg));
        assert_eq!(path, xdg.join(XDG_APP_DIR).join("config.json"));
    }

    #[test]
    fn resolved_path_without_any_base_uses_a_relative_xdg_dir() {
        let path = resolve_config_path(None, None, None, None);
        assert_eq!(path, PathBuf::from(XDG_APP_DIR).join("config.json"));
    }

    #[test]
    fn saving_creates_a_missing_parent_directory() {
        let dir = scratch("mkdir");
        let path = dir.join("holaf-mix").join("nested").join("config.json");
        save_config_atomic(&path, &json!({ "a": 1 })).unwrap();
        assert_eq!(load_config(&path).unwrap(), json!({ "a": 1 }));
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
