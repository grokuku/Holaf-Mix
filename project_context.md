# PROJECT CONTEXT - HOLAF MIX

> Mémo de contexte du dépôt.  Depuis le 2026-11-15 le produit est **100 %
> Rust** : il n'y a plus de Python, de PySide6, de PyInstaller ni de venv
> (voir `docs/python-removal.md`).  L'UI Qt décrite dans les versions
> précédentes de ce fichier n'existe plus.

## 1. Project Overview
- **Goal**: Linux audio mixer (Voicemeeter-like) on PipeWire, written entirely in
  Rust.
- **Target Platform**: CachyOS / Arch Linux (Wayland).
- **Current Phase**: **STABILIZATION & TUNING**.
- **Status**:
    - **Core Audio**: functional (virtual strips, physical routing, exclusive
      source logic) — `rust/crates/holaf-pw`.
    - **UI**: functional (VU meters, faders, buttons, right-click FX settings) —
      `rust/crates/holaf-ui` (egui/eframe).
    - **FX Engine**: functional (filter-chain, dynamic parameters, RNNoise
      confirmed working).
    - **Discovery**: `pactl -f json` / `pw-dump` through `holaf-pw`, in-process.

## 2. Architecture (Rust workspace)
- **Model**: `holaf_core::model::Strip` (`rust/crates/holaf-core/src/model.rs`).
    - Effects are stored as `{'active': bool, 'params': {key: value}}`; the
      legacy boolean form is migrated by `holaf_core::migration`.
- **Controller**: `holaf_pw::PipeWireBackend` (`rust/crates/holaf-pw`), driven
    **in-process** by the UI (no daemon, no IPC for the window).
    - **FX Hosting**: `holafd --fx-host` hosts the filter-chain out of the audio
      callback (see `docs/fx-host.md`).
    - **Routing**: exclusive source strategy — the unused path is unlinked
      before the new one is created (anti-gate).
    - **Param Parsing**: `holaf_core::spa` builds the SPA-JSON, golden-tested
      against `rust/crates/holaf-core/tests/fixtures/spa_builders_golden.json`.
- **View**: `holaf_ui` (egui): `views/`, `widgets/`, `dialogs.rs`, `fx_ui.rs`,
  `theme/`.
- **Daemon / CLI**: `holafd` + `holafctl` (`rust/crates/holafd`,
  `rust/crates/holafctl`, protocol in `holaf-ipc`) — automation and
  diagnostics; the UI does not need them.

## 3. Project File Structure
```text
Holaf-Mix/
├── rust/                         # THE product (100 % Rust)
│   ├── crates/holaf-core/        # model, migrations, SPA-JSON, config I/O
│   ├── crates/holaf-pw/          # PipeWire backend, FX host, routing
│   ├── crates/holaf-ipc/         # NDJSON protocol of the daemon
│   ├── crates/holafd/            # audio daemon (alternative backend)
│   ├── crates/holafctl/          # CLI (status, apply-snapshot, meter-sources)
│   └── crates/holaf-ui/          # the app: egui views, MIDI, tray, diagnostics
├── build.sh                      # icons + cargo build + tarball (no Python)
├── scripts/
│   ├── build_rust.sh             # cargo build --release, publishes the 3 bins
│   ├── check-wayland-guard.sh    # no X11 backend in the shipped binary
│   └── start-pipewire-headless.sh# harness for the live Rust tests
├── packaging/
│   ├── PKGBUILD                  # optional Arch package
│   ├── holaf-mix.sh              # `holaf-mix` → exec holaf-ui (shell, no Python)
│   └── holaf-mix.desktop         # Exec=holaf-ui
├── config.json                   # reference config (tests + dev)
├── docs/                         # design, migration history, python-removal
└── .github/workflows/build.yml   # CI: cargo test + tarball artefact
```

## 4. Technical Implementation Details

### Native Effect Engine
- **Method**: PipeWire `filter-chain` (hosted by `holafd --fx-host`).
- **Format**: SPA-JSON built by `holaf_core::spa` from the model.
- **Graph**: stereo (dual mono) — `Node_L` and `Node_R` per LADSPA plugin.
- **Parameter Handling**:
    - **Gate**: Threshold, Attack, Release, Hold.
    - **Compressor**: Threshold, Ratio, Attack, Release, Makeup Gain.
    - **EQ**: 15-band graphic EQ (mbeq).
    - **RNNoise**: VAD threshold.

### Audio Routing Strategy (Refined)
- **Virtual Strips**: null sinks / `Holaf_*` nodes.
- **Exclusive linking**: before connecting `FX_Output -> Bus`, disconnect
  `Physical_Source -> Bus`; before connecting `Physical_Source -> Bus`,
  disconnect `FX_Output -> Bus`.
- **Mono**: unlink stereo ports before applying `L->L, L->R`.
- **App routing**: `holaf_pw::sink_inputs` (`pactl -f json` + read-back), see
  `holaf-pw/tests/app_routing_live.rs`.

## 5. Resolved Issues (History)
- **Anti-gate**: dry signal persisted when the gate was active → exclusive
  linking (unlink the unused path first).
- **Mono "left only"**: stale stereo links → unlink before re-linking.
- **Link detection false negatives**: locale-dependent `pw-link` errors → the
  Rust backend parses the graph instead of error strings.
- **Effects loaded while disabled**: truthiness bug in the Python engine →
  `active` is an explicit `bool` in the Rust model.

## 6. Next Steps / TODO
- **Parameter tuning**: EQ/compressor defaults still need testing.
- **Real-time updates**: parameter changes currently reload the FX chain.
- **Visual feedback**: gain-reduction metering for compressor/gate.
