//! [`FakeBackend`] — an in-memory [`Backend`] simulation.
//!
//! Used by the J3 integration tests and by `holafd --backend fake`.  It keeps
//! a copy of every strip, a routing matrix, a default-sink marker and a log of
//! every order it has processed (in arrival order), mirroring what the real
//! PipeWire engine will do at J2 — minus any actual audio.

use indexmap::IndexMap;
use serde_json::{json, Value};

use holaf_core::model::{EffectKey, Strip};

use crate::backend::{Backend, BackendError};

/// An in-memory implementation of [`Backend`] with observable state.
#[derive(Debug, Default)]
pub struct FakeBackend {
    /// Live strips keyed by uid, preserving apply/creation order.
    strips: IndexMap<String, Strip>,
    /// Routing matrix rows `(source_uid, target_uid, active)`.
    routes: Vec<(String, String, bool)>,
    /// Which sink uid is the system default (if any).
    default_sink: Option<String>,
    /// Applications tracked via `app_move` (surrogate for PipeWire streams).
    apps: Vec<(String, String)>,
    /// Whether the engine is running.
    running: bool,
    /// Ordered log of orders, one string per order.
    orders: Vec<String>,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// A compact digest of every order received, in arrival order.
    pub fn orders(&self) -> &[String] {
        &self.orders
    }

    /// True if the backend processed exactly one order whose digest equals
    /// `needle`.
    pub fn order_matches(&self, needle: &str) -> bool {
        self.orders.iter().any(|o| o == needle)
    }

    pub fn strip(&self, uid: &str) -> Option<&Strip> {
        self.strips.get(uid)
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    fn log(&mut self, entry: String) {
        self.orders.push(entry);
    }

    fn unknown_strip(uid: &str) -> BackendError {
        BackendError::new("unknown_strip", format!("no strip with uid '{uid}'"))
    }

    fn strip_or_err(&mut self, uid: &str) -> Result<&mut Strip, BackendError> {
        self.strips
            .get_mut(uid)
            .ok_or_else(|| Self::unknown_strip(uid))
    }

    /// Serialize an effect state into JSON (finite floats guaranteed by the
    /// data model, so this cannot fail in practice).
    fn effect_json(state: &holaf_core::model::EffectState) -> Result<Value, BackendError> {
        serde_json::to_value(state)
            .map_err(|e| BackendError::internal("fx serialization", e))
    }
}

impl Backend for FakeBackend {
    fn apply_snapshot(&mut self, strips: &[Strip]) -> Result<Value, BackendError> {
        self.strips.clear();
        let mut applied = 0usize;
        for strip in strips {
            // Only strips with a non-empty uid are real (an empty uid is a
            // corrupt/partial entry the daemon never accepts).  Count what is
            // actually applied, not what was offered.
            if !strip.uid.is_empty() {
                self.strips.insert(strip.uid.clone(), strip.clone());
                applied += 1;
            }
        }
        self.routes.clear();
        self.apps.clear();
        self.default_sink = None;
        self.running = true;
        self.log(format!("apply_snapshot:count={applied}"));
        Ok(json!({ "applied": applied, "running": true }))
    }

    fn stop(&mut self) -> Result<(), BackendError> {
        self.running = false;
        self.log("stop".to_string());
        Ok(())
    }

    fn status(&mut self) -> Result<Value, BackendError> {
        self.log("status".to_string());
        let strips: Vec<Value> = self
            .strips
            .values()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(|e| BackendError::internal("status", e))?;
        let routes: Vec<Value> = self
            .routes
            .iter()
            .map(|(s, t, a)| json!({"source_uid": s, "target_uid": t, "active": a}))
            .collect();
        let apps: Vec<Value> = self
            .apps
            .iter()
            .map(|(id, sink)| json!({"app_id": id, "sink_name": sink}))
            .collect();
        Ok(json!({
            "running": self.running,
            "strips": strips,
            "routes": routes,
            "apps": apps,
            "default_sink": self.default_sink,
        }))
    }

    fn set_volume(&mut self, uid: &str, value: f32) -> Result<Value, BackendError> {
        let found = self.strip_or_err(uid)?;
        found.volume = value;
        self.log(format!("set_volume:uid={uid},value={value}"));
        Ok(json!({ "ok": true, "uid": uid, "value": value }))
    }

