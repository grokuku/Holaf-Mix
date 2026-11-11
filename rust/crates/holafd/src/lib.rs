//! Holaf-Mix daemon (J3): the NDJSON Unix-socket server and its injectable
//! backends.
//!
//! - [`backend`] defines the engine operations ([`Backend`] trait) the IPC
//!   protocol dispatches to.  The real PipeWire implementation arrives at
//!   J1/J2; [`backend::UnimplementedBackend`] answers `not_implemented`.
//! - [`fake`] provides [`FakeBackend`], an in-memory simulation used by the
//!   integration tests and by `holafd --backend fake`.
//! - [`server`] implements the socket listener, per-connection reader/writer
//!   threads, the backend owner thread (orders via `mpsc`) and event
//!   broadcast.
//! - [`config_check`] implements `holafd --check-config` (migrate a config
//!   and print the graph plan without touching PipeWire).
//!
//! No PipeWire dependency and no async runtime: std threads + channels.  Every
//! order is funnelled through **one** `mpsc` channel to a dedicated thread that
//! owns the backend (rewrite spec §4: « ordres → mpsc channel consommé par un
//! thread », equivalent to the Python engine's `RLock`).  Events take a
//! separate path (a registry of per-connection writers) so a backend can
//! broadcast from inside an order without deadlocking.

pub mod backend;
pub mod config_check;
pub mod fake;
pub mod server;

pub use backend::{Backend, BackendError, UnimplementedBackend};
pub use fake::FakeBackend;
pub use server::Server;
