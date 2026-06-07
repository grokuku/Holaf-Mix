#!/usr/bin/env bash
# ============================================================================
#  build.sh — End-to-end build script for Holaf-Mix
# ============================================================================
#  Renders the icon set, builds the standalone executable, and (optionally)
#  packages everything into a tarball for distribution.
#
#  Usage:
#    ./build.sh                          # icons + executable
#    ./build.sh --install-deps           # install system + Python deps
#    ./build.sh --icons-only             # just rebuild icons
#    ./build.sh --run                    # build, then launch the binary
#    ./build.sh --tarball                # also produce a .tar.gz
#    ./build.sh --clean                  # remove all build artifacts
#    ./build.sh --onedir                 # onedir build instead of onefile
#
#  Quick start on CachyOS:
#    ./build.sh --install-deps           # one-time setup
#    ./build.sh --run                    # build + launch
#
#  See BUILD.md for full documentation and troubleshooting.
# ============================================================================

set -euo pipefail

# --- Paths -------------------------------------------------------------------
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
cd "$SCRIPT_DIR"

ROOT_DIR="$SCRIPT_DIR"
DIST_DIR="$ROOT_DIR/dist"
BUILD_DIR="$ROOT_DIR/build"
PYI_WORK="$BUILD_DIR/pyinstaller"
PYI_DIST="$BUILD_DIR/dist"

# Spec lives in packaging/ (alongside PKGBUILD and .desktop)
SPEC_FILE="$ROOT_DIR/packaging/holaf-mix.spec"

# --- App metadata ------------------------------------------------------------
APP_NAME="holaf-mix"

# Try to read version from src/__init__.py; fall back to git describe.
get_version() {
    if [[ -f "$ROOT_DIR/src/__init__.py" ]] && \
       grep -q '__version__' "$ROOT_DIR/src/__init__.py"; then
        grep -oP '__version__\s*=\s*"\K[^"]+' "$ROOT_DIR/src/__init__.py"
    else
        git describe --tags --always --dirty 2>/dev/null || echo "0.1.0-dev"
    fi
}
APP_VERSION="$(get_version)"

# --- Output helpers ----------------------------------------------------------
log()  { printf '\033[1;34m[build]\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m[build]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[build]\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31m[build]\033[0m %s\n' "$*" >&2; }
hdr()  { printf '\n\033[1;36m━━━ %s ━━━\033[0m\n' "$*"; }

# --- Distro detection --------------------------------------------------------
detect_distro() {
    if [[ -f /etc/os-release ]]; then
        . /etc/os-release
        case "${ID:-unknown}" in
            arch|cachyos|manjaro|endeavouros|artix|garuda)
                DISTRO_FAMILY="arch"; PKG_MGR="pacman" ;;
            debian|ubuntu|pop|linuxmint|elementary|zorin)
                DISTRO_FAMILY="debian"; PKG_MGR="apt" ;;
            fedora|nobara|ultramarine)
                DISTRO_FAMILY="fedora"; PKG_MGR="dnf" ;;
            opensuse*)
                DISTRO_FAMILY="suse"; PKG_MGR="zypper" ;;
            *)
                DISTRO_FAMILY="unknown"; PKG_MGR="unknown" ;;
        esac
    else
        DISTRO_FAMILY="unknown"; PKG_MGR="unknown"
    fi
}

# --- Dependency lists --------------------------------------------------------
# Note on CachyOS / Arch:
#   - "librsvg2" is wrong — the package is just "librsvg" (it ships rsvg-convert)
#   - "python-pyinstaller" is wrong — pyinstaller is in AUR, not in [extra].
#     The official upstream recommendation is to `pip install pyinstaller`
#     (it's a build-time tool, not a runtime dep, so pip is fine).
#   - "python-pyside6" exists in [extra] but is frequently out of date; pip
#     gives you the version pinned in requirements.txt.
# On Arch/CachyOS, `pipewire-pulse` REPLACES `pulseaudio` (it provides
# the PulseAudio client API on top of PipeWire). Installing both at once
# triggers a hard pacman conflict. The vast majority of modern CachyOS
# installs only have pipewire-pulse; legacy pulseaudio-only systems are
# rare and would be incompatible with the rest of Holaf-Mix's PipeWire
# routing anyway. If you're on such a system, run the install manually.
SYSTEM_DEPS_ARCH=(
    "librsvg"            # provides rsvg-convert
    "imagemagick"        # provides `magick` for ICO bundling
    "pipewire"
    "pipewire-pulse"     # provides the PulseAudio client API
    "ladspa"
    "ladspa-plugins"
)
SYSTEM_DEPS_DEBIAN=(
    "librsvg2-bin"       # provides rsvg-convert on Debian/Ubuntu
    "imagemagick"
    "libpipewire-0.3-dev"
    "libpulse-dev"
    "ladspa-sdk"
    "ladspa-plugins"
)
SYSTEM_DEPS_FEDORA=(
    "librsvg2-tools"     # provides rsvg-convert on Fedora
    "ImageMagick"
    "pipewire-devel"
    "pulseaudio-libs-devel"
    "ladspa"
    "ladspa-plugins"
)
# Python deps installed via pip (NOT pacman) so versions match
# requirements.txt and stay reproducible.
PYTHON_DEPS=(
    "PySide6"
    "sounddevice"
    "numpy"
    "mido"
    "python-rtmidi"
    "pyinstaller"         # AUR-only on Arch, always installed via pip
)

