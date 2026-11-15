#!/usr/bin/env bash
# ============================================================================
#  build.sh — Build Holaf-Mix (100 % Rust) and package the distribution tarball
# ============================================================================
#  Holaf-Mix ships as Rust binaries only: `holaf-ui` (the app), `holafd` (the
#  audio daemon, an alternative/CLI backend) and `holafctl` (the CLI).  There is
#  no Python, no PyInstaller and no venv anywhere in the build.
#
#  Usage:
#    ./build.sh                  # icons + cargo build (holaf-ui, holafd, holafctl)
#    ./build.sh --tarball        # ... and produce dist/holaf-mix-<ver>-<arch>.tar.gz
#    ./build.sh --run            # build, then launch dist/holaf-ui
#    ./build.sh --icons-only     # just rebuild the icon set
#    ./build.sh --check          # verify the build dependencies
#    ./build.sh --install-deps   # install the system dependencies
#    ./build.sh --clean          # remove build artifacts (build/ and dist/)
#
#  Env:
#    HOLAF_NO_TRAY=1             # build without the tray icon
#                                # (default: tray ON — D-Bus ksni, no GTK)
#    HOLAF_TRAY_GTK=1            # force the old libappindicator backend
#                                # (needs gtk3 + libayatana-appindicator)
#    HOLAF_UI_X11=1              # add the X11 window backend (X11-only session)
#
#  Quick start on CachyOS / Arch:
#    ./build.sh --install-deps   # one-time setup
#    ./build.sh --run            # build + launch
#
#  The normal user path is NOT this script: it is the CI artefact
#  (`.github/workflows/build.yml` → `holaf-mix-<sha>-<arch>`), unpacked and
#  launched with `./holaf-ui`.  See BUILD.md.
# ============================================================================

set -euo pipefail

# --- Paths -------------------------------------------------------------------
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
cd "$SCRIPT_DIR"

ROOT_DIR="$SCRIPT_DIR"
DIST_DIR="$ROOT_DIR/dist"
BUILD_DIR="$ROOT_DIR/build"

# --- App metadata ------------------------------------------------------------
APP_NAME="holaf-mix"

