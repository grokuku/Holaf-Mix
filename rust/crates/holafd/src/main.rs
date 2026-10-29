//! `holafd` — the Holaf-Mix daemon binary.
//!
//! Usage:
//!
//! ```text
//! holafd [--socket <path>] [--backend fake|unimplemented]
//! holafd --check-config <config.json>
//! holafd --help
//! ```
//!
//! Default socket: `$HOLAF_SOCKET`, else `/tmp/holaf-$USER.sock`.  Logging via
//! `env_logger` (`RUST_LOG`).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use holaf_pw::PipeWireBackend;
use holafd::config_check::{load_plan, print_plan};
use holafd::{FakeBackend, Server, UnimplementedBackend};

const USAGE: &str = "\
holafd — Holaf-Mix daemon (NDJSON Unix-socket server)

USAGE:
    holafd [--socket <path>] [--backend fake|unimplemented|pipewire]
    holafd --check-config <config.json>
    holafd --help

OPTIONS:
    --socket <path>        Unix socket path (default: $HOLAF_SOCKET or /tmp/holaf-$USER.sock)
    --backend <kind>       Backend to run: 'fake' (in-memory) or 'unimplemented'
                           (default) or 'pipewire' (real PipeWire engine).  The
                           pipewire backend connects through the PipeWire env
                           (XDG_RUNTIME_DIR / PIPEWIRE_RUNTIME_DIR / PIPEWIRE_REMOTE).
    --check-config <path>  Load config.json, run migrations and print the graph
                           plan WITHOUT connecting to PipeWire; exit 0.
    --help                 Show this help and exit
";

/// Daemon backend choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    Fake,
    Unimplemented,
    PipeWire,
}

/// Parsed CLI invocation.
#[derive(Debug)]
enum Command {
    Run {
        socket: PathBuf,
        backend: BackendKind,
    },
    CheckConfig(PathBuf),
    Help,
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let mut socket: Option<PathBuf> = None;
    let mut backend = BackendKind::Unimplemented;
    let mut check_config: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--socket" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--socket requires a path argument".to_string())?;
                socket = Some(PathBuf::from(value));
            }
            "--backend" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--backend requires fake|unimplemented|pipewire".to_string())?;
                backend = match value.as_str() {
                    "fake" => BackendKind::Fake,
                    "unimplemented" => BackendKind::Unimplemented,
                    "pipewire" => BackendKind::PipeWire,
                    other => {
                        return Err(format!(
                            "unknown backend '{other}' (expected 'fake', 'unimplemented' or 'pipewire')"
                        ))
                    }
                };
            }
            "--check-config" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--check-config requires a config path".to_string())?;
                check_config = Some(PathBuf::from(value));
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }

    if let Some(path) = check_config {
        if socket.is_some() || backend != BackendKind::Unimplemented {
            return Err("--check-config cannot be combined with --socket/--backend".to_string());
        }
        return Ok(Command::CheckConfig(path));
    }

    let socket = socket.unwrap_or_else(default_socket_path);
    Ok(Command::Run { socket, backend })
}

/// Resolve the default socket: `$HOLAF_SOCKET`, else `/tmp/holaf-$USER.sock`.
fn default_socket_path() -> PathBuf {
    default_socket_from(
        std::env::var("HOLAF_SOCKET").ok().as_deref(),
        std::env::var("USER").ok().as_deref(),
        &std::env::temp_dir(),
    )
}

/// Pure core of [`default_socket_path`] (injectable for tests, avoids env
/// races between parallel tests).
fn default_socket_from(
    holaf_socket: Option<&str>,
    user: Option<&str>,
    temp_dir: &std::path::Path,
) -> PathBuf {
    if let Some(s) = holaf_socket {
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    temp_dir.join(format!("holaf-{}.sock", user.unwrap_or("holaf")))
}

fn run_check_config(path: &Path) -> i32 {
    let plan = match load_plan(path) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    match print_plan(&plan, &mut std::io::stdout()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: cannot write plan: {e}");
            2
        }
    }
}

fn run_daemon(socket: PathBuf, backend: BackendKind) -> Result<(), Box<dyn std::error::Error>> {
    let server = match backend {
        BackendKind::Fake => Server::bind(&socket, FakeBackend::new())?,
        BackendKind::Unimplemented => Server::bind_boxed(&socket, Box::new(UnimplementedBackend))?,
        BackendKind::PipeWire => {
            log::info!("connecting to the PipeWire daemon of the current environment…");
            let pw = PipeWireBackend::connect().map_err(|e| {
                std::io::Error::other(format!("cannot start the pipewire backend: {e}"))
            })?;
            Server::bind(&socket, pw)?
        }
    };
    log::warn!(
        "backend '{backend:?}' — 'fake' simulates in memory; 'pipewire' drives a live daemon"
    );
    log::info!("listening on {}", server.socket_path().display());
    let shutdown = Arc::new(AtomicBool::new(false));
    server.run(shutdown)?;
    Ok(())
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let code = match command {
        Command::Help => {
            print!("{USAGE}");
            0
        }
        Command::CheckConfig(path) => run_check_config(&path),
        Command::Run { socket, backend } => match run_daemon(socket, backend) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        },
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_socket_uses_env_when_set() {
        let p = default_socket_from(Some("/tmp/custom.sock"), Some("ignored"), Path::new("/tmp"));
        assert_eq!(p, PathBuf::from("/tmp/custom.sock"));
    }

    #[test]
    fn default_socket_ignores_empty_env() {
        let p = default_socket_from(Some(""), Some("alice"), Path::new("/tmp"));
        assert_eq!(p, PathBuf::from("/tmp/holaf-alice.sock"));
    }

    #[test]
    fn default_socket_falls_back_to_tmp_per_user() {
        let p = default_socket_from(None, Some("bob"), Path::new("/tmp"));
        assert_eq!(p, PathBuf::from("/tmp/holaf-bob.sock"));
        // And to a fixed user name when none is known.
        let p = default_socket_from(None, None, Path::new("/tmp"));
        assert_eq!(p, PathBuf::from("/tmp/holaf-holaf.sock"));
    }

    #[test]
    fn parse_run_defaults() {
        let args: Vec<String> = vec![];
        match parse_args(&args).unwrap() {
            Command::Run { backend, socket } => {
                assert_eq!(backend, BackendKind::Unimplemented);
                assert!(socket.as_os_str().to_string_lossy().contains("holaf-"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_backend_fake() {
        let args: Vec<String> = vec!["--backend".into(), "fake".into()];
        match parse_args(&args).unwrap() {
            Command::Run { backend, .. } => assert_eq!(backend, BackendKind::Fake),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_backend_pipewire() {
        let args: Vec<String> = vec!["--backend".into(), "pipewire".into()];
        match parse_args(&args).unwrap() {
            Command::Run { backend, .. } => assert_eq!(backend, BackendKind::PipeWire),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_check_config() {
        let args: Vec<String> = vec!["--check-config".into(), "cfg.json".into()];
        match parse_args(&args).unwrap() {
            Command::CheckConfig(p) => assert_eq!(p, PathBuf::from("cfg.json")),
            other => panic!("expected CheckConfig, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        let args: Vec<String> = vec!["--nope".into()];
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_rejects_unknown_backend() {
        let args: Vec<String> = vec!["--backend".into(), "pw".into()];
        assert!(parse_args(&args).is_err());
    }
}
