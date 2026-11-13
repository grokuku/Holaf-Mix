//! NDJSON protocol between `holafd` (parent) and its dedicated FX host child.
//!
//! One JSON object per line, in both directions.  Requests carry an `id` that
//! the reply echoes, so the parent can pipe several commands and match the
//! answers (`holafd` sends them strictly one at a time and blocks on the reply,
//! but the protocol does not depend on that).
//!
//! ```text
//! → {"id":1,"cmd":"load_chain","uid":"main","config":"{ node.name = … }","anchor_node_id":42}
//! ← {"id":1,"ok":true,"chain":{"uid":"main","input_node":"input.Holaf_FX_main",
//!                              "output_node":"output.Holaf_FX_main"}}
//! → {"id":2,"cmd":"unload_chain","uid":"main"}
//! ← {"id":2,"ok":true}
//! → {"id":3,"cmd":"ping"}
//! ← {"id":3,"ok":true,"pong":true}
//! → {"id":4,"cmd":"status"}
//! ← {"id":4,"ok":true,"chains":["main"],"pid":1234}
//! → {"id":5,"cmd":"stop"}
//! ← {"id":5,"ok":true}
//! ```
//!
//! Every reply is either `{"id":N,"ok":true,…}` or
//! `{"id":N,"ok":false,"error":{"code":"…","message":"…"}}`.  A malformed line
//! still gets a reply (`bad_request`), so the parent never hangs on a typo.
//!
//! This module is **pure** (no PipeWire): parsing and encoding are unit-tested
//! without a daemon or a child process.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// One command sent by `holafd` to its FX host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum FxCommand {
    /// Load a filter-chain module and report the two stream nodes it created.
    LoadChain {
        /// Strips' uid — the host keys its module handles by it.
        uid: String,
        /// The `libpipewire-module-filter-chain` argument string, built by
        /// [`crate::engine::ChainSpec`] on the parent side (single source of
        /// truth: the host is deliberately dumb and never re-derives it).
        config: String,
        /// Global id of the node the chain is anchored to (`target.object`).
        ///
        /// Informational: the config already carries the anchor *name*; the
        /// host only uses it to warn when the anchor is not (yet) in its
        /// registry view, which is the exact situation that used to produce a
        /// chain wired to the wrong sink.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor_node_id: Option<u32>,
    },
    /// Unload one chain (destroy the module, which removes both streams).
    UnloadChain {
        /// Strip uid whose chain must go.
        uid: String,
    },
    /// Unload every chain (used on teardown and before a respawn).
    UnloadAll,
    /// Hot-reload the flat `Props.params` of a chain (name → value pairs).
    SetParams {
        /// Strip uid whose `input.Holaf_FX_<uid>` receives the params.
        uid: String,
        /// `"<node>:<port>"` → value, i.e. the flat struct
        /// `module-filter-chain` reads (`set_control_value`).
        props: IndexMap<String, f32>,
    },
    /// Liveness probe (also used as a cheap "are you still in sync" check).
    Ping,
    /// Report the chains currently hosted.
    Status,
    /// Reply `ok` then exit(0).
    Stop,
}

impl FxRequest {
    /// Pair a command with a request id.
    pub fn new(id: u64, command: FxCommand) -> FxRequest {
        FxRequest { id, command }
    }

    /// Encode as one line **including** the trailing newline.
    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_else(|_e| {
            // Serialization of our own types cannot fail; keep the host alive
            // with a well-formed (if useless) request rather than a panic.
            format!("{{\"id\":{},\"cmd\":\"ping\"}}", self.id)
        });
        debug_assert!(!line.contains('\n'), "a request must stay on one line");
        line.push('\n');
        line
    }
}

impl FxCommand {
    /// Stable machine-readable name (logs, tests).
    pub fn name(&self) -> &'static str {
        match self {
            FxCommand::LoadChain { .. } => "load_chain",
            FxCommand::UnloadChain { .. } => "unload_chain",
            FxCommand::UnloadAll => "unload_all",
            FxCommand::SetParams { .. } => "set_params",
            FxCommand::Ping => "ping",
            FxCommand::Status => "status",
            FxCommand::Stop => "stop",
        }
    }
}

/// One line read from the parent: an id plus a command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxRequest {
    /// Echoed back in the reply.  `0` when the parent omits it.
    #[serde(default)]
    pub id: u64,
    /// The command itself (flattened: `cmd` lives at the top level).
    #[serde(flatten)]
    pub command: FxCommand,
}

