//! Parent side of the FX host: spawn it, talk NDJSON to it, notice when it
//! dies.
//!
//! The client is a **dumb transport**: it owns the child process and one
//! request/reply round trip.  Everything about *which* chains should exist
//! (and therefore what to restore after a respawn) belongs to
//! [`crate::chainhost::ChainHost`], which is the piece the engine drives.
//!
//! # Why a reader thread
//!
//! Replies arrive on a pipe, and `std` gives pipes no read timeout.  A blocked
//! read on the daemon's worker thread would freeze the whole graph (the worker
//! is what pumps the mainloop), so a small thread drains the child's stdout
//! into a channel and the request does a bounded `recv_timeout`.  That thread
//! touches nothing but the channel — no PipeWire, no locks shared with the
//! engine.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use holaf_core::backend::BackendError;

use super::proto::{parse_reply, FxCommand, FxReply, FxRequest};

/// How long one command may take before the host is declared wedged.
///
/// Deliberately short: every host operation is local (`pw_context_load_module`
/// dlopens a few LADSPA plugins, `set_param` marshals a pod), so a host that
/// does not answer in seconds is broken, and waiting longer only stalls the
/// daemon's reconciler.
pub const HOST_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a `stop` may take before the child is killed outright.
const HOST_STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the child gets to answer the startup `ping`.
///
/// Short on purpose: a wrong or broken binary must be detected while the engine
/// is still building (so it can fall back to in-process hosting) and not when
/// the first chain has to be loaded.
const HOST_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

/// The two stream nodes a loaded chain owns (as reported by the host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainInfo {
    /// The strip uid the chain belongs to.
    pub uid: String,
    /// `input.Holaf_FX_<uid>` — the chain's capture stream (virtual sink).
    pub input_node: Option<String>,
    /// `output.Holaf_FX_<uid>` — the chain's playback stream.
    pub output_node: Option<String>,
}

/// A running FX host process.
pub struct FxHostClient {
    child: Child,
    stdin: Option<ChildStdin>,
    replies: Receiver<String>,
    next_id: u64,
    program: String,
}

fn host_down(detail: &str) -> BackendError {
    BackendError::new("fx_host_down", format!("the FX host is gone: {detail}"))
}

/// Which executable to run as `--fx-host`.
///
/// Order: `$HOLAFD_BIN` (packaging escape hatch) → ourselves (the normal case)
/// → a sibling `holafd` (test binaries live in `target/debug/deps/`, the daemon
/// in `target/debug/`, so this makes the live tests drive the real host).
pub fn resolve_host_program() -> Result<std::path::PathBuf, BackendError> {
    if let Some(p) = std::env::var_os("HOLAFD_BIN") {
        if !p.is_empty() {
            return Ok(std::path::PathBuf::from(p));
        }
    }
    let exe = std::env::current_exe()
        .map_err(|e| BackendError::internal("resolve the holafd executable", e))?;
    let is_holafd = exe
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == "holafd" || n == "holafd.exe")
        .unwrap_or(false);
    if is_holafd {
        return Ok(exe);
    }
    if let Some(dir) = exe.parent() {
        for candidate in [dir.join("holafd"), dir.join("..").join("holafd")] {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Ok(exe)
}

/// Map a host-reported error code onto the daemon's **closed** error-code
/// space.
///
/// `BackendError` carries `&'static str` codes on purpose (clients switch on
/// them), so a remote code is either one we already publish or
/// `fx_host_error`: the host can never invent a code a client does not know.
fn host_code(code: &str) -> &'static str {
    match code {
        "pw_load_failed" => "pw_load_failed",
        "chain_gone" => "chain_gone",
        "unknown_chain" => "unknown_chain",
        "bad_request" => "fx_host_bad_request",
        _ => "fx_host_error",
    }
}

fn host_timeout() -> BackendError {
    BackendError::new(
        "fx_host_timeout",
        "the FX host did not answer within the request timeout",
    )
}

