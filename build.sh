#!/usr/bin/env bash
# ============================================================================
#  build.sh — End-to-end build script for Holaf-Mix
# ============================================================================
#  Renders the icon set, builds the Rust binaries (holaf-ui + holafd +
#  holafctl), builds the standalone executable (embedding all three), and
#  (optionally) packages everything into a tarball for distribution.
#
#  Usage:
#    ./build.sh                          # icons + Rust daemon + executable
#    ./build.sh --install-deps           # install system + Python deps
#    ./build.sh --icons-only             # just rebuild icons
#    ./build.sh --run                    # build, then launch the binary
#    ./build.sh --tarball                # also produce a .tar.gz
#    ./build.sh --clean                  # remove all build artifacts
#    ./build.sh --onedir                 # onedir build instead of onefile
#
#  Env:
#    HOLAF_NO_TRAY=1                     # build without the tray icon
#                                        # (default: tray ON — D-Bus ksni,
#                                        #  no GTK package needed)
#    HOLAF_TRAY_GTK=1                    # force the old libappindicator
#                                        # backend (needs gtk3 dev)
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
#   - "python-pyinstaller" is wrong — pyinstaller is not packaged for Arch at
#     all (neither [extra] nor the AUR under that name). The official upstream
#     recommendation is to `pip install pyinstaller` (it's a build-time tool,
#     not a runtime dep, so pip is fine).
#   - "python-pyside6" is the Debian name; on Arch it is "pyside6" (it is
#     frequently out of date; pip gives you the version pinned in
#     requirements.txt).
#   - "ladspa-plugins" does NOT exist on Arch (it is not a Debian package
#     either): the four LADSPA files the mixer loads come from
#     "swh-plugins" -> /usr/lib/ladspa/{gate_1410,mbeq_1197,sc4_1882}.so
#   - PipeWire is the ONLY audio server supported: `pipewire-pulse` provides
#     `pulse-native-provider`, exactly like `pulseaudio`, so installing both
#     triggers a hard pacman conflict and installs a server nothing talks to.
#     The `pactl` binary the app EXECs comes from `libpulse`, not from either
#     of them.
SYSTEM_DEPS_ARCH=(
    # --- build toolchain -------------------------------------------------
    "base-devel"         # gcc/make (Rust link step, pip wheel builds)
    "pkgconf"            # the -sys crates probe their .pc files at build time
    "clang"              # libclang.so for bindgen (libspa-sys, pipewire-sys)
    "rust"               # rustc/cargo >= MSRV (rust/Cargo.toml)
    # --- packaging tools --------------------------------------------------
    "librsvg"            # provides rsvg-convert
    "imagemagick"        # provides `magick` for ICO bundling
    # --- runtime: PipeWire stack -----------------------------------------
    "pipewire"           # libpipewire-0.3 + pw-cli/pw-dump/pw-link/pw-metadata
    "pipewire-audio"     # pw-cat — the VU meters record through it
    "pipewire-pulse"     # the Pulse server `pactl` talks to (replaces pulseaudio)
    "libpulse"           # /usr/bin/pactl itself (app list / move)
    "wireplumber"        # session manager: builds the nodes the mixer manages
    "alsa-lib"           # alsa-sys: the MIDI Mix through the ALSA sequencer
    "swh-plugins"        # gate_1410 / mbeq_1197 / sc4_1882 LADSPA plugins
    # TAP plugins (tap_tubewarmth — the TUBE WARMTH strip) are **AUR-only** on
    # Arch (`tap-plugins`), so they are NOT in this list: `pacman -S` would fail
    # for everyone.  Install it with an AUR helper if you want that strip:
    #     yay -S tap-plugins        # or: git clone https://aur.archlinux.org/tap-plugins.git
    # Without it every OTHER strip works; the UI simply cannot insert TUBE
    # WARMTH (it checks /usr/lib/ladspa before offering the FX).
    #
    # The tray icon (cargo feature `tray`, ON BY DEFAULT in
    # scripts/build_rust.sh) needs NO package at all any more: the backend is
    # `ksni`, a pure D-Bus StatusNotifierItem (no GTK3, no libappindicator —
    # see docs/dependency-audit.md).  gtk3/libayatana-appindicator used to be
    # listed here; they are not needed by any build any more.
    "ladspa"             # optional: analyseplugin/applyplugin (debug only)
)
SYSTEM_DEPS_DEBIAN=(
    # --- build toolchain -------------------------------------------------
    "build-essential"
    "pkg-config"
    "clang"              # libclang.so for bindgen
    # NOTE: Debian 12's `rustc` is 1.63, far below the workspace MSRV (1.98):
    #       install the toolchain with rustup, not with apt.
    # --- packaging tools --------------------------------------------------
    "librsvg2-bin"       # provides rsvg-convert on Debian/Ubuntu
    "imagemagick"
    # --- runtime: PipeWire stack -----------------------------------------
    "libpipewire-0.3-dev"
    "libspa-0.2-dev"
    "libpulse-dev"
    "pipewire-bin"       # pw-cat / pw-cli / pw-dump / pw-link / pw-metadata
    "pipewire-pulse"     # the Pulse server `pactl` talks to
    "wireplumber"        # session manager
    "pulseaudio-utils"   # /usr/bin/pactl (NOT in libpulse0)
    "libasound2-dev"     # alsa-sys (MIDI)
    "swh-plugins"        # gate_1410 / mbeq_1197 / sc4_1882 LADSPA plugins
    "tap-plugins"        # tap_tubewarmth — the TUBE WARMTH strip (in `main`)
    # No tray package: the `tray` feature is ksni/D-Bus since the 2026-11-15
    # migration, so GTK3/libayatana-appindicator are not needed any more.
    "ladspa-sdk"         # optional: analyseplugin/applyplugin (debug only)
)
SYSTEM_DEPS_FEDORA=(
    # --- build toolchain -------------------------------------------------
    "gcc"
    "gcc-c++"
    "make"
    "pkgconf-pkg-config"
    "clang"              # libclang.so for bindgen
    # --- packaging tools --------------------------------------------------
    "librsvg2-tools"     # provides rsvg-convert on Fedora
    "ImageMagick"
    # --- runtime: PipeWire stack -----------------------------------------
    "pipewire-devel"
    "pipewire-utils"     # pw-cat / pw-cli / pw-dump / pw-link / pw-metadata
    "pipewire-pulseaudio"
    "pulseaudio-utils"   # /usr/bin/pactl
    "wireplumber"        # session manager
    "alsa-lib-devel"     # alsa-sys (MIDI)
    "ladspa-swh-plugins" # gate_1410 / mbeq_1197 / sc4_1882 (Fedora name)
    "ladspa-tap-plugins" # tap_tubewarmth — the TUBE WARMTH strip
    "pulseaudio-libs-devel"
    # No tray package: the `tray` feature is ksni/D-Bus (no GTK), see
    # docs/dependency-audit.md.
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
    # Print the leading docblock: every "#" comment line at the top of this
    # file, from line 2 until the first real statement.  Computed rather than
    # hardcoded as a line range — the header grows (HOLAF_NO_TRAY env, …) and a
    # fixed range silently truncates the help text.
    awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"
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
        err "           libpipewire/libspa dev headers + pkgconf + clang (bindgen),"
        err "           libasound dev headers (MIDI), libpulse dev headers,"
        err "           swh-plugins (LADSPA gate_1410/mbeq_1197/sc4_1882) +"
        err "           tap-plugins (tap_tubewarmth; AUR-only on Arch)"
        err "  Runtime: pipewire (+ pipewire-audio for pw-cat), pipewire-pulse,"
        err "           wireplumber, libpulse (pactl).  No tray package needed:"
        err "           the tray is a D-Bus StatusNotifierItem (ksni), no GTK."
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

    # The tray icon needs no build package any more (ksni/D-Bus).  What is
    # worth warning about is the LADSPA set: the UI offers a strip per plugin
    # file it finds, so a missing one silently removes a feature.
    if [[ ! -f /usr/lib/ladspa/tap_tubewarmth.so ]]; then
        warn "tap_tubewarmth.so (TAP plugins) not found — the TUBE WARMTH strip"
        warn "  will not be offered by the UI.  Debian: apt install tap-plugins |"
        warn "  Fedora: dnf install ladspa-tap-plugins | Arch: AUR package tap-plugins"
    fi

    # MIDI (cargo feature `midi`, also ON by default) is built against the ALSA
    # sequencer through alsa-sys, which needs the alsa .pc file.
    if ! pkg-config --exists alsa 2>/dev/null; then
        warn "alsa (ALSA dev headers) not found — the Rust UI cannot be built with MIDI."
        warn "  Arch: pacman -S alsa-lib | Debian: apt install libasound2-dev"
    fi

    # Programs the binaries EXEC (not linked libraries): without them the build
    # is fine, but the feature they power degrades at run time, so this is a
    # warning, never an error.
    #   pw-cat -> VU meters (rust/crates/holaf-ui/src/capture.rs)
    #   pactl  -> app list / app move (rust/crates/holaf-pw/src/sink_inputs.rs)
    local rt_missing=()
    has pw-cat || rt_missing+=("pw-cat (Arch: pipewire-audio | Debian/Fedora: pipewire-bin/pipewire-utils)")
    has pactl  || rt_missing+=("pactl (Arch: libpulse | Debian/Fedora: pulseaudio-utils)")
    for m in "${rt_missing[@]:-}"; do
        [[ -n "$m" ]] || continue
        warn "missing runtime helper: $m"
        warn "  the build is unaffected; that feature stays inert until it is installed"
    done
}

