#!/usr/bin/env bash
# ============================================================================
#  scripts/build_rust.sh — Build `holaf-ui` + `holafd` + `holafctl` (release)
#  and publish the three binaries to a destination directory (default: dist/).
# ============================================================================
#
#  Purpose
#  -------
#  Holaf-Mix is a 100 % Rust product: there is no Python launcher and no
#  PyInstaller bundle any more.  This script compiles the three bins that make
#  the whole product and copies them where the packager expects them (`dist/`
#  by default, alongside the icon set):
#
#      holaf-ui   the application (window, strips, FX, MIDI, diagnostics)
#      holafd     the audio daemon (alternative backend + automation)
#      holafctl   the CLI
#
#  `build.sh` calls it as its "Step 2/3" and FAILS if it fails: a build without
#  `holaf-ui` is not a product.
#
#  The UI is a **Wayland** client (eframe's `wayland` feature, no X11: that is
#  what avoids the "X11" icon and the X-server demand under Wayland).
#  Build/runtime dependencies:
#      Arch   : wayland, wayland-protocols, libxkbcommon
#      Debian : libwayland-dev, wayland-protocols, libwayland-bin,
#               libxkbcommon-dev
#  They are `dlopen`ed at run time (invisible in `ldd`).  An X11-only machine
#  needs `--features x11` (HOLAF_UI_X11=1 below).
#
#  Usage
#  -----
#      ./scripts/build_rust.sh                 # cargo build --release → dist/{holaf-ui,holafd,holafctl}
#      ./scripts/build_rust.sh /tmp/my-dist    # destination personnalisée
#      HOLAFD_BIN_OUT=dist ./scripts/build_rust.sh
#      HOLAF_NO_TRAY=1 ./scripts/build_rust.sh # SANS l'icône de tray
#      HOLAF_UI_X11=1 ./scripts/build_rust.sh  # + backend X11 (session X11-only)
#      HOLAF_CARGO_LOCKED=1 ./scripts/build_rust.sh  # refuse de toucher Cargo.lock (CI)
#
#  L'icône de zone de notification (feature cargo `tray`) est **activée par
#  défaut** : c'est ce que le paquet distribué doit contenir (décision du
#  2026-11-14).  Depuis la migration vers le backend **ksni** (D-Bus
#  StatusNotifierItem, 2026-11-15) elle ne réclame PLUS NI GTK3 NI
#  libayatana-appindicator : aucun paquet système à installer au build, et au
#  runtime il suffit d'un hôte StatusNotifier (KDE/Plasma, waybar, swaybar,
#  gnome-shell-extension-appindicator).  `HOLAF_NO_TRAY=1` (ou `HOLAF_TRAY=0`)
#  reste là pour livrer un binaire sans icône.
#
#  Si la compilation avec `--features tray` échoue, le script réessaie **sans**
#  la feature (on garde holaf-ui, on perd juste l'icône) et affiche un WARNING.
#  Si même là ça échoue, c'est une erreur : plus de Python pour reprendre le
#  relais.
#
#  Sortie : exit 0 si les trois binaires ont été publiés, non-zéro sinon.
# ============================================================================

set -euo pipefail

# --- Paths -------------------------------------------------------------------
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
ROOT_DIR="$( cd "$SCRIPT_DIR/.." >/dev/null 2>&1 && pwd )"
RUST_DIR="$ROOT_DIR/rust"

