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
//!
//! # Shutdown
//!
//! `SIGTERM`/`SIGINT` arm the `AtomicBool` polled by the server's accept
//! loop: the daemon then stops accepting, drops the backend (running its
//! teardown — the PipeWire graph is client-owned and goes away with it),
//! removes the socket file and exits with status 0.  A *second* signal is
//! treated as "force quit" (exit 130) so a wedged order can never trap the
//! process.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use holaf_pw::{FxHostMode, PipeWireBackend};
use holafd::config_check::{load_plan, print_plan};
use holafd::{FakeBackend, Server, UnimplementedBackend};

const USAGE: &str = "\
holafd — Holaf-Mix daemon (NDJSON Unix-socket server)

USAGE:
    holafd [--socket <path>] [--backend fake|unimplemented|pipewire]
           [--fx-host-mode external|inprocess]
    holafd --fx-host
    holafd --check-config <config.json>
    holafd --help

OPTIONS:
    --socket <path>        Unix socket path (default: $HOLAF_SOCKET or /tmp/holaf-$USER.sock)
    --backend <kind>       Backend to run: 'fake' (in-memory) or 'unimplemented'
                           (default) or 'pipewire' (real PipeWire engine).  The
                           pipewire backend connects through the PipeWire env
                           (XDG_RUNTIME_DIR / PIPEWIRE_RUNTIME_DIR / PIPEWIRE_REMOTE).
    --fx-host-mode <mode>  Who hosts the strips' filter-chains:
                           'external' (default) delegates them to a dedicated
                           child process — `holafd --fx-host`, spawned on
                           demand — whose only job is to run a PipeWire
                           mainloop, exactly like the `pw-cli` the Python
                           engine uses; 'inprocess' keeps them inside this
                           process (the pre-2026-11 behaviour, and the
                           automatic fallback if the child cannot be spawned).
                           Env: $HOLAF_FX_HOST.
    --fx-host              Run as the FX host child (internal: started by
                           holafd itself; talks NDJSON on stdin/stdout and
                           never opens a socket).
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
        fx_host: FxHostMode,
    },
    /// `--fx-host`: be the FX host process (child of a `Run` daemon).
    FxHost,
    CheckConfig(PathBuf),
    Help,
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let mut socket: Option<PathBuf> = None;
    let mut backend = BackendKind::Unimplemented;
    let mut check_config: Option<PathBuf> = None;
    let mut fx_host = match std::env::var("HOLAF_FX_HOST") {
        Ok(v) if !v.is_empty() => {
            FxHostMode::parse(&v).map_err(|e| format!("HOLAF_FX_HOST: {e}"))?
        }
        _ => FxHostMode::default(),
    };
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
            "--fx-host" => return Ok(Command::FxHost),
            "--fx-host-mode" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--fx-host-mode requires external|inprocess".to_string())?;
                fx_host = FxHostMode::parse(value)?;
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
    Ok(Command::Run {
        socket,
        backend,
        fx_host,
    })
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

fn run_daemon(
    socket: PathBuf,
    backend: BackendKind,
    fx_host: FxHostMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    // Install the handlers *before* binding/connecting so a Ctrl-C during a
    // slow PipeWire connect is still honoured.
    install_signal_handlers(Arc::clone(&shutdown))?;

    let server = match backend {
        BackendKind::Fake => Server::bind(&socket, FakeBackend::new())?,
        BackendKind::Unimplemented => Server::bind_boxed(&socket, Box::new(UnimplementedBackend))?,
        BackendKind::PipeWire => {
            log::info!("connecting to the PipeWire daemon of the current environment…");
            let pw = PipeWireBackend::connect_with(fx_host).map_err(|e| {
                std::io::Error::other(format!("cannot start the pipewire backend: {e}"))
            })?;
            Server::bind(&socket, pw)?
        }
    };
    log::warn!(
        "backend '{backend:?}' — 'fake' simulates in memory; 'pipewire' drives a live daemon"
    );
    log::info!("listening on {}", server.socket_path().display());
    server.run(Arc::clone(&shutdown))?;
    // Dropping the server stops the backend owner thread (which runs the
    // backend teardown) and removes the socket file.
    log::info!("daemon stopped");
    Ok(())
}

/// Arm the graceful-shutdown flag on `SIGTERM`/`SIGINT`.
///
/// Runs a small dedicated thread iterating the signals (rather than
/// `signal_hook::flag::register`) because we also want an explicit log line
/// naming the signal, and a "force quit" on a second signal.
fn install_signal_handlers(shutdown: Arc<AtomicBool>) -> io::Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT])?;
    thread::Builder::new()
        .name("holafd-signals".into())
        .spawn(move || {
            for signal in signals.forever() {
                let name = match signal {
                    SIGTERM => "SIGTERM",
                    SIGINT => "SIGINT",
                    other => {
                        // Cannot happen: only the two above are registered.
                        log::warn!("received unexpected signal {other}, shutting down");
                        "signal"
                    }
                };
                if shutdown.swap(true, Ordering::SeqCst) {
                    log::warn!("received a second {name} — exiting immediately");
                    std::process::exit(130);
                }
                log::info!("received {name}, shutting down");
            }
        })
        .map(|_| ())
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
        // The FX host is a *child* process: it never opens a socket, never
        // accepts orders, and its whole job is running a PipeWire mainloop
        // while it hosts filter-chains (see holaf_pw::fxhost).
        Command::FxHost => holaf_pw::fxhost::run_host(),
        Command::Run {
            socket,
            backend,
            fx_host,
        } => match run_daemon(socket, backend, fx_host) {
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
            Command::Run {
                backend,
                socket,
                fx_host,
            } => {
                assert_eq!(backend, BackendKind::Unimplemented);
                assert!(socket.as_os_str().to_string_lossy().contains("holaf-"));
                // Default hosting: a dedicated child process (the `pw-cli`
                // shape the Python engine proved).
                assert_eq!(fx_host, FxHostMode::External);
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
    fn parse_fx_host_mode_flag() {
        let args: Vec<String> = vec!["--fx-host-mode".into(), "inprocess".into()];
        match parse_args(&args).unwrap() {
            Command::Run { fx_host, .. } => assert_eq!(fx_host, FxHostMode::InProcess),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_fx_host_mode_rejects_nonsense() {
        let args: Vec<String> = vec!["--fx-host-mode".into(), "elsewhere".into()];
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_fx_host_subcommand() {
        let args: Vec<String> = vec!["--fx-host".into()];
        assert!(matches!(parse_args(&args).unwrap(), Command::FxHost));
        // It must not be treated as a daemon run (which would open a socket).
        assert!(!matches!(parse_args(&args).unwrap(), Command::Run { .. }));
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
