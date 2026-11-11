//! Clean-shutdown tests of the `holafd` **binary**: it must honour `SIGTERM`
//! and `SIGINT` (spec §4 / robustness), exit with status 0 and remove its
//! socket file.
//!
//! These are real process tests (`CARGO_BIN_EXE_holafd`) with
//! `--backend fake`, so no PipeWire is involved and they run in CI as-is.
//! Every wait is bounded: a wedged daemon fails the test instead of hanging
//! the suite.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A unique socket path per test (process + nanos) so parallel tests never
/// collide.
fn unique_socket(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "holafd-shutdown-{tag}-{}-{nanos}.sock",
        std::process::id()
    ))
}

/// Spawn `holafd --backend fake` on `socket`, logging to a side file.
fn spawn_daemon(socket: &Path, log: &Path) -> Child {
    let stderr = std::fs::File::create(log).expect("create the daemon log file");
    Command::new(env!("CARGO_BIN_EXE_holafd"))
        .arg("--backend")
        .arg("fake")
        .arg("--socket")
        .arg(socket)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn the holafd binary")
}

/// Wait until the daemon has bound its socket.
fn wait_for_socket(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if socket.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Wait until the daemon exits, returning its exit status.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait on the daemon") {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    None
}

/// `kill(1)` instead of a `libc` dependency: one signal, one process.
fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("run kill(1)");
    assert!(status.success(), "kill -{signal} {pid} failed: {status:?}");
}

/// Full scenario: start, send `signal`, expect exit 0 + socket removal + the
/// matching log line.
fn assert_clean_shutdown(signal: &str, expected_log: &str) {
    let socket = unique_socket(signal);
    let log = socket.with_extension("log");
    let mut child = spawn_daemon(&socket, &log);
    let pid = child.id();

    if !wait_for_socket(&socket, Duration::from_secs(20)) {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the daemon did not bind {}", socket.display());
    }

    send_signal(pid, signal);

    let status = match wait_for_exit(&mut child, Duration::from_secs(20)) {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the daemon ignored {signal} (still running after 20 s)");
        }
    };

    let logs = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        status.code(),
        Some(0),
        "{signal} must exit cleanly (0), got {status:?}; log:\n{logs}"
    );
    assert!(
        logs.contains(expected_log),
        "expected '{expected_log}' in the daemon log; log:\n{logs}"
    );
    assert!(
        !socket.exists(),
        "the socket {} must be removed on shutdown",
        socket.display()
    );

    let _ = std::fs::remove_file(&log);
}

#[test]
fn sigterm_stops_the_daemon_cleanly() {
    assert_clean_shutdown("TERM", "received SIGTERM, shutting down");
}

#[test]
fn sigint_stops_the_daemon_cleanly() {
    assert_clean_shutdown("INT", "received SIGINT, shutting down");
}
