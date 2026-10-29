//! `holafctl` — Holaf-Mix CLI debug client for the `holafd` NDJSON protocol.
//!
//! ```text
//! holafctl [--socket <path>] status
//! holafctl [--socket <path>] apply-snapshot --config <config.json>
//! holafctl [--socket <path>] volume <uid> <0.0..1.0>
//! holafctl [--socket <path>] mute <uid> <true|false>
//! holafctl [--socket <path>] fx-active <uid> <effect> <true|false>
//! holafctl [--socket <path>] fx-params <uid> <effect> <key=value>...
//! ```
//!
//! Each command opens a fresh connection, sends one request (id 1), prints the
//! pretty JSON response and exits 0 on success / 1 on error.  Only
//! [`holaf-ipc`] is needed: `apply-snapshot` forwards the raw `config.json`
//! strips and lets the daemon run its `holaf-core` migrations, keeping this
//! client a pure transport.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use holaf_ipc::{encode, Event, Request, Response, METHOD_APPLY_SNAPSHOT, METHOD_FX_SET_ACTIVE,
    METHOD_FX_SET_PARAMS, METHOD_STATUS, METHOD_STRIP_SET_MUTE, METHOD_STRIP_SET_VOLUME};
use serde_json::{json, Value};

const USAGE: &str = "\
holafctl — Holaf-Mix CLI debug client

USAGE:
    holafctl [--socket <path>] <command> [args...]

GLOBAL:
    --socket <path>   Unix socket of holafd (default: $HOLAF_SOCKET or /tmp/holaf-$USER.sock)
    --help            Show this help and exit

COMMANDS:
    status
    apply-snapshot --config <config.json>
    volume  <uid> <0.0..1.0>
    mute    <uid> <true|false>
    fx-active <uid> <effect> <true|false>
    fx-params <uid> <effect> <key=value>...
";

/// Resolve the daemon socket the same way `holafd` does.
fn default_socket() -> PathBuf {
    if let Ok(s) = std::env::var("HOLAF_SOCKET") {
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "holaf".to_string());
    std::env::temp_dir().join(format!("holaf-{user}.sock"))
}

/// Split the global `--socket` option from the command tail.
///
/// Returns `(socket_override, index_of_command)`.
fn parse_global(args: &[String]) -> Result<(Option<PathBuf>, usize), String> {
    match args.first() {
        None => Err("no command given".to_string()),
        Some(arg) if arg == "--socket" => {
            let value = args
                .get(1)
                .ok_or_else(|| "--socket requires a path argument".to_string())?;
            Ok((Some(PathBuf::from(value)), 2))
        }
        Some(arg) if arg == "--help" || arg == "-h" => Err(String::new()), // caller prints usage
        Some(arg) if arg.starts_with('-') => Err(format!("unknown global option '{arg}'")),
        Some(_) => Ok((None, 0)),
    }
}

/// Parse a `true|false` (also accepting `1|0`) CLI boolean.
fn parse_bool(s: &str) -> Result<bool, String> {
    match s {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => Err(format!("expected true|false, got '{other}'")),
    }
}

/// Send `request` over a fresh connection and read the matching response.
fn call(socket: &Path, request: &Request) -> Result<Response, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("cannot connect to {}: {e}", socket.display()))?;
    stream
        .write_all(encode(request).as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            return Err("connection closed before a response arrived".to_string());
        }
        let value: Value = serde_json::from_str(&line)
            .map_err(|e| format!("malformed JSON from daemon: {e}"))?;
        if value.get("event").is_some() {
            // Ignore push events; wait for the response.
            if let Ok(ev) = serde_json::from_value::<Event>(value) {
                eprintln!("<event {}>", ev.event);
            }
            continue;
        }
        let response: Response = serde_json::from_value(value)
            .map_err(|e| format!("malformed response from daemon: {e}"))?;
        if response.id == request.id {
            return Ok(response);
        }
        // A response for an unknown id: not ours, keep reading.
    }
}

/// Print a response pretty-printed; return the process exit code.
fn finish(response: Response) -> i32 {
    println!("{}", serde_json::to_string_pretty(&response).unwrap());
    if response.ok {
        0
    } else {
        if let Some(err) = &response.error {
            eprintln!("error [{}]: {}", err.code, err.message);
        }
        1
    }
}

