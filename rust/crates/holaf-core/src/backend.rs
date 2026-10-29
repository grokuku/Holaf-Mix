//! The [`Backend`] trait: the slice of the audio engine the IPC layer talks
//! to, plus the [`BackendError`] every implementation reports.
//!
//! This module lives in **holaf-core** (the shared, PipeWire-free crate) so
//! that both the daemon (`holafd`) and the real PipeWire implementation
//! (`holaf-pw`) can implement/use it without a crate cycle:
//!
//! ```text
//!        holafd ────┐
//! holaf-core:backend ◄── holaf-pw (PipeWireBackend)
//! ```
//!
//! `holafd` re-exports these types from `crate::backend` so its IPC layer and
//! [`FakeBackend`](crate::fake::FakeBackend) keep compiling unchanged.
//!
//! Every method is synchronous and **serialised by the caller** (the daemon
//! holds one `Mutex<Box<dyn Backend>>`, mirroring the Python `RLock`).

use indexmap::IndexMap;
use serde_json::Value;
use thiserror::Error;

use crate::model::{EffectKey, Strip};

/// Failure reported back to an IPC client.
///
/// `code` is a stable machine-readable string that becomes the IPC
/// `error.code` (e.g. `"unknown_strip"`, `"not_implemented"`); `message` is
/// the human-readable `error.message`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct BackendError {
    pub code: &'static str,
    pub message: String,
}

impl BackendError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        BackendError {
            code,
            message: message.into(),
        }
    }

    /// A backend that does not implement a method yet (the unimplemented
    /// PipeWire backend, and any op that requires a live server).
    pub fn not_implemented(op: &str) -> Self {
        BackendError::new(
            "not_implemented",
            format!("operation '{op}' is not implemented by this backend"),
        )
    }

    pub fn internal(op: &str, cause: impl std::fmt::Display) -> Self {
        BackendError::new(
            "internal_error",
            format!("internal error in {op}: {cause}"),
        )
    }
}

/// The engine operations reachable through the IPC contract (spec §4).
///
/// `apply_snapshot` replaces the whole engine state; the other mutations are
/// incremental orders.  All methods except [`Backend::stop`] return a JSON
/// value that becomes the IPC `result` payload.
pub trait Backend: Send {
    /// Replace the engine state with `strips` (start / full restart).
    fn apply_snapshot(&mut self, strips: &[Strip]) -> Result<Value, BackendError>;

    /// Stop the engine and tear down the audio graph.
    fn stop(&mut self) -> Result<(), BackendError>;

    /// Current daemon state (running flag, strips, routes, devices, …).
    fn status(&mut self) -> Result<Value, BackendError>;

    /// Set a strip's volume (0.0–1.0 nominal, may exceed 1.0).
    fn set_volume(&mut self, uid: &str, value: f32) -> Result<Value, BackendError>;

    /// Mute / unmute a strip.
    fn set_mute(&mut self, uid: &str, muted: bool) -> Result<Value, BackendError>;

    /// Toggle stereo→mono downmix.
    fn set_mono(&mut self, uid: &str, mono: bool) -> Result<Value, BackendError>;

    /// Enable / disable one effect on a strip (returns the resulting FX
    /// state).
    fn fx_set_active(
        &mut self,
        uid: &str,
        effect: EffectKey,
        active: bool,
    ) -> Result<Value, BackendError>;

    /// Hot-reload an effect's control values (returns the resulting FX
    /// state).
    fn fx_set_params(
        &mut self,
        uid: &str,
        effect: EffectKey,
        params: &IndexMap<String, f32>,
    ) -> Result<Value, BackendError>;

    /// Add / remove a route between two strips.
    fn routing_set(
        &mut self,
        source_uid: &str,
        target_uid: &str,
        active: bool,
    ) -> Result<Value, BackendError>;

    /// List the current routing matrix.
    fn routing_list(&mut self) -> Result<Value, BackendError>;

    /// Make a sink the system default.
    fn set_default_sink(&mut self, uid: &str) -> Result<Value, BackendError>;

    /// List physical audio devices known to the engine.
    fn device_list(&mut self) -> Result<Value, BackendError>;

    /// List applications currently playing audio.
    fn app_list(&mut self) -> Result<Value, BackendError>;

    /// Move an application to another sink.
    fn app_move(&mut self, app_id: &str, sink_name: &str) -> Result<Value, BackendError>;
}

/// Placeholder for the real PipeWire backend.  Every operation fails with
/// `not_implemented`.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnimplementedBackend;

impl Backend for UnimplementedBackend {
    fn apply_snapshot(&mut self, _strips: &[Strip]) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("engine.apply_snapshot"))
    }

    fn stop(&mut self) -> Result<(), BackendError> {
        Err(BackendError::not_implemented("engine.stop"))
    }

    fn status(&mut self) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("engine.status"))
    }

    fn set_volume(&mut self, _uid: &str, _value: f32) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("strip.set_volume"))
    }

    fn set_mute(&mut self, _uid: &str, _muted: bool) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("strip.set_mute"))
    }

    fn set_mono(&mut self, _uid: &str, _mono: bool) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("strip.set_mono"))
    }

    fn fx_set_active(
        &mut self,
        _uid: &str,
        _effect: EffectKey,
        _active: bool,
    ) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("fx.set_active"))
    }

    fn fx_set_params(
        &mut self,
        _uid: &str,
        _effect: EffectKey,
        _params: &IndexMap<String, f32>,
    ) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("fx.set_params"))
    }

    fn routing_set(
        &mut self,
        _source_uid: &str,
        _target_uid: &str,
        _active: bool,
    ) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("routing.set"))
    }

    fn routing_list(&mut self) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("routing.list"))
    }

    fn set_default_sink(&mut self, _uid: &str) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("system.set_default_sink"))
    }

    fn device_list(&mut self) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("device.list"))
    }

    fn app_list(&mut self) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("app.list"))
    }

    fn app_move(&mut self, _app_id: &str, _sink_name: &str) -> Result<Value, BackendError> {
        Err(BackendError::not_implemented("app.move"))
    }
}