/// The `error` object of a failed reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FxErrorInfo {
    /// Stable code (`bad_request`, `pw_load_failed`, `unknown_chain`, …).
    pub code: String,
    /// Human-readable detail.
    pub message: String,
}

/// One line sent back to the parent.
#[derive(Debug, Clone, PartialEq)]
pub struct FxReply {
    /// Id of the request this answers.
    pub id: u64,
    /// Whether the command succeeded.
    pub ok: bool,
    /// Error detail when `ok == false`.
    pub error: Option<FxErrorInfo>,
    /// Command-specific payload (`chain`, `chains`, `pid`, …).
    pub data: Map<String, Value>,
}

impl FxReply {
    /// Successful reply carrying `data` (merged at the top level).
    pub fn ok(id: u64, data: Map<String, Value>) -> FxReply {
        FxReply {
            id,
            ok: true,
            error: None,
            data,
        }
    }

    /// Successful reply with no payload.
    pub fn done(id: u64) -> FxReply {
        FxReply::ok(id, Map::new())
    }

    /// Failed reply.
    pub fn err(id: u64, code: &str, message: impl Into<String>) -> FxReply {
        FxReply {
            id,
            ok: false,
            error: Some(FxErrorInfo {
                code: code.to_string(),
                message: message.into(),
            }),
            data: Map::new(),
        }
    }

    /// Encode as one line **including** the trailing newline.
    ///
    /// The payload is merged first and the envelope (`id`, `ok`, `error`)
    /// overwritten last, so a payload can never forge the header.
    pub fn to_line(&self) -> String {
        let mut obj = self.data.clone();
        obj.insert("id".to_string(), json!(self.id));
        obj.insert("ok".to_string(), json!(self.ok));
        match &self.error {
            Some(e) => {
                obj.insert(
                    "error".to_string(),
                    json!({ "code": e.code, "message": e.message }),
                );
            }
            None => {
                obj.remove("error");
            }
        }
        let mut line = Value::Object(obj).to_string();
        line.push('\n');
        line
    }

    /// Error code, `None` when the reply is successful.
    pub fn error_code(&self) -> Option<&str> {
        self.error.as_ref().map(|e| e.code.as_str())
    }

    /// Read one field of the payload.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }
}

/// Parse one request line; on failure return a **ready-to-send** error reply.
///
/// The id is recovered best-effort from the raw text so the parent can match
/// the failure with its request instead of timing out.
pub fn parse_request(line: &str) -> Result<FxRequest, FxReply> {
    match serde_json::from_str::<FxRequest>(line) {
        Ok(req) => Ok(req),
        Err(e) => {
            let id = guess_id(line);
            Err(FxReply::err(id, "bad_request", e.to_string()))
        }
    }
}

/// Parse one reply line coming from the FX host.
///
/// Unknown top-level keys are kept in [`FxReply::data`] so a future host can
/// add fields without breaking this side.
pub fn parse_reply(line: &str) -> Result<FxReply, String> {
    #[derive(Deserialize)]
    struct RawReply {
        #[serde(default)]
        id: u64,
        #[serde(default)]
        ok: bool,
        #[serde(default)]
        error: Option<FxErrorInfo>,
        #[serde(flatten)]
        data: Map<String, Value>,
    }
    let raw: RawReply = serde_json::from_str(line).map_err(|e| e.to_string())?;
    Ok(FxReply {
        id: raw.id,
        ok: raw.ok,
        error: raw.error,
        data: raw.data,
    })
}

