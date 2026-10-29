//! The [`Backend`] trait shared by the daemon and the real PipeWire backend.
//!
//! The definition now lives in **holaf-core** (`holaf_core::backend`) so both
//! `holafd` and `holaf-pw` can depend on it without a crate cycle.  This
//! module re-exports the canonical types so the rest of `holafd` (IPC
//! dispatch, [`FakeBackend`](crate::fake::FakeBackend)) keeps compiling
//! unchanged:
//!
//! - [`Backend`] — the engine operations the IPC protocol dispatches to.
//! - [`BackendError`] — stable `code` + human `message` returned to clients.
//! - [`UnimplementedBackend`] — answers `not_implemented` on every order.
//!
//! The real PipeWire implementation lives in `holaf-pw`
//! ([`holaf_pw::PipeWireBackend`]) and is wired into `holafd --backend
//! pipewire`.

pub use holaf_core::backend::{Backend, BackendError, UnimplementedBackend};