# --- Helpers -----------------------------------------------------------------
has() { command -v "$1" >/dev/null 2>&1; }

print_help() {
    # Print the header docblock (lines 2..21 in this file), stripping the
    # leading "# " comment marker that bash sees in the script body.
    sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
}

# --- Step implementations ----------------------------------------------------
install_deps() {
    # ========================================================================
    # SAFETY GUARDS — read these before running --install-deps
    # ========================================================================
    # This function invokes `sudo pacman` / `apt` / `dnf` on the host. It is
    # INTENDED FOR USE INSIDE A BUILD ENVIRONMENT (a fresh container, a
    # throwaway VM, a GitHub Actions runner) — NOT on a workstation you care
    # about. It will refuse to run by default in interactive desktop
    # sessions. To opt in, set the env var HOLAF_BUILD_INSTALL_DEPS=1.
    # ========================================================================

    detect_distro
    hdr "Detected distro: $DISTRO_FAMILY ($PKG_MGR)"

    # Guard 1: explicit opt-in env var
    if [[ "${HOLAF_BUILD_INSTALL_DEPS:-0}" != "1" ]]; then
        err "Refusing to install system packages without explicit opt-in."
        err ""
        err "This script calls 'sudo pacman' / 'apt' / 'dnf' on your machine."
        err "If you really want that, re-run with:"
        err "    HOLAF_BUILD_INSTALL_DEPS=1 ./build.sh --install-deps"
        err ""
        err "Safer alternatives:"
        err "  - Build inside a container (see .github/workflows/build.yml)"
        err "  - Install deps manually (see BUILD.md for your distro)"
        exit 2
    fi

    # Guard 2: refuse if a graphical session is detected
    if [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}${XDG_CURRENT_DESKTOP:-}" ]]; then
        err "Refusing to run --install-deps: a graphical session is detected."
        err "  DISPLAY=${DISPLAY:-(unset)}"
        err "  WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-(unset)}"
        err "  XDG_CURRENT_DESKTOP=${XDG_CURRENT_DESKTOP:-(unset)}"
        err ""
        err "This usually means you're on a desktop / workstation. Running"
        err "package managers in a GUI session is risky because:"
        err "  - It can disrupt running services (e.g. pipewire-pulse)"
        err "  - A failed transaction can leave the system in a partial state"
        err "  - It can break user sessions mid-operation"
        err ""
        err "Use a build environment instead: container, VM, or CI runner."
        err "If you REALLY know what you're doing, unset these vars first."
        exit 3
    fi

    # Guard 3: confirm interactively (skipped if non-interactive, e.g. CI)
    if [[ -t 0 ]]; then
        printf '\033[1;33m[build]\033[0m About to run:\n'
        case "$DISTRO_FAMILY" in
            arch)   printf '    sudo pacman -S --needed %s\n' "${SYSTEM_DEPS_ARCH[*]}" ;;
            debian) printf '    sudo apt install %s\n'         "${SYSTEM_DEPS_DEBIAN[*]}" ;;
            fedora) printf '    sudo dnf install %s\n'         "${SYSTEM_DEPS_FEDORA[*]}" ;;
        esac
        printf '    pip install %s\n' "${PYTHON_DEPS[*]}"
        printf '\033[1;33m[build]\033[0m Continue? [y/N] '
        read -r answer
        if [[ ! "$answer" =~ ^[Yy]$ ]]; then
            err "Aborted by user."
            exit 4
        fi
    fi

    if [[ "$DISTRO_FAMILY" == "unknown" ]]; then
        err "Unsupported distro. Please install manually:"
        err "  System:  rsvg-convert (Arch: librsvg | Debian: librsvg2-bin), imagemagick"
        err "           libpipewire dev headers, pulseaudio dev headers, ladspa-plugins"
        err "  Python:  ${PYTHON_DEPS[*]}"
        exit 1
    fi

    case "$DISTRO_FAMILY" in
        arch)
            log "Installing system packages via pacman..."
            sudo pacman -S --needed --noconfirm "${SYSTEM_DEPS_ARCH[@]}" || {
                err "pacman install failed. Try: sudo pacman -S --needed ${SYSTEM_DEPS_ARCH[*]}"
                exit 1
            }
            PIP_USER_FLAG="--user"
            ;;
        debian)
            log "Installing system packages via apt..."
            sudo apt update
            sudo apt install -y "${SYSTEM_DEPS_DEBIAN[@]}" || {
                err "apt install failed"
                exit 1
            }
            PIP_USER_FLAG="--user --break-system-packages"
            warn "Using --break-system-packages (PEP 668). Consider a venv instead."
            ;;
        fedora)
            log "Installing system packages via dnf..."
            sudo dnf install -y "${SYSTEM_DEPS_FEDORA[@]}" || {
                err "dnf install failed"
                exit 1
            }
            PIP_USER_FLAG="--user"
            ;;
    esac

    log "Installing Python packages via pip ${PIP_USER_FLAG}..."
    python3 -m pip install $PIP_USER_FLAG "${PYTHON_DEPS[@]}" || {
        err "pip install failed"
        exit 1
    }

    ok "All dependencies installed."
    ok "  Python deps: ${PYTHON_DEPS[*]}"
    ok "  System deps: distro-appropriate packages above"
}

