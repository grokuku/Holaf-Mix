//! End-to-end test of the **dedicated FX host** through the real `holafd`
//! binary: 7 strips with FX, one child process hosting every chain, then a
//! clean teardown that leaves nothing behind.
//!
//! This is the scenario of the reported bug (the app applying a full snapshot
//! with the EQ active at launch), driven exactly as the Python UI drives it:
//! `holafd --backend pipewire` + `apply-snapshot` over the NDJSON socket.
//!
//! Requires a reachable PipeWire daemon (the headless sandbox); skipped with a
//! note otherwise, like the other live tests.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use holaf_ipc::{decode_line, encode, Request, Response, METHOD_APPLY_SNAPSHOT, METHOD_STATUS};
use serde_json::{json, Value};

/// Both tests drive the *same* sandbox daemon (and restart it), so they must
/// never overlap: cargo runs tests of one binary in parallel threads.
fn live_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Restart the headless sandbox daemon so the test starts from a pristine
/// graph, exactly like the `holaf-pw` live tests do (PipeWire 0.3.65 stops
/// publishing `Port` globals after many create/destroy cycles on one daemon).
fn fresh_sandbox_daemon() {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../scripts/start-pipewire-headless.sh"
    );
    let _ = Command::new("bash").arg(script).arg("restart").output();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if Command::new("pw-dump")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn unique_socket(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "holafd-fxhost-{tag}-{}-{nanos}.sock",
        std::process::id()
    ))
}

/// Same gate as `holaf-pw`'s live tests: never touch a real user daemon.
fn live_allowed() -> bool {
    if std::env::var("HOLAF_PW_LIVE").as_deref() == Ok("1") {
        return true;
    }
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("PIPEWIRE_RUNTIME_DIR"))
        .unwrap_or_default();
    dir.contains("pw-runtime") || dir.contains("holaf")
}

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

/// Is `pid` a live (non-zombie) process?
fn pid_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let state = stat.rsplit(") ").next().and_then(|t| t.chars().next());
            !matches!(state, Some('Z') | None)
        }
        Err(_) => false,
    }
}

fn pid_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Client {
    fn connect(path: &Path) -> Client {
        let stream = UnixStream::connect(path).expect("connect to holafd");
        // Bound every wait: a wedged daemon must fail the test, not hang CI.
        stream
            .set_read_timeout(Some(Duration::from_secs(120)))
            .expect("set read timeout");
        let reader = BufReader::new(stream.try_clone().expect("clone"));
        Client {
            reader,
            writer: stream,
            next_id: 1,
        }
    }

    fn send(&mut self, method: &str, params: Value) -> Response {
        let id = self.next_id;
        self.next_id += 1;
        self.writer
            .write_all(encode(&Request::with_params(id, method, params)).as_bytes())
            .expect("write request");
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).expect("read reply");
            assert!(n > 0, "daemon closed the connection waiting for id {id}");
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = decode_line(line.trim()).expect("reply is JSON");
            if value.get("event").is_some() {
                continue; // push event interleaved with our response
            }
            let r: Response = serde_json::from_value(value).expect("response envelope");
            if r.id == id {
                return r;
            }
        }
    }

    fn status(&mut self) -> Value {
        let r = self.send(METHOD_STATUS, json!({}));
        assert!(r.error.is_none(), "status failed: {:?}", r.error);
        r.result.expect("status result")
    }
}

/// One strip of the "full app" scenario.
fn strip(uid: &str, with_fx: bool) -> Value {
    json!({
        "uid": uid,
        "label": format!("Strip {uid}"),
        "kind": "output",
        "mode": "virtual",
        "volume": 0.8,
        "mute": false,
        "is_mono": false,
        "routes": [],
        "device_name": null,
        "assigned_apps": [],
        "is_default": false,
        "effects": {
            "eq": { "active": with_fx, "params": {} },
            "compressor": { "active": false, "params": {} },
            "tube": { "active": false, "params": {} }
        }
    })
}

fn spawn_daemon(socket: &Path, log: &Path) -> Child {
    let stderr = std::fs::File::create(log).expect("create the daemon log");
    Command::new(env!("CARGO_BIN_EXE_holafd"))
        .args(["--backend", "pipewire", "--socket"])
        .arg(socket)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn holafd")
}

