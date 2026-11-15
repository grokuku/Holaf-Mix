#!/usr/bin/env bash
# ============================================================================
#  scripts/build_rust.sh — Build the Rust `holafd` daemon + `holafctl` CLI and
#  publish them next to the Python build output.
# ============================================================================
#
#  Purpose
#  -------
#  Le backend Rust est le mode **par défaut** de Holaf-Mix, et depuis la bascule
#  B1 l'**interface** aussi : `main.py` lance le binaire `holaf-ui` (UI
#  egui/eframe) et l'UI Qt n'est plus qu'une échappatoire (`HOLAF_UI=python`).
#  Ce script compile donc trois binaires en release et les copie à un
#  emplacement prévu — `dist/` à la racine du dépôt, côte à côte avec la sortie
#  de `build.sh` (dist/holaf-mix, dist/icons/, …) :
#
#      holaf-ui   l'interface (fenêtre, strips, FX, MIDI, diagnostic)
#      holafd     le daemon audio (backend PipeWire)
#      holafctl   la CLI
#
#  `build.sh` appelle ce script (étape « Build Rust daemon + CLI ») **avant**
#  PyInstaller, et `packaging/holaf-mix.spec` embarque ensuite les trois
#  binaires dans le bundle.  L'appel est best-effort côté build.sh : sans cargo,
#  ou si la compilation échoue, le build continue en Python+Qt seul (le lanceur
#  se rabat sur l'UI Qt au démarrage).
#
#  L'interface est une cliente **Wayland** (feature cargo `wayland` de `eframe`,
#  sans X11 : c'est ce qui évite l'icône « X11 » et la demande de serveur X sous
#  Wayland).  Dépendances de build/runtime :
#      Arch   : wayland, wayland-protocols, libxkbcommon
#      Debian : libwayland-dev, wayland-protocols, libwayland-bin,
#               libxkbcommon-dev
#  Elles sont `dlopen`ées au run (invisibles dans `ldd`).  Une machine X11-only
#  doit compiler avec `--features x11` (HOLAF_UI_X11=1 ci-dessous).
#
#  Si `holaf-ui` ne compile pas (libpipewire/wayland absentes de la machine de
#  build), le script retombe sur holafd+holafctl afin de ne pas perdre le
#  daemon : c'est un warning, pas une erreur.
#
#  Usage
#  -----
#      ./scripts/build_rust.sh                 # cargo build --release → dist/{holaf-ui,holafd,holafctl}
#      ./scripts/build_rust.sh /tmp/my-dist    # destination personnalisée
#      HOLAFD_BIN_OUT=dist ./scripts/build_rust.sh
#      HOLAF_NO_TRAY=1 ./scripts/build_rust.sh # SANS l'icône de tray
#      HOLAF_UI_X11=1 ./scripts/build_rust.sh  # + backend X11 (session X11-only)
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
#  Dans TOUS les cas, si la compilation avec `--features tray` échoue, le script
#  réessaie **sans** la feature (on garde holaf-ui, on perd juste l'icône) et
#  affiche un WARNING ; si même là ça échoue, il retombe sur holafd+holafctl.
#
#  Sortie : exit 0 si holafd/holafctl ont été copiés, non-zéro sinon.
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
# holaf-ui : l'interface (UI Rust — c'est elle que `main.py` lance)
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

build_all() {
    if [[ ${#UI_FEATURES[@]} -gt 0 ]]; then
        ( cd "$RUST_DIR" && cargo build --release -p holaf-ui -p holafd -p holafctl "${UI_FEATURES[@]}" )
    else
        ( cd "$RUST_DIR" && cargo build --release -p holaf-ui -p holafd -p holafctl )
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
        warn "full build failed — retrying without holaf-ui (Qt UI kept as the app's UI)"
        if ! ( cd "$RUST_DIR" && cargo build --release -p holafd -p holafctl ); then
            err "cargo build failed for holafd/holafctl too — nothing to publish"
            exit 1
        fi
    fi
fi

# --- Publish ----------------------------------------------------------------
# holafd/holafctl sont obligatoires ; holaf-ui est optionnel (bundle Qt seul).
mkdir -p "$DEST_DIR"
for bin in holaf-ui holafd holafctl; do
    src="$RUST_DIR/target/release/$bin"
    if [[ ! -x "$src" ]]; then
        if [[ "$bin" == "holaf-ui" ]]; then
            warn "$src missing — the bundle will have NO Rust UI (Qt fallback)"
            continue
        fi
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
    log "Rust UI published: $DEST_DIR/holaf-ui (main.py launches it by default)"
fi

log "Done. The app's launcher finds holaf-ui / holafd automatically: in a"
log "packaged build it looks next to the executable / in the PyInstaller bundle"
log "first, then rust/target/{release,debug}, then \$PATH (\$HOLAF_UI_BIN and"
log "\$HOLAFD_BIN override all)."
