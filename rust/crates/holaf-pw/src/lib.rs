//! Holaf-Mix PipeWire layer — the real J2 runtime.
//!
//! Two deliverables:
//!
//! - [`sys`] — the tiny FFI shim ("holaf-sys") around
//!   `pw_context_load_module` so holafd can host the LADSPA filter-chain
//!   **in-process**, exactly like `pw-cli` does (J1 recon conclusion).  This
//!   is the one C API pipewire-rs does not expose; everything else lives in
//!   pipewire-rs proper.
//! - [`spa_config`] — SPA-JSON builders kept as a compact reference (the
//!   generic filter-chain graph / flat-Props builders live in `holaf-core`).
//! - [`backend`] / [`engine`] / [`live`] — the real J2 **PipeWireBackend**
//!   behind the `holaf_core::backend::Backend` trait: a graph reconciler that
//!   creates null-sinks + filter-chains + links on a live daemon, with volume/
//!   mute, default-sink metadata and device discovery.
//!
//! # Threading
//!
//! [`PipeWireBackend`] is a `Send` handle that marshals every operation to a
//! dedicated worker thread owning the PipeWire objects (see [`backend`]).  To
//! point it at a specific daemon, set the usual PipeWire environment
//! (`XDG_RUNTIME_DIR`, `PIPEWIRE_RUNTIME_DIR`, `PIPEWIRE_REMOTE`) before
//! connecting.
//!
//! The end-to-end spike scenario lives in the `holaf-spike` binary
//! (`src/bin/holaf-spike.rs`).

pub mod backend;
pub mod engine;
pub mod live;
pub mod spa_config;
pub mod sys;

pub use backend::PipeWireBackend;
pub use live::LiveView;
// Backwards-compatible re-export for the holaf-spike binary (J1 helpers).
pub use spa_config::*;