    fn set_mute(&mut self, uid: &str, muted: bool) -> Result<Value, BackendError> {
        let found = self.strip_or_err(uid)?;
        found.mute = muted;
        self.log(format!("set_mute:uid={uid},muted={muted}"));
        Ok(json!({ "ok": true, "uid": uid, "muted": muted }))
    }

    fn set_mono(&mut self, uid: &str, mono: bool) -> Result<Value, BackendError> {
        let found = self.strip_or_err(uid)?;
        found.is_mono = mono;
        self.log(format!("set_mono:uid={uid},mono={mono}"));
        Ok(json!({ "ok": true, "uid": uid, "mono": mono }))
    }

    fn fx_set_active(
        &mut self,
        uid: &str,
        effect: EffectKey,
        active: bool,
    ) -> Result<Value, BackendError> {
        // Scope the mutable borrow so it is released before self.log().
        let result = {
            let found = self.strip_or_err(uid)?;
            let state = found.effects.get_mut(&effect).ok_or_else(|| {
                BackendError::new(
                    "unknown_effect",
                    format!("strip '{uid}' has no effect '{}'", effect.as_str()),
                )
            })?;
            state.active = active;
            Self::effect_json(state)?
        };
        self.log(format!(
            "fx_set_active:uid={uid},effect={},active={active}",
            effect.as_str()
        ));
        Ok(result)
    }

    fn fx_set_params(
        &mut self,
        uid: &str,
        effect: EffectKey,
        params: &IndexMap<String, f32>,
    ) -> Result<Value, BackendError> {
        let digest = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        // Scope the mutable borrow so it is released before self.log().
        let result = {
            let found = self.strip_or_err(uid)?;
            let state = found.effects.get_mut(&effect).ok_or_else(|| {
                BackendError::new(
                    "unknown_effect",
                    format!("strip '{uid}' has no effect '{}'", effect.as_str()),
                )
            })?;
            for (port, value) in params {
                state.params.insert(port.clone(), *value);
            }
            Self::effect_json(state)?
        };
        self.log(format!(
            "fx_set_params:uid={uid},effect={},{}",
            effect.as_str(),
            digest
        ));
        Ok(result)
    }

    fn routing_set(
        &mut self,
        source_uid: &str,
        target_uid: &str,
        active: bool,
    ) -> Result<Value, BackendError> {
        if active {
            if let Some(row) = self
                .routes
                .iter_mut()
                .find(|(s, t, _)| s == source_uid && t == target_uid)
            {
                row.2 = true;
            } else {
                self.routes
                    .push((source_uid.to_string(), target_uid.to_string(), true));
            }
        } else {
            self.routes
                .retain(|(s, t, _)| s != source_uid || t != target_uid);
        }
        self.log(format!(
            "routing_set:source_uid={source_uid},target_uid={target_uid},active={active}"
        ));
        Ok(json!({
            "ok": true,
            "source_uid": source_uid,
            "target_uid": target_uid,
            "active": active,
        }))
    }

    fn routing_list(&mut self) -> Result<Value, BackendError> {
        self.log("routing_list".to_string());
        let routes: Vec<Value> = self
            .routes
            .iter()
            .map(|(s, t, a)| json!({"source_uid": s, "target_uid": t, "active": a}))
            .collect();
        Ok(json!({ "routes": routes }))
    }

    fn set_default_sink(&mut self, uid: &str) -> Result<Value, BackendError> {
        self.strip_or_err(uid)?;
        self.default_sink = Some(uid.to_string());
        self.log(format!("set_default_sink:uid={uid}"));
        Ok(json!({ "ok": true, "uid": uid }))
    }

    fn device_list(&mut self) -> Result<Value, BackendError> {
        self.log("device_list".to_string());
        let devices: Vec<Value> = self
            .strips
            .values()
            .filter_map(|s| s.device_name.as_ref().map(|d| (s, d)))
            .map(|(s, d)| {
                json!({
                    "uid": s.uid,
                    "label": s.label,
                    "kind": s.kind.as_str(),
                    "device_name": d,
                })
            })
            .collect();
        Ok(json!({ "devices": devices }))
    }

    fn app_list(&mut self) -> Result<Value, BackendError> {
        self.log("app_list".to_string());
        let apps: Vec<Value> = self
            .apps
            .iter()
            .map(|(id, sink)| json!({"app_id": id, "sink_name": sink}))
            .collect();
        Ok(json!({ "apps": apps }))
    }