impl FxHostClient {
    /// Spawn the `holafd` binary as `--fx-host`, then verify it really answers.
    ///
    /// The binary is [`resolve_host_program`]: `$HOLAFD_BIN`, else this very
    /// executable (the normal case — no extra artifact to ship), else a
    /// sibling `holafd` (which is what makes the *live tests*, running inside a
    /// `cargo test` binary, exercise the real dedicated host instead of falling
    /// back).
    ///
    /// The `ping` handshake turns "the process started" into "the host works",
    /// so a wrong binary (or one without `--fx-host`) is reported as a spawn
    /// failure and [`crate::chainhost::ChainHost::from_mode`] can fall back
    /// instead of leaving the graph chain-less.
    pub fn spawn() -> Result<FxHostClient, BackendError> {
        let exe = resolve_host_program()?;
        let mut client = FxHostClient::spawn_with(&exe, &["--fx-host"])?;
        if let Err(e) = client.request_with_timeout(FxCommand::Ping, HOST_HANDSHAKE_TIMEOUT) {
            let pid = client.pid();
            client.shutdown();
            return Err(BackendError::new(
                "fx_host_unusable",
                format!(
                    "'{}' did not answer --fx-host (pid {pid}): {}",
                    exe.display(),
                    e.message
                ),
            ));
        }
        Ok(client)
    }