# Default destination: dist/ at the repo root (next to dist/icons/).  Override
# with the first CLI arg or $HOLAFD_BIN_OUT.
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
# holaf-ui : l'interface (l'application — point d'entrée du produit)
# holafd   : le daemon audio (backend par défaut : PipeWire)
# holafctl : la CLI (status / apply-snapshot / volume / meter-sources …)
#
# Les trois en UNE invocation cargo : ils partagent toutes leurs dépendances
# (holaf-pw, egui…), un second `cargo build` ne recompilerait rien mais
# multiplierait les risques d'états intermédiaires incohérents.
#
# La feature `tray` (icône de zone de notification) est ACTIVÉE PAR DÉFAUT :
# c'est la parité Qt, et c'est ce que le paquet distribué contient.
#
# Échappatoires, dans cet ordre :
#   HOLAF_NO_TRAY=1        -> pas de tray (la façon recommandée)
#   HOLAF_TRAY=0           -> idem (l'ancienne variable, conservée pour les
#                             scripts/CI existants qui la mettaient à 0)
# Backend par défaut = `ksni` (D-Bus pur, zéro GTK) ; `HOLAF_TRAY_GTK=1` force
# l'ancien backend `libappindicator` (feature `tray-gtk`, qui exige `gtk3` +
# `libayatana-appindicator` au build) — filet de sécurité, pas le défaut.
# Si la compilation avec la feature échoue, on réessaie sans (voir plus bas)
# pour ne jamais perdre holaf-ui.
# Features cargo de holaf-ui :
#   tray  l'icône de zone de notification (voir ci-dessus)
#   x11   backend X11 **en plus** de Wayland (opt-in, jamais dans le paquet :
#         l'app cible Wayland, cf. rust/README.md § « Serveur graphique »)
TRAY_FEATURE="tray"
if [[ "${HOLAF_TRAY_GTK:-0}" == "1" ]]; then
    TRAY_FEATURE="tray-gtk"
fi
UI_FEATURES=(--features "$TRAY_FEATURE")
if [[ "${HOLAF_NO_TRAY:-0}" == "1" || "${HOLAF_TRAY:-1}" == "0" ]]; then
    UI_FEATURES=()
fi
if [[ "${HOLAF_UI_X11:-0}" == "1" ]]; then
    if [[ ${#UI_FEATURES[@]} -gt 0 ]]; then
        UI_FEATURES+=("x11")
    else
        UI_FEATURES=(--features x11)
    fi
fi

# `HOLAF_CARGO_LOCKED=1` makes the build refuse to update Cargo.lock — the CI
# uses it so a lockfile out of sync with the manifests fails visibly instead of
# producing a non-reproducible binary.
CARGO_LOCKED=()
if [[ "${HOLAF_CARGO_LOCKED:-0}" == "1" ]]; then
    CARGO_LOCKED=(--locked)
fi

build_all() {
    if [[ ${#UI_FEATURES[@]} -gt 0 ]]; then
        ( cd "$RUST_DIR" && cargo build --release "${CARGO_LOCKED[@]}" -p holaf-ui -p holafd -p holafctl "${UI_FEATURES[@]}" )
    else
        ( cd "$RUST_DIR" && cargo build --release "${CARGO_LOCKED[@]}" -p holaf-ui -p holafd -p holafctl )
    fi
}

log "Building holaf-ui + holafd + holafctl (release) in $RUST_DIR ..."
if [[ ${#UI_FEATURES[@]} -gt 0 ]]; then
    log "  + cargo feature: $TRAY_FEATURE (ksni/D-Bus by default — no GTK)"
else
    log "  tray feature DISABLED (HOLAF_NO_TRAY/HOLAF_TRAY=0) - no notification icon"
fi
if ! build_all; then
    if [[ ${#UI_FEATURES[@]} -gt 0 ]]; then
        warn "WARNING: build with --features $TRAY_FEATURE FAILED"
        warn "         retrying WITHOUT the tray icon - holaf-ui is kept, but the"
        warn "         packaged app will have NO notification icon."
        warn "         (ksni needs no system package; if you forced HOLAF_TRAY_GTK=1,"
        warn "         install gtk3 + libayatana-appindicator, or set HOLAF_NO_TRAY=1"
        warn "         to make this explicit and silence this warning.)"
        UI_FEATURES=()
    fi
    if ! build_all; then
        warn "build failed — there is no Python fallback any more."
        warn "Fix the build (see BUILD.md §5) and re-run."
        exit 1
    fi
fi

# --- Publish ----------------------------------------------------------------
# All three are required: the product is the Rust binaries, there is no Python
# bundle to fall back to.
mkdir -p "$DEST_DIR"
for bin in holaf-ui holafd holafctl; do
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
    log "Published $src → $dest ($(du -h --apparent-size "$dest" | cut -f1))"
done

if [[ -x "$DEST_DIR/holaf-ui" ]]; then
    log "Rust UI published: $DEST_DIR/holaf-ui (the app's entry point)"
fi

log "Done. Run '$DEST_DIR/holaf-ui' (or package it with './build.sh --tarball')."
