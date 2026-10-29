//! Holaf-Mix core library (J0 — pure data model, no PipeWire dependency).
//!
//! This crate is the single source of truth for:
//!
//! - [`model`]: the strip / effect data model (serde-compatible with the
//!   current `config.json`).
//! - [`fx`]: the LADSPA FX tables (`DEFAULT_EFFECT_PARAMS`, `BYPASS_PARAMS`,
//!   plugin mapping, fusion rules, stereo port names, plugin availability)
//!   copied byte-for-byte from the Python `strip_model.py` /
//!   `audio_engine.py`.
//! - [`spa`]: SPA-JSON formatting — `_format_params`, the `filter.graph`
//!   builder (`build_graph`, Python `_build_fx_graph`) and the flat `Props`
//!   payload for hot-reload (`build_props` / `control_pairs`, J1-corrected
//!   shape).  Golden-tested against the Python fixtures in
//!   `tests/fixtures/spa_builders_golden.json`.
//! - [`migration`]: `from_dict` migrations (old short port names, legacy
//!   boolean effect format, stale `Model` param).
//! - [`config`]: atomic `config.json` read/write.
//!
//! - [`backend`]: the [`Backend`] engine-operations trait + [`BackendError`]
//!   shared by the daemon (`holafd`) and the real PipeWire implementation
//!   (`holaf-pw`) — this is the one non-data domain concept of the core crate.
//! - [`graph`]: pure graph-planning helpers (node naming, chain decisions)
//!   used by the J2 reconciler.
//!
//! Zero runtime dependencies beyond std + serde/indexmap.

pub mod backend;
pub mod config;
pub mod fx;
pub mod graph;
pub mod migration;
pub mod model;
pub mod spa;
