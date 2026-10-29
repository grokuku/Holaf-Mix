#!/usr/bin/env bash
# ============================================================================
#  scripts/build_rust.sh — Build the Rust `holafd` daemon and publish it
#  next to the Python build output.
# ============================================================================
#
#  Purpose
#  -------
#  J5 (partie sans PipeWire) : le daemon Rust (`holafd`) est un binaire
#  indépendant que la nouvelle UI « Rust mode » (HOLAF_USE_RUST=1) lance au
#  besoin.  Ce script le compile en release et le copie à un emplacement
#  prévu — `dist/holafd` à la racine du dépôt, côte à côte avec la sortie de
#  `build.sh` (dist/holaf-mix, dist/icons/, …).
#
#  Ce script NE MODIFIE PAS `build.sh`.  L'intégration future (quand le
#  backend PipeWire réel sera stable, J1/J2) consistera à :
#    1. appeler ce script depuis `build.sh` (ou inliner `cargo build`) juste
#       avant l'étape PyInstaller ;
#    2. déclarer le binaire dans packaging/holaf-mix.spec (binaries=…) et/ou
#       packaging/PKGBUILD pour l'embarquer dans le paquet final.
#
#  Usage
#  -----
#      ./scripts/build_rust.sh                 # cargo build --release -p holafd → dist/holafd
#      ./scripts/build_rust.sh /tmp/my-dist    # destination personnalisée
#      HOLAFD_BIN_OUT=dist/holafd ./scripts/build_rust.sh
#
#  Sortie : exit 0 si le binaire a été copié, non-zéro sinon.
# ============================================================================

set -euo pipefail

# --- Paths -------------------------------------------------------------------
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
ROOT_DIR="$( cd "$SCRIPT_DIR/.." >/dev/null 2>&1 && pwd )"
RUST_DIR="$ROOT_DIR/rust"

# Default destination: alongside the Python build output.  Override with the
# first CLI arg or $HOLAFD_BIN_OUT.
DEST_DIR="${HOLAFD_BIN_OUT:-}"
if [[ -z "$DEST_DIR" ]]; then
    DEST_DIR="${1:-$ROOT_DIR/dist}"
fi

log() { printf '\033[1;34m[build-rust]\033[0m %s\n' "$*"; }
err() { printf '\033[1;31m[build-rust]\033[0m %s\n' "$*" >&2; }

# --- Cargo availability ------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    err "cargo not found on PATH — install Rust (rustup) first."
    err "See rust/README.md (Prérequis)."
    exit 1
fi

if [[ ! -f "$RUST_DIR/Cargo.toml" ]]; then
    err "workspace Cargo.toml not found at $RUST_DIR/Cargo.toml"
    exit 1
fi

# --- Build -------------------------------------------------------------------
log "Building holafd (release) in $RUST_DIR ..."
(
    cd "$RUST_DIR"
    cargo build --release -p holafd
)

SOURCE_BIN="$RUST_DIR/target/release/holafd"
if [[ ! -x "$SOURCE_BIN" ]]; then
    err "expected binary not found after build: $SOURCE_BIN"
    exit 1
fi

# --- Publish ----------------------------------------------------------------
mkdir -p "$DEST_DIR"
DEST_BIN="$DEST_DIR/holafd"
cp "$SOURCE_BIN" "$DEST_BIN"
chmod +x "$DEST_BIN"

log "Published $SOURCE_BIN → $DEST_BIN"
log "Done. The Rust launcher finds this binary when HOLAFD_BIN=$DEST_BIN is"
log "not set and no rust/target/release/holafd exists next to the sources."