build_icons() {
    hdr "Step 1/4 — Building icon set"
    if [[ -x "$ROOT_DIR/assets/icons/build_icons.sh" ]]; then
        "$ROOT_DIR/assets/icons/build_icons.sh"
    else
        warn "assets/icons/build_icons.sh not found, skipping icons"
        return 0
    fi
    ok "Icons ready in $DIST_DIR/icons/"
}

build_rust_binaries() {
    # ========================================================================
    #  Rust UI + daemon/CLI (holaf-ui, holafd, holafctl) — BEFORE PyInstaller
    # ========================================================================
    #  Since migration B1 the app's window IS the Rust UI: `main.py` locates
    #  `holaf-ui` and runs it (the Qt interface is only the `HOLAF_UI=python`
    #  escape hatch).  packaging/holaf-mix.spec embeds all three binaries as
    #  data files, so they must exist *before* PyInstaller runs.
    #
    #  Therefore this step stays strictly best-effort: a machine without a Rust
    #  toolchain keeps producing a working Python+Qt build exactly like before
    #  (the launcher then logs that holaf-ui is missing and starts Qt).
    # ========================================================================
    hdr "Step 2/4 — Building Rust UI + daemon + CLI"

    # The system-tray icon (cargo feature `tray`) is ON BY DEFAULT here: it is
    # part of the packaged app.  HOLAF_NO_TRAY=1 (or the legacy HOLAF_TRAY=0)
    # removes it; the env is inherited by scripts/build_rust.sh, which also
    # falls back to a tray-less build if the feature fails to compile.
    # The backend is ksni (D-Bus), so no GTK3/appindicator package is involved.
    if [[ "${HOLAF_NO_TRAY:-0}" == "1" || "${HOLAF_TRAY:-1}" == "0" ]]; then
        log "Tray icon DISABLED (HOLAF_NO_TRAY/HOLAF_TRAY=0)"
    else
        log "Tray icon: ENABLED by default (feature \`tray\`; HOLAF_NO_TRAY=1 to drop it)"
    fi

    local script="$ROOT_DIR/scripts/build_rust.sh"
    if [[ ! -x "$script" ]]; then
        warn "scripts/build_rust.sh not found/executable — Python+Qt build"
        warn "  (main.py will fall back to the Qt UI: holaf-ui is not built)"
        return 0
    fi
    if ! has cargo; then
        warn "cargo not found on PATH — Python+Qt build."
        warn "  Install Rust (rustup or your distro's package) to embed holaf-ui."
        return 0
    fi

    # bindgen (libspa-sys) needs libclang; on Arch it lives in /usr/lib with
    # no versioned dir, which is not always in clang-sys' search path.
    # Harmless on distros where it is found anyway.
    if [[ -z "${LIBCLANG_PATH:-}" ]] && [[ -e /usr/lib/libclang.so ]]; then
        export LIBCLANG_PATH=/usr/lib
    fi

    if "$script" "$DIST_DIR"; then
        ok "Rust binaries published to $DIST_DIR :"
        # Say plainly what got embedded, with sizes: the UI is the binary a user
        # notices (it is the window), and a missing one is the difference
        # between the Rust UI and the Qt fallback.
        local b
        for b in holaf-ui holafd holafctl; do
            if [[ -x "$DIST_DIR/$b" ]]; then
                ok "  $b ($(du -h --apparent-size "$DIST_DIR/$b" | cut -f1))"
            elif [[ "$b" == "holaf-ui" ]]; then
                warn "  holaf-ui MISSING — the bundle will start the Qt UI instead"
            else
                warn "  $b missing"
            fi
        done
    else
        warn "Rust build failed — continuing with a Python+Qt bundle"
        warn "  (main.py will fall back to the Qt UI: see the log above)"
    fi
    return 0
}