    fn app_move(&mut self, app_id: &str, sink_name: &str) -> Result<Value, BackendError> {
        self.apps
            .retain(|(id, _)| id != app_id);
        self.apps.push((app_id.to_string(), sink_name.to_string()));
        self.log(format!("app_move:app_id={app_id},sink_name={sink_name}"));
        Ok(json!({ "ok": true, "app_id": app_id, "sink_name": sink_name }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holaf_core::model::{EffectKey, Strip, StripType};

    fn sample_strips() -> Vec<Strip> {
        vec![
            Strip::with_uid("in-1", "Micro", StripType::Input),
            Strip::with_uid("out-1", "Speakers", StripType::Output),
        ]
    }

    #[test]
    fn apply_snapshot_seeds_state_and_orders() {
        let mut fake = FakeBackend::new();
        let strips = sample_strips();
        let result = fake.apply_snapshot(&strips).unwrap();
        assert_eq!(result["applied"], 2);
        assert!(fake.is_running());
        assert!(fake.strip("in-1").is_some());
        assert!(fake.strip("out-1").is_some());
        assert!(fake.order_matches("apply_snapshot:count=2"));
    }

    #[test]
    fn apply_snapshot_skips_and_does_not_count_empty_uids() {
        let mut fake = FakeBackend::new();
        let strips = vec![
            Strip::with_uid("in-1", "Micro", StripType::Input),
            Strip::with_uid("", "Empty", StripType::Input),
            Strip::with_uid(" ", "Blank", StripType::Output),
        ];
        // A whitespace-only uid is still non-empty (`" ".is_empty()` is
        // false), so this filter keeps it — only the truly empty "" uid is
        // skipped.  The daemon layer rejects empty uids upstream; this test
        // pins the counter to the strips actually stored.
        let result = fake.apply_snapshot(&strips).unwrap();
        assert_eq!(result["applied"], 2, "applied must count real (non-empty uid) strips only");
        assert!(fake.strip("in-1").is_some());
        assert!(fake.strip(" ").is_some(), "blank uid is non-empty and stored");
        assert!(fake.strip("").is_none(), "empty-uid strip must not be stored");
        assert_eq!(fake.strips.len(), 2);
        assert!(fake.order_matches("apply_snapshot:count=2"));
    }

    #[test]
    fn volume_unknown_strip_errors() {
        let mut fake = FakeBackend::new();
        let err = fake.set_volume("nope", 0.5).unwrap_err();
        assert_eq!(err.code, "unknown_strip");
        assert!(fake.orders().is_empty(), "failed ops must not be logged");
    }

    #[test]
    fn volume_and_mute_update_state() {
        let mut fake = FakeBackend::new();
        fake.apply_snapshot(&sample_strips()).unwrap();

        assert_eq!(fake.set_volume("in-1", 0.25).unwrap()["value"], 0.25);
        fake.set_mute("in-1", true).unwrap();
        fake.set_mono("in-1", true).unwrap();

        let strip = fake.strip("in-1").unwrap();
        assert_eq!(strip.volume, 0.25);
        assert!(strip.mute);
        assert!(strip.is_mono);
        assert!(fake.order_matches("set_volume:uid=in-1,value=0.25"));
        assert!(fake.order_matches("set_mute:uid=in-1,muted=true"));
        assert!(fake.order_matches("set_mono:uid=in-1,mono=true"));
    }

    #[test]
    fn fx_set_active_returns_and_stores_state() {
        let mut fake = FakeBackend::new();
        fake.apply_snapshot(&sample_strips()).unwrap();

        let result = fake.fx_set_active("in-1", EffectKey::Eq, true).unwrap();
        assert_eq!(result["active"], true);
        assert!(result["params"].is_object());
        assert!(fake.strip("in-1").unwrap().effects[&EffectKey::Eq].active);
        assert!(fake.order_matches("fx_set_active:uid=in-1,effect=eq,active=true"));
    }

    #[test]
    fn fx_set_params_merges_into_existing_ports() {
        let mut fake = FakeBackend::new();
        fake.apply_snapshot(&sample_strips()).unwrap();

        let mut params = IndexMap::new();
        params.insert("50Hz gain (low shelving)".to_string(), -6.0);
        params.insert("10000Hz gain".to_string(), 3.0);
        let result = fake
            .fx_set_params("in-1", EffectKey::Eq, &params)
            .unwrap();

        assert_eq!(result["params"]["50Hz gain (low shelving)"], -6.0);
        assert_eq!(result["params"]["10000Hz gain"], 3.0);
        // Untouched EQ bands stay at their default 0.0.
        assert_eq!(result["params"]["5000Hz gain"], 0.0);

        let stored = &fake.strip("in-1").unwrap().effects[&EffectKey::Eq].params;
        assert_eq!(stored["50Hz gain (low shelving)"], -6.0);
        assert_eq!(stored["10000Hz gain"], 3.0);
        assert!(fake
            .order_matches("fx_set_params:uid=in-1,effect=eq,50Hz gain (low shelving)=-6,10000Hz gain=3"));
    }

    #[test]
    fn fx_on_effect_absent_from_strip_errors() {
        let mut fake = FakeBackend::new();
        fake.apply_snapshot(&sample_strips()).unwrap();
        // out-1 was built with kind Output but migration keeps all five
        // effects, so target a uid with no strips at all instead.
        let err = fake.fx_set_active("ghost", EffectKey::Gate, true).unwrap_err();
        assert_eq!(err.code, "unknown_strip");
    }

    #[test]
    fn routing_set_and_list_round_trip() {
        let mut fake = FakeBackend::new();
        fake.apply_snapshot(&sample_strips()).unwrap();

        fake.routing_set("in-1", "out-1", true).unwrap();
        fake.routing_set("in-1", "out-1", true).unwrap(); // idempotent re-add
        let list = fake.routing_list().unwrap();
        assert_eq!(list["routes"].as_array().unwrap().len(), 1);
        assert_eq!(list["routes"][0]["source_uid"], "in-1");

        fake.routing_set("in-1", "out-1", false).unwrap();
        let list = fake.routing_list().unwrap();
        assert_eq!(list["routes"].as_array().unwrap().len(), 0);
        assert!(fake
            .order_matches("routing_set:source_uid=in-1,target_uid=out-1,active=true"));
    }

    #[test]
    fn default_sink_and_device_list() {
        let mut fake = FakeBackend::new();
        let mut speakers = Strip::with_uid("out-1", "Speakers", StripType::Output);
        speakers.device_name = Some("alsa_output.abc.analog-stereo".to_string());
        let strips = vec![
            Strip::with_uid("in-1", "Micro", StripType::Input),
            speakers,
        ];
        fake.apply_snapshot(&strips).unwrap();

        fake.set_default_sink("out-1").unwrap();
        assert_eq!(fake.default_sink.as_deref(), Some("out-1"));

        let status = fake.status().unwrap();
        assert_eq!(status["default_sink"], "out-1");
        assert_eq!(status["strips"].as_array().unwrap().len(), 2);
        assert_eq!(status["running"], true);

        let devices = fake.device_list().unwrap();
        let arr = devices["devices"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["device_name"], "alsa_output.abc.analog-stereo");

        fake.stop().unwrap();
        assert!(!fake.is_running());
        assert!(fake.order_matches("stop"));
    }

    #[test]
    fn app_move_then_list() {
        let mut fake = FakeBackend::new();
        fake.app_move("app-1", "Speakers").unwrap();
        fake.app_move("app-1", "Casque").unwrap(); // re-home
        let list = fake.app_list().unwrap();
        let arr = list["apps"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["app_id"], "app-1");
        assert_eq!(arr[0]["sink_name"], "Casque");
        assert!(fake.order_matches("app_move:app_id=app-1,sink_name=Casque"));
    }

    #[test]
    fn status_reflects_mutations() {
        let mut fake = FakeBackend::new();
        fake.apply_snapshot(&sample_strips()).unwrap();
        fake.set_volume("in-1", 0.75).unwrap();
        fake.fx_set_active("in-1", EffectKey::Tube, true).unwrap();

        let status = fake.status().unwrap();
        let arr = status["strips"].as_array().unwrap();
        let in_strip = arr.iter().find(|s| s["uid"] == "in-1").unwrap();
        assert_eq!(in_strip["volume"], 0.75);
        assert_eq!(in_strip["effects"]["tube"]["active"], true);
    }
}
