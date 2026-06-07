#!/usr/bin/env bash
# ============================================================================
#  build_icons.sh — Generate icon assets for Holaf-Mix
# ============================================================================
#  Renders assets/icons/holaf-mix.svg to a complete set of PNGs at all the
#  sizes KDE / Linux desktops / installers expect, plus a .ico bundle and
#  an AppImage-style icon (no extension, square 512).
#
#  Requires:
#    - rsvg-convert   (librsvg2-bin)        — primary renderer
#    - ImageMagick    (imagemagick)         — ICO bundling, fallback
#    - Inkscape       (inkscape, optional)  — only used if rsvg fails
#
#  Usage:
#    ./build_icons.sh                # build everything
#    ./build_icons.sh --check        # verify required tools are present
#    ./build_icons.sh --clean        # remove all generated files
#
#  Output (under dist/icons/):
#    holaf-mix.svg         (copied from source)
#    holaf-mix-16.png
#    holaf-mix-24.png
#    holaf-mix-32.png
#    holaf-mix-48.png
#    holaf-mix-64.png
#    holaf-mix-128.png
#    holaf-mix-256.png
#    holaf-mix-512.png
#    holaf-mix.png                  (256x256 default)
#    holaf-mix.ico                  (multi-resolution for Windows compat)
# ============================================================================

set -euo pipefail

# --- Paths -------------------------------------------------------------------
# This script lives at <repo>/assets/icons/build_icons.sh, so we go up
# two levels to get to the repo root.
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
ROOT_DIR="$( cd "$SCRIPT_DIR/../.." && pwd )"
SRC_SVG="$ROOT_DIR/assets/icons/holaf-mix.svg"
OUT_DIR="$ROOT_DIR/dist/icons"

# --- Sizes -------------------------------------------------------------------
# KDE Plasma conventions: 16, 22, 32, 48, 64, 128, 256, 512
# Tray icon: usually 22 or 32
# .desktop icons: 48, 64, 128, 256
# High-DPI: 512, 1024
SIZES=(16 22 24 32 48 64 128 256 512)

# --- Helpers -----------------------------------------------------------------
log()  { printf '\033[1;34m[icons]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[icons]\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31m[icons]\033[0m %s\n' "$*" >&2; }

check_tools() {
    local missing=0
    for tool in rsvg-convert magick; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            err "Missing required tool: $tool"
            missing=1
        fi
    done
    if [[ $missing -ne 0 ]]; then
        err ""
        err "Install the missing tools:"
        err "  CachyOS / Arch:  sudo pacman -S librsvg2 imagemagick"
        err "  Debian / Ubuntu: sudo apt install librsvg2-bin imagemagick"
        err "  Fedora:          sudo dnf install librsvg2-tools ImageMagick"
        exit 1
    fi
    if [[ ! -f "$SRC_SVG" ]]; then
        err "Source SVG not found: $SRC_SVG"
        exit 1
    fi
}

clean() {
    log "Cleaning $OUT_DIR"
    rm -rf "$OUT_DIR"
}

build() {
    check_tools
    mkdir -p "$OUT_DIR"

    # Copy the SVG so the dist tree is self-contained
    cp "$SRC_SVG" "$OUT_DIR/holaf-mix.svg"
    log "Copied SVG to $OUT_DIR"

    # Render each size
    for size in "${SIZES[@]}"; do
        local out="$OUT_DIR/holaf-mix-${size}.png"
        # rsvg-convert preserves the viewBox, so it scales cleanly.
        # --keep-aspect-ratio is the default; we don't pad because our
        # viewBox is exactly 256x256.
        if rsvg-convert -w "$size" -h "$size" -f png -o "$out" "$SRC_SVG" 2>/dev/null; then
            log "  ${size}x${size} -> $(basename "$out")"
        else
            warn "rsvg-convert failed for size $size; trying ImageMagick fallback"
            magick -background none -density 384 "$SRC_SVG" -resize "${size}x${size}" "$out"
        fi
    done

    # Default name used by the .desktop file
    cp "$OUT_DIR/holaf-mix-256.png" "$OUT_DIR/holaf-mix.png"
    log "  default icon -> holaf-mix.png (256x256)"

    # Build a multi-resolution .ico (Windows + some Linux tools)
    # Includes 16, 32, 48, 64, 128, 256 — the standard set
    log "Building multi-resolution .ico"
    magick \
        "$OUT_DIR/holaf-mix-16.png" \
        "$OUT_DIR/holaf-mix-32.png" \
        "$OUT_DIR/holaf-mix-48.png" \
        "$OUT_DIR/holaf-mix-64.png" \
        "$OUT_DIR/holaf-mix-128.png" \
        "$OUT_DIR/holaf-mix-256.png" \
        "$OUT_DIR/holaf-mix.ico"
    log "  -> holaf-mix.ico (16/32/48/64/128/256)"

    # Summary
    log ""
    log "Generated icons in $OUT_DIR:"
    ls -la "$OUT_DIR" | awk 'NR>1 {printf "  %-30s %8s bytes\n", $9, $5}' | grep -v '^  $\|^  total'
    log ""
    log "Done. Use holaf-mix.png as the default or pick a specific size."
}

# --- Entry point -------------------------------------------------------------
case "${1:-build}" in
    --check)  check_tools && log "All required tools present." ;;
    --clean)  clean ;;
    build|"") build ;;
    -h|--help)
        sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
        ;;
    *)
        err "Unknown argument: $1"
        err "Run '$0 --help' for usage."
        exit 1
        ;;
esac