build_executable() {
    local mode="${1:-onefile}"
    hdr "Step 3/4 — Building $mode executable with PyInstaller"

    if [[ ! -f "$SPEC_FILE" ]]; then
        err "Spec file not found: $SPEC_FILE"
        err "Did you run ./build.sh from the project root?"
        exit 1
    fi

    # Build mode is selected through the environment so the spec has a single,
    # always-valid source: no sed-patching (which silently produced an invalid
    # spec whenever the EXE/COLLECT layout changed).
    case "$mode" in
        onefile|onedir) ;;
        *) err "Unknown build mode: $mode"; exit 1 ;;
    esac
    export HOLAF_BUILD_MODE="$mode"

    # Clean only the pyinstaller work dir, not the whole build/
    rm -rf "$PYI_WORK" "$PYI_DIST"
    mkdir -p "$PYI_WORK" "$PYI_DIST"

    log "Running PyInstaller ($mode, this takes 2-5 minutes the first time)..."
    pyinstaller \
        --noconfirm \
        --workpath "$PYI_WORK" \
        --distpath "$PYI_DIST" \
        "$SPEC_FILE"

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
    hdr "Step 4/4 — Building distribution tarball"

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

    # --- Rust UI + core (holaf-ui, holafd, holafctl) ----------------------
    # `main.py` launches the Rust UI (`holaf-ui`) and the Qt fallback uses the
    # Rust daemon (`holafd`), so build.sh builds them *before* PyInstaller and
    # the spec embeds them inside the bundle (onefile → sys._MEIPASS, onedir →
    # <bundle>/_internal/).  We ALSO ship the binaries at the top level of the
    # tarball: they sit next to holaf-mix, which is the first path the launchers
    # probe in a frozen app (no extraction, no chmod needed), and holafctl is
    # the CLI users invoke directly.
    # When absent (plain local build without cargo), we say so and continue:
    # the Python+Qt bundle must keep working exactly as before.
    local rust_bins=()
    local b
    for b in holaf-ui holafd holafctl; do
        if [[ -x "$DIST_DIR/$b" ]]; then
            cp "$DIST_DIR/$b" "$staging/"
            chmod +x "$staging/$b"
            rust_bins+=("$b")
        fi
    done
    if ((${#rust_bins[@]})); then
        ok "Rust binaries included: ${rust_bins[*]}"
    else
        warn "Rust binaries not built, skipping (run scripts/build_rust.sh first)"
    fi
    if [[ ! -x "$staging/holaf-ui" ]]; then
        warn "holaf-ui is NOT in this tarball: holaf-mix will start the Qt UI"
    fi

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
    exe)     build_icons; build_rust_binaries; build_executable "$MODE" ;;
    build)   build_icons; build_rust_binaries; build_executable "$MODE" ;;
    all-tar) build_icons; build_rust_binaries; build_executable "$MODE"; make_tarball ;;
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