/// Best-effort `"id":N` extraction from a line we could not deserialize.
fn guess_id(line: &str) -> u64 {
    let idx = match line.find("\"id\"") {
        Some(i) => i + 4,
        None => return 0,
    };
    let rest = line[idx..].trim_start().trim_start_matches(':').trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: Value) -> String {
        v.to_string()
    }

    #[test]
    fn parses_load_chain() {
        let req = parse_request(&line(json!({
            "id": 7,
            "cmd": "load_chain",
            "uid": "main",
            "config": "{ node.name = \"Holaf_FX_main\" }",
            "anchor_node_id": 42
        })))
        .expect("valid request");
        assert_eq!(req.id, 7);
        match req.command {
            FxCommand::LoadChain {
                uid,
                config,
                anchor_node_id,
            } => {
                assert_eq!(uid, "main");
                assert!(config.contains("Holaf_FX_main"));
                assert_eq!(anchor_node_id, Some(42));
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn anchor_node_id_is_optional() {
        let req = parse_request(&line(json!({
            "id": 1, "cmd": "load_chain", "uid": "a", "config": "{}"
        })))
        .expect("valid request");
        match req.command {
            FxCommand::LoadChain {
                anchor_node_id, ..
            } => assert_eq!(anchor_node_id, None),
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn parses_every_command() {
        for (text, expected) in [
            (
                json!({"id": 2, "cmd": "unload_chain", "uid": "a"}),
                FxCommand::UnloadChain { uid: "a".into() },
            ),
            (json!({"id": 3, "cmd": "unload_all"}), FxCommand::UnloadAll),
            (json!({"id": 4, "cmd": "ping"}), FxCommand::Ping),
            (json!({"id": 5, "cmd": "status"}), FxCommand::Status),
            (json!({"id": 6, "cmd": "stop"}), FxCommand::Stop),
        ] {
            let req = parse_request(&line(text)).expect("valid request");
            assert_eq!(req.command, expected);
        }
    }

    #[test]
    fn set_params_keeps_flat_order() {
        let req = parse_request(&line(json!({
            "id": 9,
            "cmd": "set_params",
            "uid": "a",
            "props": {"eq_L:100Hz gain": 3.5, "eq_R:100Hz gain": 3.5}
        })))
        .expect("valid request");
        match req.command {
            FxCommand::SetParams { uid, props } => {
                assert_eq!(uid, "a");
                let names: Vec<&str> = props.keys().map(String::as_str).collect();
                assert_eq!(names, ["eq_L:100Hz gain", "eq_R:100Hz gain"]);
                assert_eq!(props[0], 3.5);
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn malformed_json_gets_a_matching_error_reply() {
        let err = parse_request("{\"id\": 12, \"cmd\": ").expect_err("must fail");
        assert_eq!(err.id, 12, "id must be recovered for the parent");
        assert_eq!(err.error_code(), Some("bad_request"));
        assert!(!err.ok);
    }

    #[test]
    fn unknown_command_gets_an_error_reply() {
        let err = parse_request(&line(json!({"id": 3, "cmd": "explode"})))
            .expect_err("must fail");
        assert_eq!(err.id, 3);
        assert_eq!(err.error_code(), Some("bad_request"));
    }

    #[test]
    fn missing_field_gets_an_error_reply() {
        let err = parse_request(&line(json!({"id": 4, "cmd": "load_chain", "uid": "a"})))
            .expect_err("config is required");
        assert_eq!(err.id, 4);
        assert_eq!(err.error_code(), Some("bad_request"));
    }

    #[test]
    fn reply_encoding_is_one_line_and_round_trips_through_serde() {
        let mut data = Map::new();
        data.insert(
            "chain".to_string(),
            json!({"uid": "a", "input_node": "input.Holaf_FX_a"}),
        );
        let text = FxReply::ok(5, data).to_line();
        assert!(text.ends_with('\n'));
        assert_eq!(text.matches('\n').count(), 1, "exactly one line: {text:?}");
        let v: Value = serde_json::from_str(text.trim()).expect("valid json");
        assert_eq!(v["id"], 5);
        assert_eq!(v["ok"], true);
        assert_eq!(v["chain"]["input_node"], "input.Holaf_FX_a");
    }

    #[test]
    fn error_reply_shapes_the_documented_object() {
        let text = FxReply::err(8, "unknown_chain", "no chain 'x'").to_line();
        let v: Value = serde_json::from_str(text.trim()).expect("valid json");
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "unknown_chain");
        assert_eq!(v["error"]["message"], "no chain 'x'");
    }

    #[test]
    fn request_without_id_defaults_to_zero() {
        let req = parse_request(&line(json!({"cmd": "ping"}))).expect("valid");
        assert_eq!(req.id, 0);
    }

    #[test]
    fn data_payload_wins_over_reserved_keys() {
        // A hostile/buggy payload must not be able to flip `ok`.
        let mut data = Map::new();
        data.insert("ok".to_string(), json!(false));
        let text = FxReply::ok(1, data).to_line();
        let v: Value = serde_json::from_str(text.trim()).expect("valid json");
        assert_eq!(v["ok"], true);
        assert_eq!(v["id"], 1);
    }
}