    /// Spawn `program args…` as an FX host (used by `spawn` and by the tests,
    /// which drive a scripted fake host).
    pub fn spawn_with(program: &Path, args: &[&str]) -> Result<FxHostClient, BackendError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Keep the host's log lines in the daemon's log stream: a host that
            // crashes must be diagnosable without hunting for a file.
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|e| {
            BackendError::internal(
                "spawn the FX host",
                format!("{}: {e}", program.display()),
            )
        })?;
        let stdin = child.stdin.take();
        let stdout: ChildStdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = child.kill();
                return Err(host_down("no stdout pipe"));
            }
        };
        let (tx, replies) = mpsc::channel::<String>();
        thread::Builder::new()
            .name("holaf-fxhost-rx".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) => {
                            if tx.send(l).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
                // EOF: the child exited.  Dropping `tx` disconnects the channel,
                // which is how `request` reports the death.
            })
            .map_err(|e| BackendError::internal("spawn the FX host reader", e))?;

        log::info!("FX host started: {} (pid {})", program.display(), child.id());
        Ok(FxHostClient {
            child,
            stdin,
            replies,
            next_id: 1,
            program: program.display().to_string(),
        })
    }

    /// OS pid of the host (diagnostics / `ps` cross-check).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Executable used for this host (diagnostics).
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Is the child still running *and* still answering on its stdout?
    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_)) => false, // exited
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// One request/reply round trip, bounded by [`HOST_REQUEST_TIMEOUT`].
    pub fn request(&mut self, command: FxCommand) -> Result<FxReply, BackendError> {
        self.request_with_timeout(command, HOST_REQUEST_TIMEOUT)
    }

    /// One request/reply round trip, bounded by `timeout`.
    pub fn request_with_timeout(
        &mut self,
        command: FxCommand,
        timeout: Duration,
    ) -> Result<FxReply, BackendError> {
        let id = self.next_id;
        self.next_id += 1;
        let line = FxRequest::new(id, command).to_line();
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| host_down("its command pipe is closed"))?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| host_down(&format!("cannot write command: {e}")))?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(host_timeout());
            }
            match self.replies.recv_timeout(remaining) {
                Ok(text) => {
                    let reply = parse_reply(&text).map_err(|e| {
                        BackendError::new(
                            "fx_host_protocol",
                            format!("unparsable reply {text:?}: {e}"),
                        )
                    })?;
                    if reply.id != id {
                        // A late reply to an abandoned request: ignore it
                        // rather than mis-attributing it.
                        log::warn!(
                            "FX host replied id {} while waiting for {id} — ignoring",
                            reply.id
                        );
                        continue;
                    }
                    return Ok(reply);
                }
                Err(RecvTimeoutError::Timeout) => return Err(host_timeout()),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(host_down("its stdout was closed (process exited)"))
                }
            }
        }
    }

    /// Turn a failed reply into an error, keeping the host's own code.
    fn ok(mut reply: FxReply) -> Result<FxReply, BackendError> {
        if reply.ok {
            Ok(reply)
        } else {
            // Take the error out of the reply so the borrow cannot outlive it.
            match reply.error.take() {
                Some(e) => Err(BackendError::new(host_code(&e.code), e.message)),
                None => Err(BackendError::new(
                    "fx_host_error",
                    "the FX host reported a failure without details",
                )),
            }
        }
    }

    /// Load one chain; returns the stream node names the host created.
    pub fn load_chain(
        &mut self,
        uid: &str,
        config: &str,
        anchor_node_id: Option<u32>,
    ) -> Result<ChainInfo, BackendError> {
        let reply = self.request(FxCommand::LoadChain {
            uid: uid.to_string(),
            config: config.to_string(),
            anchor_node_id,
        })?;
        let reply = Self::ok(reply)?;
        let chain = reply.get("chain").cloned().unwrap_or(Value::Null);
        let node = |key: &str| {
            chain
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Ok(ChainInfo {
            uid: uid.to_string(),
            input_node: node("input_node"),
            output_node: node("output_node"),
        })
    }

    /// Unload one chain.  Idempotent on the host side.
    pub fn unload_chain(&mut self, uid: &str) -> Result<(), BackendError> {
        let reply = self.request(FxCommand::UnloadChain {
            uid: uid.to_string(),
        })?;
        Self::ok(reply).map(|_| ())
    }

    /// Unload every chain hosted here.
    pub fn unload_all(&mut self) -> Result<(), BackendError> {
        let reply = self.request(FxCommand::UnloadAll)?;
        Self::ok(reply).map(|_| ())
    }

    /// Hot-reload the flat `Props.params` of a chain.
    pub fn set_params(
        &mut self,
        uid: &str,
        props: &[(String, f32)],
    ) -> Result<(), BackendError> {
        let mut map = indexmap::IndexMap::new();
        for (name, value) in props {
            map.insert(name.clone(), *value);
        }
        let reply = self.request(FxCommand::SetParams {
            uid: uid.to_string(),
            props: map,
        })?;
        Self::ok(reply).map(|_| ())
    }

    /// Liveness probe.
    pub fn ping(&mut self) -> Result<(), BackendError> {
        let reply = self.request(FxCommand::Ping)?;
        Self::ok(reply).map(|_| ())
    }

    /// Host self-report (`chains`, `pid`, `uptime_ms`).
    pub fn status(&mut self) -> Result<Value, BackendError> {
        let reply = self.request(FxCommand::Status)?;
        let reply = Self::ok(reply)?;
        Ok(Value::Object(reply.data))
    }

    /// Ask the host to exit, then reap it.  Best effort: a wedged host is
    /// killed, a dead one is ignored.
    pub fn shutdown(mut self) {
        if self.stdin.is_some() {
            let _ = self.request(FxCommand::Stop);
        }
        self.stdin = None; // close the pipe → EOF → the host leaves its loop
        let deadline = Instant::now() + HOST_STOP_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(_) => break,
            }
            if Instant::now() >= deadline {
                log::warn!("FX host (pid {}) did not stop in time — killing it", self.pid());
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FxHostClient {
    fn drop(&mut self) {
        // Never leave an orphan behind, even on an early return / panic path.
        self.stdin = None;
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A scripted fake host: answers `{"id":N,"ok":true}` to every line and
    /// appends each `cmd` it saw to `$log`.
    ///
    /// `die_after` > 0 makes it exit after that many commands, which is how the
    /// respawn behaviour is tested without a PipeWire server.
    /// Returned script is run as `sh <script>`, never exec'd directly: a file
    /// that is still open for writing in this process makes `execve` fail with
    /// `ETXTBSY` (a real flake when several tests fork concurrently).
    fn fake_host(dir: &Path, log: &Path, die_after: u32) -> PathBuf {
        let path = dir.join("fake-fxhost.sh");
        let script = format!(
            "\
             n=0\n\
             while IFS= read -r line; do\n\
               n=$((n+1))\n\
               cmd=$(printf '%s' \"$line\" | sed -n 's/.*\"cmd\":\"\\([a-z_]*\\)\".*/\\1/p')\n\
               id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')\n\
               printf '%s\\n' \"$cmd\" >> {log}\n\
               printf '{{\"id\":%s,\"ok\":true,\"chain\":{{\"uid\":\"x\",\"input_node\":\"input.Holaf_FX_x\",\"output_node\":\"output.Holaf_FX_x\"}}}}\\n' \"$id\"\n\
               if [ {die} -gt 0 ] && [ \"$n\" -ge {die} ]; then exit 0; fi\n\
             done\n",
            log = log.display(),
            die = die_after
        );
        fs::write(&path, script).expect("write fake host");
        path
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("holaf-fxhost-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("mkdir");
        p
    }

    #[test]
    fn load_chain_round_trips_over_ndjson() {
        let dir = tmpdir("load");
        let log = dir.join("cmds.log");
        let host = fake_host(&dir, &log, 0);
        let mut client = FxHostClient::spawn_with(Path::new("/bin/sh"), &[host.to_str().unwrap()])
            .expect("spawn");
        let info = client
            .load_chain("main", "{ node.name = \"Holaf_FX_main\" }", Some(7))
            .expect("load_chain");
        assert_eq!(info.uid, "main");
        assert_eq!(info.input_node.as_deref(), Some("input.Holaf_FX_x"));
        assert_eq!(info.output_node.as_deref(), Some("output.Holaf_FX_x"));
        client.shutdown();
        let seen = fs::read_to_string(&log).unwrap_or_default();
        assert!(seen.contains("load_chain"), "host saw: {seen:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_host_is_reported_as_down() {
        let dir = tmpdir("dead");
        let log = dir.join("cmds.log");
        let host = fake_host(&dir, &log, 1); // exits after the first command
        let mut client = FxHostClient::spawn_with(Path::new("/bin/sh"), &[host.to_str().unwrap()])
            .expect("spawn");
        client.load_chain("a", "{}", None).expect("first load");
        // Let the child exit and the reader thread see EOF.
        let err = loop {
            thread::sleep(Duration::from_millis(20));
            match client.load_chain("b", "{}", None) {
                Ok(_) => continue,
                Err(e) => break e,
            }
        };
        assert_eq!(err.code, "fx_host_down", "got {err:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_failure_is_reported() {
        let err = match FxHostClient::spawn_with(Path::new("/nonexistent/fx-host"), &[]) {
            Ok(_) => panic!("spawning a nonexistent binary must fail"),
            Err(e) => e,
        };
        assert_eq!(err.code, "internal_error");
    }

    #[test]
    fn host_error_replies_keep_their_code() {
        let dir = tmpdir("err");
        let script = dir.join("err-host.sh");
        fs::write(
            &script,
            "read -r line\nid=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')\n\
             printf '{\"id\":%s,\"ok\":false,\"error\":{\"code\":\"pw_load_failed\",\"message\":\"boom\"}}\\n' \"$id\"\n",
        )
        .expect("write");
        let mut client =
            FxHostClient::spawn_with(Path::new("/bin/sh"), &[script.to_str().unwrap()])
                .expect("spawn");
        let err = client.load_chain("a", "{}", None).expect_err("must fail");
        assert_eq!(err.code, "pw_load_failed");
        assert_eq!(err.message, "boom");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unresponsive_host_times_out() {
        let dir = tmpdir("hang");
        let script = dir.join("hang-host.sh");
        fs::write(&script, "sleep 60\n").expect("write");
        let mut client =
            FxHostClient::spawn_with(Path::new("/bin/sh"), &[script.to_str().unwrap()])
                .expect("spawn");
        // A short timeout keeps the test fast; the production value is 10 s.
        let started = Instant::now();
        let err = client
            .request_with_timeout(FxCommand::Ping, Duration::from_millis(150))
            .expect_err("must time out");
        assert_eq!(err.code, "fx_host_timeout");
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_reaps_the_child() {
        let dir = tmpdir("reap");
        let log = dir.join("cmds.log");
        let host = fake_host(&dir, &log, 0);
        let mut client = FxHostClient::spawn_with(Path::new("/bin/sh"), &[host.to_str().unwrap()])
            .expect("spawn");
        let pid = client.pid();
        assert!(client.is_alive());
        client.ping().expect("ping");
        client.shutdown();
        // `shutdown` reaps the child: no zombie and no /proc entry left.
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "pid {pid} is still around after shutdown"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
