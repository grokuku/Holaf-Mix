//! End-to-end IPC integration tests: a real `holafd::Server` on a unique temp
//! Unix socket, exercised with raw NDJSON lines from in-process clients
//! backed by a [`FakeBackend`].
//!
//! These tests talk the exact same wire protocol `holafctl` uses, so they
//! cover the NDJSON framing, per-connection reader/writer threads, dispatch,
//! error mapping and event broadcast — no PipeWire involved.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use holaf_ipc::{decode_line, encode, Event, Request, Response, METHOD_FX_SET_ACTIVE,
    METHOD_STATUS, METHOD_STRIP_SET_VOLUME};
use serde_json::{json, Value};

use holafd::{FakeBackend, Server};

/// A unique socket path per test (process + nanos) so parallel tests never
/// collide.
fn unique_socket(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "holafd-it-{tag}-{}-{nanos}.sock",
        std::process::id()
    ))
}

/// A tiny NDJSON client that keeps one connection open and correlates
/// responses by id, skipping push events.
struct TestClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl TestClient {
    fn connect(path: &std::path::Path) -> TestClient {
        let stream = UnixStream::connect(path).expect("connect to test server");
        let reader = BufReader::new(stream.try_clone().expect("clone stream"));
        TestClient {
            reader,
            writer: stream,
            next_id: 1,
        }
    }

    /// Send one request and read the matching response (skipping events).
    fn send(&mut self, method: &str, params: Value) -> Response {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request::with_params(id, method, params);
        self.writer
            .write_all(encode(&request).as_bytes())
            .expect("write request");
        self.read_response(id)
    }

    fn read_response(&mut self, id: u64) -> Response {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self
                .reader
                .read_line(&mut line)
                .expect("read response line");
            assert!(n > 0, "server closed the connection while awaiting id {id}");
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = decode_line(line.trim()).expect("response is valid JSON");
            if value.get("event").is_some() {
                continue; // push event interleaved with our response
            }
            let response: Response = serde_json::from_value(value).expect("response envelope");
            if response.id == id {
                return response;
            }
        }
    }

    /// Read the next line and require it to be a push event with `name`.
    fn expect_event(&mut self, name: &str) -> Value {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).expect("read event line");
            assert!(n > 0, "server closed before the event arrived");
            if line.trim().is_empty() {
                continue;
            }
            let event: Event = decode_line(line.trim()).expect("event is valid JSON");
            if event.event == name {
                return event.params;
            }
        }
    }
}

/// Run a closure against a live server backed by a fresh FakeBackend.
fn run_server<T>(path: &std::path::Path, body: impl FnOnce(&Server) -> T) -> T {
    let server = Server::bind(path, FakeBackend::new()).expect("bind server");
    let shutdown = Arc::new(AtomicBool::new(false));
    let result = std::thread::scope(|scope| {
        let handle = scope.spawn(|| server.run(Arc::clone(&shutdown)).unwrap());
        let out = body(&server);
        shutdown.store(true, Ordering::Relaxed);
        handle.join().expect("server loop must stop cleanly");
        out
    });
    let _ = std::fs::remove_file(path);
    result
}

/// A simple, migrated input strip for apply_snapshot payloads.
fn sample_strips_payload() -> Value {
    json!([
        {
            "uid": "in-1",
            "label": "Micro",
            "kind": "input",
            "volume": 1.0,
            "mute": false,
            "is_mono": false,
            "routes": [],
            "device_name": null,
            "is_default": false,
            "effects": {
                "gate": true,
                "eq": { "active": true, "params": { "50Hz": -6.0, "20000Hz": 2.0 } },
                "tube": { "active": false, "params": { "Distortion level": 3.0 } }
            }
        }
    ])
}

#[test]
fn full_sequence_over_socket() {
    let path = unique_socket("sequence");
    run_server(&path, |_server| {
        let mut client = TestClient::connect(&path);

        // 1. apply_snapshot with a *legacy* config: the daemon must migrate it
        // (short EQ band, boolean gate, old tube name) before storing.
        let resp = client.send("engine.apply_snapshot", json!({ "strips": sample_strips_payload() }));
        assert!(resp.ok, "apply_snapshot failed: {resp:?}");
        assert_eq!(resp.result.unwrap()["applied"], 1);

        // 2. volume + fx tweaks.
        let resp = client.send(METHOD_STRIP_SET_VOLUME, json!({ "uid": "in-1", "value": 0.5 }));
        assert!(resp.ok);
        assert_eq!(resp.result.unwrap()["value"], 0.5);

        let resp = client.send(
            METHOD_FX_SET_ACTIVE,
            json!({ "uid": "in-1", "effect": "compressor", "active": true }),
        );
        assert!(resp.ok, "fx_set_active failed: {resp:?}");
        assert_eq!(resp.result.unwrap()["active"], true);

        // 3. status must reflect everything, including the migrations.
        let resp = client.send(METHOD_STATUS, json!({}));
        assert!(resp.ok);
        let strips = resp.result.unwrap()["strips"].as_array().unwrap().clone();
        assert_eq!(strips.len(), 1);
        let strip = &strips[0];
        assert_eq!(strip["uid"], "in-1");
        assert_eq!(strip["volume"], 0.5);
        assert_eq!(strip["effects"]["gate"]["active"], true, "legacy bool gate migrated");
        assert_eq!(
            strip["effects"]["eq"]["params"]["50Hz gain (low shelving)"],
            -6.0,
            "legacy short EQ band renamed by the daemon"
        );
        assert_eq!(strip["effects"]["tube"]["params"]["Drive"], 3.0);
        assert_eq!(strip["effects"]["compressor"]["active"], true);
    });
}