# The version lives in the Rust workspace (rust/Cargo.toml, [workspace.package]);
# git describe is only a fallback for a checkout where that file is missing.
get_version() {
    local version=""
    if [[ -f "$ROOT_DIR/rust/Cargo.toml" ]]; then
        version=$(awk '
            /^\[workspace\.package\]/ { in_section = 1; next }
            /^\[/                     { in_section = 0 }
            in_section && /^version[[:space:]]*=/ {
                gsub(/[",]/, "", $3); print $3; exit
            }
        ' "$ROOT_DIR/rust/Cargo.toml")
    fi
    if [[ -n "$version" ]]; then
        printf '%s\n' "$version"
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
# Notes on the package names (verified, not guessed):
#   - "librsvg2" is wrong on Arch — the package is `librsvg` (rsvg-convert).
#   - "ladspa-plugins" does NOT exist: the four LADSPA files come from
#     `swh-plugins` -> /usr/lib/ladspa/{gate_1410,mbeq_1197,sc4_1882}.so
#   - PipeWire is the ONLY audio server supported: `pipewire-pulse` provides
#     `pulse-native-provider` exactly like `pulseaudio`, so installing both
#     triggers a hard pacman conflict.  `pactl` comes from `libpulse`.
#   - The tray is a D-Bus StatusNotifierItem (`ksni`): no GTK package at all.
SYSTEM_DEPS_ARCH=(
    # --- build toolchain -------------------------------------------------
    "base-devel"         # gcc/make (Rust link step)
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
    #     yay -S tap-plugins
    # Without it every OTHER strip works; the UI simply cannot insert TUBE
    # WARMTH (it checks /usr/lib/ladspa before offering the FX).
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
    # No tray package: the `tray` feature is ksni/D-Bus, so GTK3 and
    # libayatana-appindicator are not needed.
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
)

# --- Helpers -----------------------------------------------------------------
has() { command -v "$1" >/dev/null 2>&1; }

print_help() {
    # Print the leading docblock: every "#" comment line at the top of this
    # file, from line 2 until the first real statement.
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
        printf '\033[1;33m[build]\033[0m Continue? [y/N] '
        read -r answer
        if [[ ! "$answer" =~ ^[Yy]$ ]]; then
            err "Aborted by user."
            exit 4
        fi
    fi

    if [[ "$DISTRO_FAMILY" == "unknown" ]]; then
        err "Unsupported distro. Please install manually:"
        err "  Build:   rust (>= 1.98), pkgconf, clang (bindgen),"
        err "           rsvg-convert (Arch: librsvg | Debian: librsvg2-bin),"
        err "           imagemagick (magick), libpipewire/libspa dev headers,"
        err "           libasound dev headers (MIDI)"
        err "  Runtime: pipewire (+ pipewire-audio for pw-cat), pipewire-pulse,"
        err "           wireplumber, libpulse (pactl), alsa-lib, swh-plugins,"
        err "           and optionally tap-plugins (AUR on Arch)."
        err "  No tray package needed: the tray is a D-Bus StatusNotifierItem."
        exit 1
    fi

    case "$DISTRO_FAMILY" in
        arch)
            log "Installing system packages via pacman..."
            sudo pacman -S --needed --noconfirm "${SYSTEM_DEPS_ARCH[@]}" || {
                err "pacman install failed. Try: sudo pacman -S --needed ${SYSTEM_DEPS_ARCH[*]}"
                exit 1
            }
            ;;
        debian)
            log "Installing system packages via apt..."
            sudo apt update
            sudo apt install -y "${SYSTEM_DEPS_DEBIAN[@]}" || {
                err "apt install failed"
                exit 1
            }
            warn "Rust >= 1.98 is required; Debian/Ubuntu's rustc is older — use rustup."
            ;;
        fedora)
            log "Installing system packages via dnf..."
            sudo dnf install -y "${SYSTEM_DEPS_FEDORA[@]}" || {
                err "dnf install failed"
                exit 1
            }
            ;;
    esac

    ok "All dependencies installed."
    ok "  System deps: distro-appropriate packages above (no Python)"
}

check_deps() {
    log "Checking build dependencies..."

    local missing=()
    has cargo        || missing+=("cargo (install Rust: https://rustup.rs or the distro's rust package)")
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

    ok "Build tools present (cargo, rsvg-convert, magick)."

    # The tray icon needs no build package any more (ksni/D-Bus).  What is
    # worth warning about is the LADSPA set: the UI offers a strip per plugin
    # file it finds, so a missing one silently removes a feature.
    if [[ ! -f /usr/lib/ladspa/tap_tubewarmth.so ]]; then
        warn "tap_tubewarmth.so (TAP plugins) not found — the TUBE WARMTH strip"
        warn "  will not be offered by the UI.  Debian: apt install tap-plugins |"
        warn "  Fedora: dnf install ladspa-tap-plugins | Arch: AUR package tap-plugins"
    fi

    # MIDI (cargo feature `midi`, ON by default) is built against the ALSA
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
    hdr "Step 1/3 — Building icon set"
    if [[ -x "$ROOT_DIR/assets/icons/build_icons.sh" ]]; then
        "$ROOT_DIR/assets/icons/build_icons.sh"
    else
        warn "assets/icons/build_icons.sh not found, skipping icons"
        return 0
    fi
    ok "Icons ready in $DIST_DIR/icons/"
}

build_binaries() {
    # ========================================================================
    #  Release binaries: holaf-ui (the app) + holafd + holafctl
    # ========================================================================
    #  There is nothing else in the product: no launcher, no bundle, no
    #  interpreter.  `holaf-ui` is the entry point, `holafd` an alternative/
    #  automation backend and `holafctl` its CLI.
    # ========================================================================
    hdr "Step 2/3 — Building holaf-ui + holafd + holafctl (release)"

    # The system-tray icon (cargo feature `tray`) is ON BY DEFAULT here: it is
    # part of the packaged app.  HOLAF_NO_TRAY=1 (or the legacy HOLAF_TRAY=0)
    # removes it; the env is inherited by scripts/build_rust.sh, which falls
    # back to a tray-less build if the feature fails to compile.
    # The backend is ksni (D-Bus), so no GTK3/appindicator package is involved.
    if [[ "${HOLAF_NO_TRAY:-0}" == "1" || "${HOLAF_TRAY:-1}" == "0" ]]; then
        log "Tray icon DISABLED (HOLAF_NO_TRAY/HOLAF_TRAY=0)"
    else
        log "Tray icon: ENABLED by default (feature \`tray\`; HOLAF_NO_TRAY=1 to drop it)"
    fi

    local script="$ROOT_DIR/scripts/build_rust.sh"
    if [[ ! -x "$script" ]]; then
        err "scripts/build_rust.sh not found or not executable"
        exit 1
    fi

    # bindgen (libspa-sys) needs libclang; on Arch it lives in /usr/lib with
    # no versioned dir, which is not always in clang-sys' search path.
    # Harmless on distros where it is found anyway.
    if [[ -z "${LIBCLANG_PATH:-}" ]] && [[ -e /usr/lib/libclang.so ]]; then
        export LIBCLANG_PATH=/usr/lib
    fi

    # A Rust build failure is FATAL now: a product without holaf-ui does not
    # exist any more (the Python build used to be the fallback).
    if ! "$script" "$DIST_DIR"; then
        err "cargo build failed — nothing to package."
        exit 1
    fi

    local b
    for b in holaf-ui holafd holafctl; do
        if [[ ! -x "$DIST_DIR/$b" ]]; then
            err "expected binary missing after the build: $DIST_DIR/$b"
            exit 1
        fi
        ok "  $b ($(du -h --apparent-size "$DIST_DIR/$b" | cut -f1))"
    done
}

# Short usage note shipped inside the tarball: the tarball is what the user
# actually downloads, so the launch instructions must be impossible to miss.
write_usage_note() {
    local dest="$1/USAGE.txt"
    cat > "$dest" <<'EOF'
Holaf-Mix — lancement
=====================

    ./holaf-ui          # l'application (UI Rust, moteur PipeWire in-process)
    ./holaf-mix         # même chose (petit lanceur : exec ./holaf-ui "$@")
    ./holafd --help     # démon audio alternatif / automatisation
    ./holafctl --help   # CLI du démon

Aucun Python, aucun PyInstaller, aucun venv : ce dossier ne contient que des
binaires Rust.

Dépendances runtime (une seule fois) :
  Arch / CachyOS :
    sudo pacman -S pipewire pipewire-audio pipewire-pulse wireplumber \
                   libpulse alsa-lib swh-plugins
    yay -S tap-plugins        # optionnel : strip TUBE WARMTH (AUR)
  Debian / Ubuntu :
    sudo apt install pipewire pipewire-bin pipewire-pulse wireplumber \
                     pulseaudio-utils libasound2 swh-plugins tap-plugins

Configuration : config.json est cherché dans cet ordre
  1. --config FICHIER
  2. $HOLAF_CONFIG
  3. à côté du binaire (./config.json du dossier décompressé), s'il existe
  4. ./config.json du répertoire courant, s'il existe
  5. $XDG_CONFIG_HOME/holaf-mix/config.json (~/.config/holaf-mix/config.json)

Le premier lancement crée le fichier au premier emplacement inscriptible de
cette liste ; --help liste les options (--midi, --midi-list, ...).
Plus de détails : README.md et BUILD.md dans ce dossier.
EOF
}

make_tarball() {
    hdr "Step 3/3 — Building distribution tarball"

    local arch; arch="$(uname -m)"
    local tar_name="${APP_NAME}-${APP_VERSION}-${arch}.tar.gz"
    local staging_name="${APP_NAME}-${APP_VERSION}"
    local staging="$DIST_DIR/$staging_name"

    # The binaries are the product: refuse to ship a tarball without them.
    local b
    for b in holaf-ui holafd holafctl; do
        if [[ ! -x "$DIST_DIR/$b" ]]; then
            err "$DIST_DIR/$b is missing — run './build.sh' first."
            exit 1
        fi
    done

    rm -rf "$staging"
    mkdir -p "$staging"

    for b in holaf-ui holafd holafctl; do
        cp "$DIST_DIR/$b" "$staging/"
        chmod +x "$staging/$b"
    done

    # The `holaf-mix` habit, preserved by a 20-line shell script (no Python).
    cp "$ROOT_DIR/packaging/holaf-mix.sh" "$staging/holaf-mix"
    chmod +x "$staging/holaf-mix"

    if [[ -d "$DIST_DIR/icons" ]]; then
        cp -r "$DIST_DIR/icons" "$staging/icons"
    fi

    # Docs: the released revision's own, plus the usage note above.
    for doc in README.md BUILD.md ROADMAP.md; do
        [[ -f "$ROOT_DIR/$doc" ]] && cp "$ROOT_DIR/$doc" "$staging/"
    done
    if [[ -d "$ROOT_DIR/docs" ]]; then
        mkdir -p "$staging/docs"
        cp -r "$ROOT_DIR/docs/." "$staging/docs/"
    fi
    [[ -f "$ROOT_DIR/packaging/holaf-mix.desktop" ]] && \
        cp "$ROOT_DIR/packaging/holaf-mix.desktop" "$staging/"
    write_usage_note "$staging"

    tar -czf "$DIST_DIR/$tar_name" -C "$DIST_DIR" "$staging_name"
    rm -rf "$staging"

    ok "Tarball: $DIST_DIR/$tar_name"
    # Capture the listing first: piping `tar -t | head` under `set -o pipefail`
    # turns a truncated listing into SIGPIPE (tar exits 141) on a big tarball.
    local listing
    listing=$(tar -tzf "$DIST_DIR/$tar_name")
    ok "Contents ($(printf '%s\n' "$listing" | wc -l) entries, first 40):"
    printf '%s\n' "$listing" | sed -n '1,40p' | sed 's|^|  |'
    ls -la "$DIST_DIR/$tar_name"
}

run_binary() {
    local bin="$DIST_DIR/holaf-ui"
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
    ok "Cleaned ($BUILD_DIR, $DIST_DIR). Source files untouched."
}

# --- CLI parsing -------------------------------------------------------------
ACTION="build"
RUN_AFTER=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --install-deps) install_deps; exit 0 ;;
        --check)        check_deps; exit 0 ;;
        --icons-only)   ACTION="icons" ;;
        --tarball)      ACTION="all-tar" ;;
        --run)          RUN_AFTER=true ;;
        --clean)        clean; exit 0 ;;
        -h|--help)      print_help; exit 0 ;;
        -V|--version)   echo "$APP_NAME $APP_VERSION"; exit 0 ;;
        *)              err "Unknown arg: $1"; print_help; exit 1 ;;
    esac
    shift
done

# --- Main sequence -----------------------------------------------------------
hdr "Holaf-Mix build (v$APP_VERSION)"
check_deps

case "$ACTION" in
    icons)   build_icons ;;
    build)   build_icons; build_binaries ;;
    all-tar) build_icons; build_binaries; make_tarball ;;
esac

if $RUN_AFTER; then
    run_binary
else
    ok ""
    ok "Build complete."
    ok "  Run:   $DIST_DIR/holaf-ui"
    ok "  Or:    ./build.sh --run"
    ok "  Docs:  cat BUILD.md"
fi
