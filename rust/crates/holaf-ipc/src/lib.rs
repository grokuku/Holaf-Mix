//! Holaf-Mix IPC protocol types (J3).
//!
//! The daemon and its clients speak one line-oriented JSON protocol over a
//! Unix socket (**NDJSON**: exactly one JSON value per line, `\n`-terminated,
//! per the rewrite spec §4).
//!
//! Wire envelopes:
//!
//! - [`Request`]  `{"id":1,"method":"…","params":{…}}`
//! - [`Response`] `{"id":1,"ok":true,"result":{…}}` or
//!   `{"id":1,"ok":false,"error":{"code":"…","message":"…"}}`
//! - [`Event`]    `{"event":"…","params":{…}}`
//!
//! Method and event names are exported as documented constants
//! ([`METHOD_APPLY_SNAPSHOT`], [`EVENT_STATE_CHANGED`], …) so both the daemon
//! dispatcher and the clients never typo a literal string.
//!
//! The crate is deliberately **pure** (serde + serde_json only): the concrete
//! payload *contents* (strips, effects, …) are typed in `holaf-core` and
//! deserialized by the daemon, keeping this crate usable from any consumer.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `Value` fallback for absent `params`/`result` envelopes: the spec always
/// shows an object, never `null`.
fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

// ---------------------------------------------------------------------------
// Method names (spec §4)
// ---------------------------------------------------------------------------

/// Replace the whole engine state (equivalent to `start_engine`).
///
/// Params: `{"strips":[{…Strip…}, …]}`.  The daemon runs every entry through
/// the `holaf-core` migrations before handing it to the backend, so legacy
/// configs (old port names, boolean effects, `Model` param) work unchanged.
pub const METHOD_APPLY_SNAPSHOT: &str = "engine.apply_snapshot";

/// Stop the engine and tear down the audio graph.
pub const METHOD_STOP: &str = "engine.stop";

/// Query the daemon state (running flag, strips, routes, devices, …).
pub const METHOD_STATUS: &str = "engine.status";

/// Set a strip volume.  Params: `{"uid":…,"value":0.0..1.0+}`.
pub const METHOD_STRIP_SET_VOLUME: &str = "strip.set_volume";

/// Mute/unmute a strip.  Params: `{"uid":…,"muted":bool}`.
pub const METHOD_STRIP_SET_MUTE: &str = "strip.set_mute";

/// Toggle stereo→mono downmix.  Params: `{"uid":…,"mono":bool}`.
pub const METHOD_STRIP_SET_MONO: &str = "strip.set_mono";

/// Enable/disable one effect on a strip.  Params:
/// `{"uid":…,"effect":"eq","active":bool}`.
pub const METHOD_FX_SET_ACTIVE: &str = "fx.set_active";

/// Hot-reload an effect's control values.  Params:
/// `{"uid":…,"effect":"eq","params":{"port":value,…}}`.
pub const METHOD_FX_SET_PARAMS: &str = "fx.set_params";

/// Add/remove an audio route.  Params:
/// `{"source_uid":…,"target_uid":…,"active":bool}`.
pub const METHOD_ROUTING_SET: &str = "routing.set";

/// List the current routing matrix.
pub const METHOD_ROUTING_LIST: &str = "routing.list";

/// Make a sink the system default.  Params: `{"uid":…}`.
pub const METHOD_SYSTEM_SET_DEFAULT_SINK: &str = "system.set_default_sink";

/// List physical audio devices known to the backend.
pub const METHOD_DEVICE_LIST: &str = "device.list";

/// List applications currently playing audio.
pub const METHOD_APP_LIST: &str = "app.list";

/// Move an application to another sink.  Params:
/// `{"app_id":…,"sink_name":…}`.
pub const METHOD_APP_MOVE: &str = "app.move";

/// Every method name implemented in the current protocol.
pub const ALL_METHODS: &[&str] = &[
    METHOD_APPLY_SNAPSHOT,
    METHOD_STOP,
    METHOD_STATUS,
    METHOD_STRIP_SET_VOLUME,
    METHOD_STRIP_SET_MUTE,
    METHOD_STRIP_SET_MONO,
    METHOD_FX_SET_ACTIVE,
    METHOD_FX_SET_PARAMS,
    METHOD_ROUTING_SET,
    METHOD_ROUTING_LIST,
    METHOD_SYSTEM_SET_DEFAULT_SINK,
    METHOD_DEVICE_LIST,
    METHOD_APP_LIST,
    METHOD_APP_MOVE,
];

// ---------------------------------------------------------------------------
// Event names (spec §4)
// ---------------------------------------------------------------------------

/// Anything relevant changed (engine start/stop, a strip was hot-reloaded…).
pub const EVENT_STATE_CHANGED: &str = "state_changed";

/// The list of physical devices changed (hotplug).
pub const EVENT_DEVICE_LIST_CHANGED: &str = "device_list_changed";

/// An effect chain finished building on a strip.  Params: `{"uid":…}`.
pub const EVENT_STRIP_FX_READY: &str = "strip_fx_ready";

/// Every event name defined in the current protocol.
pub const ALL_EVENTS: &[&str] = &[
    EVENT_STATE_CHANGED,
    EVENT_DEVICE_LIST_CHANGED,
    EVENT_STRIP_FX_READY,
];

// ---------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------

/// A client → daemon order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Caller-supplied correlation id, echoed back in the [`Response`].
    pub id: u64,
    /// One of the `METHOD_*` names.
    pub method: String,
    /// Method-specific payload; `{}` when the method takes no arguments.
    #[serde(default = "empty_object")]
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>) -> Self {
        Request {
            id,
            method: method.into(),
            params: Value::Object(Default::default()),
        }
    }

    pub fn with_params(id: u64, method: impl Into<String>, params: Value) -> Self {
        Request {
            id,
            method: method.into(),
            params,
        }
    }
}