#[test]
fn multiple_requests_on_one_connection_are_ordered() {
    let path = unique_socket("ordered");
    run_server(&path, |_server| {
        let mut client = TestClient::connect(&path);
        let resp = client.send(
            "engine.apply_snapshot",
            json!({ "strips": json!([{ "uid": "a", "label": "A", "kind": "output" }]) }),
        );
        assert!(resp.ok);

        // Fire several orders back-to-back; responses must come in order.
        let id0 = client.send(METHOD_STATUS, json!({}));
        let id1 = client.send(METHOD_STRIP_SET_VOLUME, json!({ "uid": "a", "value": 0.25 }));
        let id2 = client.send(METHOD_STRIP_SET_VOLUME, json!({ "uid": "a", "value": 0.5 }));
        let id3 = client.send(METHOD_STATUS, json!({}));

        assert!(id0.ok && id1.ok && id2.ok && id3.ok);
        let strips_after = id3.result.unwrap()["strips"].as_array().unwrap().clone();
        assert_eq!(strips_after[0]["volume"], 0.5);
    });
}

#[test]
fn unknown_method_and_invalid_params_and_unknown_strip_over_socket() {
    let path = unique_socket("errors");
    run_server(&path, |_server| {
        let mut client = TestClient::connect(&path);
        client.send("engine.apply_snapshot", json!({ "strips": json!([]) }));
        let client = &mut client;

        // Unknown method.
        let resp = client.send("engine.nope", json!({}));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "unknown_method");

        // Well-formed envelope, malformed params (volume missing `value`).
        let resp = client.send(METHOD_STRIP_SET_VOLUME, json!({ "uid": "x" }));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "invalid_params");

        // Valid params, unknown strip.
        let resp = client.send(METHOD_STRIP_SET_VOLUME, json!({ "uid": "ghost", "value": 0.5 }));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "unknown_strip");
    });
}

#[test]
fn malformed_json_line_gets_parse_error_with_id_zero() {
    let path = unique_socket("malformed");
    run_server(&path, |_server| {
        let mut client = TestClient::connect(&path);
        client
            .writer
            .write_all(b"{ this is not json at all\n")
            .expect("write malformed line");
        let resp = client.read_response(0);
        assert!(!resp.ok);
        assert_eq!(resp.id, 0);
        assert_eq!(resp.error.unwrap().code, "parse_error");

        // The connection must stay usable afterwards.
        client.send("engine.apply_snapshot", json!({ "strips": json!([]) }));
        let ok = client.send(METHOD_STATUS, json!({}));
        assert!(ok.ok);
    });
}

#[test]
fn push_events_are_broadcast_to_all_connected_clients() {
    let path = unique_socket("broadcast");
    run_server(&path, |server| {
        let mut client_a = TestClient::connect(&path);
        let mut client_b = TestClient::connect(&path);

        // Round-trips guarantee both clients are registered with the server
        // before we broadcast.
        assert!(client_a.send(METHOD_STATUS, json!({})).ok);
        assert!(client_b.send(METHOD_STATUS, json!({})).ok);

        let delivered = server.notify(&Event::new("state_changed", json!({ "why": "test" })));
        assert_eq!(delivered, 2, "both clients must receive the broadcast");

        assert_eq!(client_a.expect_event("state_changed")["why"], "test");
        assert_eq!(client_b.expect_event("state_changed")["why"], "test");

        // Second broadcast reaches a client that only connected earlier.
        let delivered = server.notify(&Event::new("strip_fx_ready", json!({ "uid": "in-1" })));
        assert_eq!(delivered, 2);
        assert_eq!(client_a.expect_event("strip_fx_ready")["uid"], "in-1");
    });
}

#[test]
fn unimplemented_backend_reports_not_implemented_over_the_wire() {
    let path = unique_socket("unimplemented");
    let server = Server::bind_boxed(&path, Box::new(holafd::UnimplementedBackend)).unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| server.run(Arc::clone(&shutdown)).unwrap());
        let mut client = TestClient::connect(&path);
        let resp = client.send(METHOD_STATUS, json!({}));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "not_implemented");
        shutdown.store(true, Ordering::Relaxed);
        handle.join().unwrap();
    });
    let _ = std::fs::remove_file(&path);
}