fn cmd_status(socket: &Path) -> i32 {
    let req = Request::with_params(1, METHOD_STATUS, json!({}));
    match call(socket, &req) {
        Ok(resp) => finish(resp),
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// Read `config.json`, forward the raw `strips` array and let the daemon run
/// its migrations (keeps holafctl a pure transport over holaf-ipc).
fn cmd_apply_snapshot(socket: &Path, tail: &[String]) -> i32 {
    let path = match tail {
        [flag, path] if flag == "--config" => PathBuf::from(path),
        _ => {
            eprintln!("usage: holafctl apply-snapshot --config <config.json>");
            return 2;
        }
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", path.display());
            return 1;
        }
    };
    let config: Value = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {} is not valid JSON: {e}", path.display());
            return 1;
        }
    };
    let strips = config
        .get("strips")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let req = Request::with_params(
        1,
        METHOD_APPLY_SNAPSHOT,
        json!({ "strips": strips }),
    );
    match call(socket, &req) {
        Ok(resp) => finish(resp),
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn cmd_volume(socket: &Path, tail: &[String]) -> i32 {
    let (uid, value) = match tail {
        [uid, value] => {
            let v: f32 = match value.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("error: invalid volume '{value}'");
                    return 2;
                }
            };
            if !(0.0..=1.0).contains(&v) {
                eprintln!("error: volume must be within 0.0..=1.0 (got {v})");
                return 2;
            }
            (uid.clone(), v)
        }
        _ => {
            eprintln!("usage: holafctl volume <uid> <0.0..1.0>");
            return 2;
        }
    };
    let req = Request::with_params(1, METHOD_STRIP_SET_VOLUME, json!({"uid": uid, "value": value}));
    match call(socket, &req) {
        Ok(resp) => finish(resp),
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn cmd_mute(socket: &Path, tail: &[String]) -> i32 {
    let (uid, muted) = match tail {
        [uid, flag] => match parse_bool(flag) {
            Ok(muted) => (uid.clone(), muted),
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        },
        _ => {
            eprintln!("usage: holafctl mute <uid> <true|false>");
            return 2;
        }
    };
    let req = Request::with_params(1, METHOD_STRIP_SET_MUTE, json!({"uid": uid, "muted": muted}));
    match call(socket, &req) {
        Ok(resp) => finish(resp),
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn cmd_fx_active(socket: &Path, tail: &[String]) -> i32 {
    let (uid, effect, active) = match tail {
        [uid, effect, flag] => match parse_bool(flag) {
            Ok(active) => (uid.clone(), effect.clone(), active),
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        },
        _ => {
            eprintln!("usage: holafctl fx-active <uid> <effect> <true|false>");
            return 2;
        }
    };
    let req = Request::with_params(
        1,
        METHOD_FX_SET_ACTIVE,
        json!({"uid": uid, "effect": effect, "active": active}),
    );
    match call(socket, &req) {
        Ok(resp) => finish(resp),
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn cmd_fx_params(socket: &Path, tail: &[String]) -> i32 {
    if tail.len() < 3 {
        eprintln!("usage: holafctl fx-params <uid> <effect> <key=value>...");
        return 2;
    }
    let uid = tail[0].clone();
    let effect = tail[1].clone();
    let mut params = serde_json::Map::new();
    for pair in &tail[2..] {
        let Some((key, value)) = pair.split_once('=') else {
            eprintln!("error: '{}' is not a key=value pair", pair);
            return 2;
        };
        if key.is_empty() {
            eprintln!("error: empty param key in '{}'", pair);
            return 2;
        }
        let value: f32 = match value.parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("error: '{}' has a non-numeric value", pair);
                return 2;
            }
        };
        params.insert(key.to_string(), json!(value));
    }
    let req = Request::with_params(
        1,
        METHOD_FX_SET_PARAMS,
        json!({"uid": uid, "effect": effect, "params": Value::Object(params)}),
    );
    match call(socket, &req) {
        Ok(resp) => finish(resp),
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn run(args: &[String]) -> i32 {
    let (socket_override, cmd_index) = match parse_global(args) {
        Ok(pair) => pair,
        Err(msg) if msg.is_empty() => {
            print!("{USAGE}");
            return 0;
        }
        Err(msg) => {
            eprintln!("error: {msg}\n\n{USAGE}");
            return 2;
        }
    };
    let socket = socket_override.unwrap_or_else(default_socket);
    let tail = &args[cmd_index..];
    if tail.is_empty() {
        eprintln!("error: no command\n\n{USAGE}");
        return 2;
    }
    match tail[0].as_str() {
        "status" => cmd_status(&socket),
        "apply-snapshot" => cmd_apply_snapshot(&socket, &tail[1..]),
        "volume" => cmd_volume(&socket, &tail[1..]),
        "mute" => cmd_mute(&socket, &tail[1..]),
        "fx-active" => cmd_fx_active(&socket, &tail[1..]),
        "fx-params" => cmd_fx_params(&socket, &tail[1..]),
        "--help" | "-h" => {
            print!("{USAGE}");
            0
        }
        other => {
            eprintln!("error: unknown command '{other}'\n\n{USAGE}");
            2
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&args);
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn global_socket_option_is_extracted() {
        let args = vec!["--socket".to_string(), "/tmp/x.sock".to_string(), "status".to_string()];
        let (socket, idx) = parse_global(&args).unwrap();
        assert_eq!(socket.as_deref(), Some(Path::new("/tmp/x.sock")));
        assert_eq!(idx, 2);
    }

    #[test]
    fn no_global_option_keeps_command_first() {
        let args = vec!["apply-snapshot".to_string(), "--config".to_string(), "c.json".to_string()];
        let (socket, idx) = parse_global(&args).unwrap();
        assert_eq!(socket, None);
        assert_eq!(idx, 0);
    }

    #[test]
    fn parse_bool_accepts_true_false_and_digits() {
        assert!(parse_bool("true").unwrap());
        assert!(!parse_bool("false").unwrap());
        assert!(parse_bool("1").unwrap());
        assert!(!parse_bool("0").unwrap());
        assert!(parse_bool("yes").is_err());
    }

    #[test]
    fn call_handles_events_before_response() {
        // Build a tiny fake daemon that writes an event then a response.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "holafctl-test-{}-{nanos}.sock",
            std::process::id()
        ));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            // Push an event, then the actual response.
            let event = holaf_ipc::Event::new("state_changed", json!({}));
            stream.write_all(encode(&event).as_bytes()).unwrap();
            let resp = Response::ok(1, json!({"ok": true}));
            stream.write_all(encode(&resp).as_bytes()).unwrap();
            stream.flush().unwrap();
        });
        let req = Request::new(1, "engine.status");
        let resp = call(&path, &req).unwrap();
        assert!(resp.ok);
        handle.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