/// Structured error attached to a failed [`Response`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    /// Stable machine-readable code, e.g. `"unknown_strip"`,
    /// `"invalid_params"`, `"parse_error"`, `"not_implemented"`.
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
}

impl ErrorInfo {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        ErrorInfo {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// A daemon → client reply.  Exactly one of `result`/`error` is meaningful,
/// selected by `ok`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Echo of the originating [`Request::id`].
    pub id: u64,
    /// `true` → success (see `result`); `false` → failure (see `error`).
    pub ok: bool,
    /// Success payload, omitted on error.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<Value>,
    /// Failure details, omitted on success.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<ErrorInfo>,
}

impl Response {
    /// Successful reply carrying `result` for request `id`.
    pub fn ok(id: u64, result: Value) -> Self {
        Response {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// Failed reply for request `id`.
    pub fn err(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Response {
            id,
            ok: false,
            result: None,
            error: Some(ErrorInfo::new(code, message)),
        }
    }
}

/// A daemon → client push notification (no id, no reply expected).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// One of the `EVENT_*` names.
    pub event: String,
    /// Event-specific payload; `{}` when there is none.
    #[serde(default = "empty_object")]
    pub params: Value,
}

impl Event {
    pub fn new(event: impl Into<String>, params: Value) -> Self {
        Event {
            event: event.into(),
            params,
        }
    }
}

// ---------------------------------------------------------------------------
// NDJSON helpers
// ---------------------------------------------------------------------------

/// Serialize `value` as a single NDJSON line (compact JSON + `\n`).
///
/// # Panics
/// Panics if `value` cannot be represented as JSON — in practice only
/// non-finite floats (`NaN`/`inf`).  The whole protocol layer guarantees
/// finite numbers end-to-end, so a panic here means a programming error
/// rather than a legitimate payload.
pub fn encode<T: Serialize + ?Sized>(value: &T) -> String {
    let mut out = serde_json::to_string(value).expect("IPC payload must be JSON-serializable");
    out.push('\n');
    out
}

/// Deserialize one NDJSON line (leading/trailing whitespace tolerated).
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trip() {
        let req = Request::with_params(
            7,
            METHOD_STRIP_SET_VOLUME,
            json!({"uid": "mic-1", "value": 0.5}),
        );
        let line = encode(&req);
        assert!(line.ends_with('\n'), "NDJSON line must end with \\n");
        let back: Request = decode_line(line.trim_end_matches('\n')).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn request_defaults_params_to_empty_object() {
        let line = r#"{"id":3,"method":"engine.stop"}"#;
        let req: Request = decode_line(line).unwrap();
        assert_eq!(req.id, 3);
        assert_eq!(req.method, METHOD_STOP);
        assert_eq!(req.params, json!({}));
    }

    #[test]
    fn response_ok_round_trip_and_shape() {
        let resp = Response::ok(1, json!({"applied": 2}));
        let json_text = serde_json::to_string(&resp).unwrap();
        // `error` must be omitted entirely on success.
        assert_eq!(json_text, r#"{"id":1,"ok":true,"result":{"applied":2}}"#);

        let back: Response = decode_line(&json_text).unwrap();
        assert_eq!(back, resp);
        assert!(back.ok);
        assert_eq!(back.result, Some(json!({"applied": 2})));
        assert_eq!(back.error, None);
    }

    #[test]
    fn response_err_round_trip_and_shape() {
        let resp = Response::err(1, "unknown_strip", "no strip with uid 'x'");
        let json_text = serde_json::to_string(&resp).unwrap();
        // `result` must be omitted entirely on error.
        assert_eq!(
            json_text,
            r#"{"id":1,"ok":false,"error":{"code":"unknown_strip","message":"no strip with uid 'x'"}}"#
        );

        let back: Response = decode_line(&json_text).unwrap();
        assert_eq!(back, resp);
        assert!(!back.ok);
        assert_eq!(back.error.unwrap().code, "unknown_strip");
    }

    #[test]
    fn response_helpers_are_consistent() {
        let ok = Response::ok(42, json!({"x": 1}));
        assert!(ok.ok);
        assert!(ok.result.is_some());
        assert!(ok.error.is_none());
        assert_eq!(ok.id, 42);

        let err = Response::err(42, "boom", "kaboom");
        assert!(!err.ok);
        assert!(err.result.is_none());
        assert_eq!(err.error.as_ref().unwrap().message, "kaboom");
        assert_eq!(err.id, 42);
    }

    #[test]
    fn event_round_trip() {
        let ev = Event::new(EVENT_STRIP_FX_READY, json!({"uid": "mic-1"}));
        let line = encode(&ev);
        let back: Event = decode_line(line.trim_end_matches('\n')).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn decode_line_rejects_malformed_json() {
        assert!(decode_line::<Request>("not json").is_err());
        assert!(decode_line::<Request>("").is_err());
        assert!(decode_line::<Request>(r#"{"id":1}"#).is_err()); // missing method
    }

    #[test]
    fn method_and_event_names_are_documented_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for m in ALL_METHODS {
            assert!(seen.insert(*m), "duplicate method {m}");
        }
        assert_eq!(ALL_METHODS.len(), 14);

        let mut seen = std::collections::HashSet::new();
        for e in ALL_EVENTS {
            assert!(seen.insert(*e), "duplicate event {e}");
        }
        assert_eq!(ALL_EVENTS.len(), 3);
        assert_eq!(METHOD_APPLY_SNAPSHOT, "engine.apply_snapshot");
        assert_eq!(EVENT_DEVICE_LIST_CHANGED, "device_list_changed");
    }
}
