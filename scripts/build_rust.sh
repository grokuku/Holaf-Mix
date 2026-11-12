#!/usr/bin/env bash
# ============================================================================
#  scripts/build_rust.sh — Build the Rust `holafd` daemon + `holafctl` CLI and
#  publish them next to the Python build output.
# ============================================================================
#
#  Purpose
#  -------
#  Le backend Rust est le mode **par défaut** de Holaf-Mix : `main.py` lance
#  `holafd` (backend PipeWire) au démarrage.  Ce script compile le daemon et la
#  CLI en release et les copie à un emplacement prévu — `dist/` à la racine du
#  dépôt, côte à côte avec la sortie de `build.sh` (dist/holaf-mix,
#  dist/icons/, …).
#
#  `build.sh` appelle ce script (étape « Build Rust daemon + CLI ») **avant**
#  PyInstaller, et `packaging/holaf-mix.spec` embarque ensuite les deux binaires
#  dans le bundle.  L'appel est best-effort côté build.sh : sans cargo, ou si
#  la compilation échoue, le build continue en Python seul (l'app se rabattra
#  sur l'`AudioEngine` classique au démarrage).
#
#  Usage
#  -----
#      ./scripts/build_rust.sh                 # cargo build --release → dist/{holafd,holafctl}
#      ./scripts/build_rust.sh /tmp/my-dist    # destination personnalisée
#      HOLAFD_BIN_OUT=dist ./scripts/build_rust.sh
#
#  Sortie : exit 0 si les binaires ont été copiés, non-zéro sinon.
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
warn() { printf '\033[1;33m[build-rust]\033[0m %s\n' "$*" >&2; }
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
# holafd  : the daemon the app talks to (default backend: PipeWire)
# holafctl: the CLI (status / apply-snapshot / volume / meter-sources …)
log "Building holafd + holafctl (release) in $RUST_DIR ..."
(
    cd "$RUST_DIR"
    cargo build --release -p holafd -p holafctl
)

# --- Publish ----------------------------------------------------------------
mkdir -p "$DEST_DIR"
for bin in holafd holafctl; do
    src="$RUST_DIR/target/release/$bin"
    if [[ ! -x "$src" ]]; then
        err "expected binary not found after build: $src"
        exit 1
    fi
    dest="$DEST_DIR/$bin"
    # Atomic replace: copying straight onto `dest` fails with ETXTBSY when a
    # daemon is *currently running that file* (a leftover holafd from an
    # earlier session, or one the dev is testing).  Writing a sibling temp file
    # and renaming over it always works, and the running process keeps its own
    # (unlinked) inode untouched.
    tmp="$dest.tmp.$$"
    cp "$src" "$tmp"
    chmod +x "$tmp"
    mv -f "$tmp" "$dest"
    log "Published $src → $dest"
done

log "Done. The app's launcher finds holafd automatically: in a packaged build"
log "it looks next to the executable / in the PyInstaller bundle first, then"
log "rust/target/{release,debug}, then \$PATH (\$HOLAFD_BIN overrides all)."