check_deps() {
    log "Checking build dependencies..."

    local missing=()
    has pyinstaller  || missing+=("pyinstaller (pip install pyinstaller)")
    has rsvg-convert || missing+=("rsvg-convert (Arch: pacman -S librsvg | Debian: apt install librsvg2-bin)")
    has magick       || missing+=("magick (pacman -S imagemagick | apt install imagemagick)")

    if [[ ${#missing[@]} -gt 0 ]]; then
        err "Missing required tools:"
        for m in "${missing[@]}"; do err "  - $m"; done
        err ""
        err "Run './build.sh --install-deps' to install them automatically."
        err "Or install manually: see BUILD.md for your distro."
        exit 1
    fi

    # Python deps: try to import, fall back to a clear error
    if ! python3 -c "import PySide6, sounddevice, numpy, mido, rtmidi" 2>/dev/null; then
        err "Missing Python packages. Run './build.sh --install-deps' or:"
        err "  pip install --user ${PYTHON_DEPS[*]}"
        exit 1
    fi

    ok "All dependencies present."
}

build_icons() {
    hdr "Step 1/3 — Building icon set"
    if [[ -x "$ROOT_DIR/assets/icons/build_icons.sh" ]]; then
        "$ROOT_DIR/assets/icons/build_icons.sh"
    else
        warn "assets/icons/build_icons.sh not found, skipping icons"
        return 0
    fi
    ok "Icons ready in $DIST_DIR/icons/"
}

build_executable() {
    local mode="${1:-onefile}"
    hdr "Step 2/3 — Building $mode executable with PyInstaller"

    if [[ ! -f "$SPEC_FILE" ]]; then
        err "Spec file not found: $SPEC_FILE"
        err "Did you run ./build.sh from the project root?"
        exit 1
    fi

    # Edit spec on the fly if --onedir requested (toggle the COLLECT block)
    local spec_to_use="$SPEC_FILE"
    if [[ "$mode" == "onedir" ]]; then
        log "Patching spec to produce onedir build..."
        spec_to_use="$BUILD_DIR/holaf-mix-onedir.spec"
        mkdir -p "$BUILD_DIR"
        # Uncomment the COLLECT block; comment the EXE's bundled binaries
        sed -e 's|# COLLECT(|COLLECT(|' \
            -e 's|^\(# \)\(a.binaries, a.zipfiles, a.datas,\)|\2|' \
            -e '/^exe_onefile = EXE/,/^)$/ {
                s|a.binaries,|# a.binaries,|
                s|a.datas,|# a.datas,|
            }' \
            "$SPEC_FILE" > "$spec_to_use"
    fi

    # Clean only the pyinstaller work dir, not the whole build/
    rm -rf "$PYI_WORK" "$PYI_DIST"
    mkdir -p "$PYI_WORK" "$PYI_DIST"

    log "Running PyInstaller (this takes 2-5 minutes the first time)..."
    pyinstaller \
        --noconfirm \
        --workpath "$PYI_WORK" \
        --distpath "$PYI_DIST" \
        "$spec_to_use"

    # Publish into ./dist/ for clarity
    rm -rf "$DIST_DIR/holaf-mix" "$DIST_DIR/holaf-mix.bin"
    if [[ -f "$PYI_DIST/holaf-mix" ]]; then
        cp "$PYI_DIST/holaf-mix" "$DIST_DIR/holaf-mix"
        chmod +x "$DIST_DIR/holaf-mix"
        ok "Onefile binary: $DIST_DIR/holaf-mix"
    fi
    if [[ -d "$PYI_DIST/holaf-mix" ]]; then
        cp -r "$PYI_DIST/holaf-mix" "$DIST_DIR/"
        ok "Onedir bundle:  $DIST_DIR/holaf-mix/"
    fi

    # Copy icons alongside so the user has them in one place
    if [[ -d "$DIST_DIR/icons" ]]; then
        if [[ -d "$DIST_DIR/holaf-mix" ]]; then
            cp -r "$DIST_DIR/icons" "$DIST_DIR/holaf-mix/icons"
        fi
    fi

    ok "Executable built."
}

make_tarball() {
    hdr "Step 3/3 — Building distribution tarball"

    local arch; arch="$(uname -m)"
    local tar_name="${APP_NAME}-${APP_VERSION}-${arch}.tar.gz"
    local staging="$DIST_DIR/${APP_NAME}-${APP_VERSION}"

    rm -rf "$staging"
    mkdir -p "$staging"

    if [[ -f "$DIST_DIR/holaf-mix" ]]; then
        cp "$DIST_DIR/holaf-mix" "$staging/"
    fi
    if [[ -d "$DIST_DIR/holaf-mix" ]] && [[ ! -f "$DIST_DIR/holaf-mix" ]]; then
        cp -r "$DIST_DIR/holaf-mix" "$staging/"
    fi
    if [[ -d "$DIST_DIR/icons" ]]; then
        cp -r "$DIST_DIR/icons" "$staging/icons"
    fi
    [[ -f "$ROOT_DIR/README.md" ]]   && cp "$ROOT_DIR/README.md"   "$staging/"
    [[ -f "$ROOT_DIR/ROADMAP.md" ]]  && cp "$ROOT_DIR/ROADMAP.md"  "$staging/"
    [[ -f "$ROOT_DIR/BUILD.md" ]]    && cp "$ROOT_DIR/BUILD.md"    "$staging/"
    [[ -f "$ROOT_DIR/packaging/holaf-mix.desktop" ]] && \
        cp "$ROOT_DIR/packaging/holaf-mix.desktop" "$staging/"

    tar -czf "$DIST_DIR/$tar_name" -C "$DIST_DIR" "${APP_NAME}-${APP_VERSION}"
    rm -rf "$staging"

    ok "Tarball: $DIST_DIR/$tar_name"
    ls -la "$DIST_DIR/$tar_name"
}

run_binary() {
    local bin="$DIST_DIR/holaf-mix"
    if [[ ! -x "$bin" ]]; then
        err "Binary not found at $bin. Run ./build.sh first."
        exit 1
    fi
    log "Launching $bin ..."
    exec "$bin" "$@"
}

clean() {
    hdr "Cleaning build artifacts"
    rm -rf "$BUILD_DIR" "$DIST_DIR"
    # Clean PyInstaller cache too
    rm -rf "$ROOT_DIR/__pycache__" "$ROOT_DIR/src/__pycache__"
    find "$ROOT_DIR/src" -name "__pycache__" -type d -exec rm -rf {} + 2>/dev/null || true
    ok "Cleaned. (Source files untouched.)"
}

# --- CLI parsing -------------------------------------------------------------
MODE="onefile"
ACTION="build"
RUN_AFTER=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --install-deps) install_deps; exit 0 ;;
        --check)        check_deps; exit 0 ;;
        --icons-only)   ACTION="icons" ;;
        --executable)   ACTION="exe" ;;
        --tarball)      ACTION="all-tar" ;;
        --run)          RUN_AFTER=true ;;
        --onedir)       MODE="onedir" ;;
        --onefile)      MODE="onefile" ;;
        --clean)        clean; exit 0 ;;
        -h|--help)      print_help; exit 0 ;;
        -V|--version)   echo "$APP_NAME $APP_VERSION"; exit 0 ;;
        *)              err "Unknown arg: $1"; print_help; exit 1 ;;
    esac
    shift
done

# --- Main sequence -----------------------------------------------------------
hdr "Holaf-Mix build (v$APP_VERSION, mode=$MODE)"
check_deps

case "$ACTION" in
    icons)   build_icons ;;
    exe)     build_icons; build_executable "$MODE" ;;
    build)   build_icons; build_executable "$MODE" ;;
    all-tar) build_icons; build_executable "$MODE"; make_tarball ;;
esac

if $RUN_AFTER; then
    run_binary
else
    ok ""
    ok "Build complete."
    if [[ -x "$DIST_DIR/holaf-mix" ]]; then
        ok "  Run:   $DIST_DIR/holaf-mix"
    fi
    if [[ -d "$DIST_DIR/holaf-mix" ]] && [[ ! -f "$DIST_DIR/holaf-mix" ]]; then
        ok "  Run:   $DIST_DIR/holaf-mix/holaf-mix"
    fi
    ok "  Or:    ./build.sh --run"
    ok "  Docs:  cat BUILD.md"
fi
