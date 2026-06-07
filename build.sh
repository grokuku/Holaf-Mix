#!/usr/bin/env bash
# ============================================================================
#  build.sh — End-to-end build script for Holaf-Mix
# ============================================================================
#  Builds the icons, the standalone executable, and (optionally) packages
#  everything into a tarball for distribution.
#
#  Usage:
#    ./build.sh                 # icons + executable
#    ./build.sh --icons-only    # just rebuild the icon set
#    ./build.sh --clean         # remove all build artifacts
#    ./build.sh --tarball       # also produce dist/holaf-mix-<ver>.tar.gz
#
#  Requires (on CachyOS / Arch):
#    pacman -S python-pip python-pyqt6
#                -- OR --
#    pacman -S python-pip
#    pip install --user pyinstaller PySide6 sounddevice numpy mido python-rtmidi
#    pacman -S librsvg2 imagemagick
# ============================================================================

set -euo pipefail

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
ROOT_DIR="$( cd "$SCRIPT_DIR" && pwd )"
cd "$ROOT_DIR"

# --- Helpers -----------------------------------------------------------------
log()  { printf '\033[1;34m[build]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[build]\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31m[build]\033[0m %s\n' "$*" >&2; }
ok()   { printf '\033[1;32m[build]\033[0m %s\n' "$*"; }

# --- Config ------------------------------------------------------------------
APP_NAME="holaf-mix"
VERSION=$(grep -oP '__version__\s*=\s*"\K[^"]+' src/__init__.py 2>/dev/null || echo "0.1.0")
if [[ "$VERSION" == "0.1.0" ]]; then
    # No __version__ yet — derive from ROADMAP / git
    VERSION=$(git describe --tags --always --dirty 2>/dev/null || echo "0.1.0-dev")
fi

# --- Steps -------------------------------------------------------------------
check_dependencies() {
    log "Checking build dependencies..."

    local missing=()
    command -v pyinstaller  >/dev/null 2>&1 || missing+=("pyinstaller")
    command -v rsvg-convert >/dev/null 2>&1 || missing+=("rsvg-convert (librsvg2-bin)")
    command -v magick        >/dev/null 2>&1 || missing+=("magick (imagemagick)")

    if [[ ${#missing[@]} -gt 0 ]]; then
        err "Missing required tools:"
        for m in "${missing[@]}"; do err "  - $m"; done
        err ""
        err "On CachyOS / Arch, install with:"
        err "  sudo pacman -S --needed librsvg2 imagemagick python-pyinstaller"
        err "  pip install --user PySide6 sounddevice numpy mido python-rtmidi"
        exit 1
    fi

    # Python deps
    python3 -c "import PySide6, sounddevice, numpy, mido, rtmidi" 2>/dev/null || {
        err "Missing Python packages. Run:"
        err "  pip install --user PySide6 sounddevice numpy mido python-rtmidi"
        exit 1
    }

    ok "All dependencies present"
}

build_icons() {
    log "Building icon set..."
    ./assets/icons/build_icons.sh
    ok "Icons built in dist/icons/"
}

build_executable() {
    log "Building executable (this can take 2-5 minutes the first time)..."

    # Use the spec file we ship in packaging/
    pyinstaller \
        --noconfirm \
        --clean \
        --workpath build/pyinstaller \
        --distpath dist \
        packaging/holaf-mix.spec

    # Copy icons next to the binary so the desktop file / app menu can find them
    if [[ -d dist/holaf-mix ]]; then
        cp -r dist/icons dist/holaf-mix/ 2>/dev/null || true
    fi
    if [[ -f dist/holaf-mix ]]; then
        mkdir -p dist/holaf-mix-icons
        cp -r dist/icons/* dist/holaf-mix-icons/ 2>/dev/null || true
    fi

    ok "Executable built"
}

make_tarball() {
    log "Creating distribution tarball..."
    local tar_name="${APP_NAME}-${VERSION}-$(uname -m).tar.gz"
    local staging_dir="dist/${APP_NAME}-${VERSION}"

    rm -rf "$staging_dir"
    mkdir -p "$staging_dir"

    # If onefile: just the binary. If onedir: the whole folder.
    if [[ -d dist/holaf-mix ]]; then
        cp -r dist/holaf-mix "$staging_dir/"
    fi
    if [[ -f dist/holaf-mix ]]; then
        cp dist/holaf-mix "$staging_dir/"
    fi

    # Always include icons and docs
    cp -r dist/icons "$staging_dir/icons"
    cp README.md ROADMAP.md "$staging_dir/" 2>/dev/null || true
    cp packaging/holaf-mix.desktop "$staging_dir/" 2>/dev/null || true

    tar -czf "dist/$tar_name" -C dist "${APP_NAME}-${VERSION}"
    rm -rf "$staging_dir"

    ok "Tarball created: dist/$tar_name"
    ls -la "dist/$tar_name"
}

clean() {
    log "Cleaning build artifacts..."
    rm -rf build/ dist/
    ok "Cleaned"
}

# --- Entry point -------------------------------------------------------------
ACTION="all"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --icons-only)  ACTION="icons" ;;
        --executable)  ACTION="exe"   ;;
        --tarball)     ACTION="all-tar" ;;
        --clean)       ACTION="clean" ;;
        -h|--help)
            sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) err "Unknown arg: $1"; exit 1 ;;
    esac
    shift
done

case "$ACTION" in
    icons)   build_icons ;;
    exe)     check_dependencies; build_executable ;;
    all)     check_dependencies; build_icons; build_executable ;;
    all-tar) check_dependencies; build_icons; build_executable; make_tarball ;;
    clean)   clean ;;
esac

ok "All done."