#[test]
fn seven_strips_run_in_one_dedicated_fx_host_and_leave_nothing_behind() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP fx_host e2e: no headless PipeWire configured");
        return;
    }
    fresh_sandbox_daemon();
    let socket = unique_socket("e2e");
    let log = socket.with_extension("log");
    let mut child = spawn_daemon(&socket, &log);
    if !wait_for_socket(&socket, Duration::from_secs(20)) {
        let _ = child.kill();
        let _ = child.wait();
        let logs = std::fs::read_to_string(&log).unwrap_or_default();
        // No daemon reachable: the live gate cannot do better than skip.
        eprintln!("SKIP fx_host e2e: the daemon did not come up (no PipeWire?):\n{logs}");
        return;
    }
    let daemon_pid = child.id();
    let mut client = Client::connect(&socket);

    // --- apply the full snapshot (7 strips, 5 of them with FX) --------------
    let strips: Vec<Value> = (0..7)
        .map(|i| strip(&format!("s{i}"), i < 5))
        .collect();
    let r = client.send(METHOD_APPLY_SNAPSHOT, json!({ "strips": strips }));
    assert!(r.error.is_none(), "apply_snapshot failed: {:?}", r.error);

    let status = client.status();
    assert_eq!(status["strips"].as_array().unwrap().len(), 7);
    assert_eq!(status["degraded"], json!(false), "graph must be complete: {status}");
    assert_eq!(status["pending"], json!([]), "nothing pending: {status}");

    // --- every chain is hosted by ONE child process, none by the daemon ----
    let host = status["fx_host"].clone();
    assert_eq!(host["mode"], json!("external"), "default hosting: {host}");
    let host_pid = host["pid"].as_u64().expect("fx_host.pid") as u32;
    assert_ne!(host_pid, daemon_pid, "the FX host must not be the daemon");
    assert!(pid_alive(host_pid), "the FX host (pid {host_pid}) must be running");
    let chains: Vec<String> = host["chains"]
        .as_array()
        .expect("fx_host.chains")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(chains.len(), 5, "5 chains hosted in the child: {host}");

    // The nodes the graph is wired to exist, in the daemon's registry.
    let nodes: Vec<String> = status["holaf_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    for uid in ["s0", "s1", "s2", "s3", "s4"] {
        for want in [
            format!("Holaf_Strip_{uid}"),
            format!("input.Holaf_FX_{uid}"),
            format!("output.Holaf_FX_{uid}"),
        ] {
            assert!(
                nodes.iter().any(|n| n == &want),
                "missing node {want} in {nodes:?}"
            );
        }
    }
    // ... and no chain for the two strips without FX.
    assert!(
        !nodes.iter().any(|n| n.contains("Holaf_FX_s5")),
        "strip without FX must not grow a chain: {nodes:?}"
    );

    // --- teardown: SIGTERM → exit 0, no daemon, no FX host, no socket ------
    let killed = Command::new("kill")
        .args(["-TERM", &daemon_pid.to_string()])
        .status()
        .expect("kill -TERM");
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        assert!(Instant::now() < deadline, "the daemon ignored SIGTERM");
        std::thread::sleep(Duration::from_millis(20));
    };
    let logs = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(status.code(), Some(0), "clean exit expected; log:\n{logs}");
    assert!(
        pid_dead(host_pid, Duration::from_secs(10)),
        "the FX host (pid {host_pid}) outlived the daemon — that is an orphan process; \
         log:\n{logs}"
    );
    assert!(!socket.exists(), "the socket must be removed on shutdown");
    let _ = std::fs::remove_file(&log);
}

/// The FX host must also die when the daemon is **killed** (`SIGKILL`, no
/// chance to run any teardown): the child exits on EOF of its command pipe.
///
/// This is the property a `prctl(PR_SET_PDEATHSIG)` would give us — and it is
/// stronger, because it survives the daemon being unable to react at all.
#[test]
fn fx_host_dies_with_its_daemon_on_sigkill() {
    let _guard = live_lock();
    let _ = env_logger::try_init();
    if !live_allowed() {
        eprintln!("SKIP fx_host sigkill: no headless PipeWire configured");
        return;
    }
    fresh_sandbox_daemon();
    let socket = unique_socket("sigkill");
    let log = socket.with_extension("log");
    let mut child = spawn_daemon(&socket, &log);
    if !wait_for_socket(&socket, Duration::from_secs(20)) {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("SKIP fx_host sigkill: the daemon did not come up");
        return;
    }
    let daemon_pid = child.id();
    let mut client = Client::connect(&socket);
    let r = client.send(
        METHOD_APPLY_SNAPSHOT,
        json!({ "strips": [strip("k0", true)] }),
    );
    assert!(r.error.is_none(), "apply_snapshot failed: {:?}", r.error);
    let host_pid = client.status()["fx_host"]["pid"]
        .as_u64()
        .expect("fx_host.pid") as u32;
    assert!(pid_alive(host_pid));

    // Drop the client connection first so nobody notices the daemon dying.
    drop(client);
    let killed = Command::new("kill")
        .args(["-9", &daemon_pid.to_string()])
        .status()
        .expect("kill -9");
    assert!(killed.success());
    let _ = child.wait();
    assert!(
        pid_dead(host_pid, Duration::from_secs(10)),
        "the FX host (pid {host_pid}) survived a SIGKILLed daemon — its command pipe \
         should have hit EOF"
    );
    let _ = std::fs::remove_file(&log);
}
