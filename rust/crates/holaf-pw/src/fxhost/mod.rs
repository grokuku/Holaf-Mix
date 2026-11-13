//! The dedicated FX host: a child process whose only job is to host
//! `libpipewire-module-filter-chain` modules and keep a PipeWire mainloop
//! running — the shape the Python engine has always used (a long-lived
//! `pw-cli`) and the one that never chopped.
//!
//! * [`proto`] — the NDJSON protocol (pure, unit-tested).
//! * [`host`] — the child side, entered by `holafd --fx-host`.
//! * [`client`] — the parent side: spawn, request/reply, death detection.
//!
//! The decision of *which* chains should exist and their respawn/restore policy
//! lives one level up, in [`crate::chainhost::ChainHost`].

pub mod client;
pub mod host;
pub mod proto;

pub use client::{ChainInfo, FxHostClient, HOST_REQUEST_TIMEOUT};
pub use host::run_host;
pub use proto::{FxCommand, FxErrorInfo, FxReply, FxRequest};
