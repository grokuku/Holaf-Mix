# Holaf-Mix — Installing and Building

Two distinct paths. Do not mix them:

| I want to… | Section |
|---|---|
| **use Holaf-Mix** | **§1 — install from a CI build** (download the GitHub Actions artifact, install the runtime dependencies, run) |
| check a runtime problem | §3 — troubleshooting |
| ship an Arch package | §4 — `packaging/PKGBUILD` (maintained option) |
| compile it myself | §5 — local build (developers / packagers) |

**The normal path is §1, and it requires no compilation at all**: every binary
(`holaf-mix`, `holaf-ui`, `holafd`, `holafctl`) is built by
`.github/workflows/build.yml` on Arch Linux and published as a downloadable
artifact.  §5 (local build) is kept for developers and packagers;
`packaging/PKGBUILD` (§4) stays maintained for whoever wants to distribute
Holaf-Mix as an Arch package, but the current delivery workflow does not use it.

---

## 1. Install from a CI build (normal path)

### 1.1 Download the artifact

GitHub → **Actions** → workflow **Build Holaf-Mix** → the run for the commit or
tag you want → **Artifacts** (bottom of the run page).

Each run publishes two independent artifact sets:

| Artifact | Content |
|---|---|
| `holaf-mix-<sha>-<arch>` | `holaf-mix-<version>-<arch>.tar.gz` (the distributable bundle) + `SHA256SUMS` |
| `holaf-rust-<sha>-<arch>` | `holaf-ui`, `holafd`, `holafctl` (release binaries, stripped) + `holaf-rust-<arch>.tar.gz` + `SHA256SUMS` |

`SHA256SUMS` is generated over everything the job produced in `dist/` (both
tarballs and the three Rust binaries), so it can list a file that is in the
*other* artifact.  It is not inside the tarball.

Take **`holaf-mix-<sha>-<arch>`** to use the application.  The
`holaf-rust-…` artifact is the same three Rust binaries on their own — useful to
update/replace them, or to script `holafd`/`holafctl` without the launcher.

### 1.2 What the tarball contains

The tarball extracts to a single `holaf-mix-<version>/` directory:

| File | Role |
|---|---|
| `holaf-mix` | the launcher (PyInstaller onefile bundle) — the entry point |
| `holaf-ui` | the application window (Rust/egui); drives PipeWire **in its own process** |
| `holafd` | Rust audio daemon — backend of the Qt fallback (`HOLAF_UI=python`) |
| `holafctl` | CLI that drives the daemon from a shell (`holafctl status`, …) |
| `icons/` | the icon set (`holaf-mix-{16..512}.png`, `.svg`, `.ico`) |
| `holaf-mix.desktop` | the desktop entry, with `__EXEC_PATH__` / `__ICON_PATH__` placeholders |
| `BUILD.md`, `README.md`, `ROADMAP.md` | the docs of the released revision |

`holaf-ui`, `holafd` and `holafctl` are shipped **twice over**: embedded inside
`holaf-mix` (extracted to `sys._MEIPASS` at launch, exec bit restored by the
launcher) and present as plain files next to it.  Consequence: **no Rust
toolchain, no compiler and no Python packages are needed** on the target
machine — only the runtime dependencies below.

### 1.3 Runtime dependencies (install these — this is all you need)

The binaries from CI link against and `exec` the files below.  It is the same
set `packaging/PKGBUILD` declares as `depends`: installing it is all the
downloaded tarball needs.  None of the §5 build dependencies (compiler,
headers, Rust, pip packages) is required.

```bash
# Arch / CachyOS
sudo pacman -S pipewire pipewire-audio pipewire-pulse wireplumber libpulse \
               alsa-lib swh-plugins

# Debian / Ubuntu
sudo apt install pipewire pipewire-bin pipewire-pulse wireplumber \
                 pulseaudio-utils libasound2 swh-plugins tap-plugins
```

Then, for the **TUBE WARMTH** strip (optional, one LADSPA file):
`sudo pacman -S --needed base-devel git` then an AUR helper —
`yay -S tap-plugins` (Arch: **AUR only**, there is no repo package) or
`sudo apt install tap-plugins` (Debian/Ubuntu, in `main`).  Without
`/usr/lib/ladspa/tap_tubewarmth.so` every other strip works; the UI simply does
not offer that FX (it checks the plugin file before listing it).

* `pipewire` + `pipewire-pulse` + `wireplumber` — the audio server, the Pulse
  API shim and a session manager. **Do not install `pulseaudio` next to
  `pipewire-pulse`**: both provide `pulse-native-provider` and pacman/apt refuse
  the combination. Holaf-Mix is PipeWire-only.
* `pipewire-audio` (Debian: `pipewire-bin`) — provides `pw-cat`, used to record
  each strip's VU meter.  (`pipewire-bin` also carries `pw-cli`, `pw-dump`,
  `pw-link`, `pw-metadata`, `pw-top`.)
* `libpulse` (Debian: `pulseaudio-utils`) — provides `/usr/bin/pactl`, which
  lists and moves application streams.
* `alsa-lib` (Debian: `libasound2`) — ALSA sequencer, i.e. MIDI input from the
  controller.
* `swh-plugins` — the LADSPA FX (`gate_1410`, `mbeq_1197`, `sc4_1882`).
* `tap-plugins` — the fourth LADSPA FX, `tap_tubewarmth` (the **TUBE WARMTH**
  strip).  AUR-only on Arch (see the command above), a normal package on
  Debian/Ubuntu.  Missing ⇒ that one strip is not offered, nothing else breaks.
* **No tray package.**  The notification icon is a D-Bus
  `org.kde.StatusNotifierItem` (`ksni` backend): `holaf-ui` links **no GTK3**
  and needs no `libappindicator`.  It only needs a StatusNotifier *host*, which
  every desktop already runs — see §3.
* Optional: `gnome-shell-extension-appindicator` (GNOME panel host for the tray
  icon), `mesa` (GL driver), `xdg-utils` (open links from the context menus),
  `gtk3` **only if you use the Qt fallback** (`HOLAF_UI=python`, whose GTK
  platform theme loads the system GTK3).

Missing helpers degrade gracefully (`pactl` errors logged once, meter at 0,
empty app list) but the two hard links (`libpipewire-0.3.so.0`, `libasound.so.2`)
are not optional.

### 1.4 Unpack and run

```bash
# optional but cheap: the artifact ships SHA256SUMS next to the tarball
# (--ignore-missing: it also lists files that belong to the other artifact)
sha256sum -c SHA256SUMS --ignore-missing

tar xzf holaf-mix-<version>-<arch>.tar.gz -C ~/apps/
cd ~/apps/holaf-mix-<version>
./holaf-mix
```

The first launch creates `config.json` next to the executable (§2).

`holaf-mix` **is a launcher**: it resolves `holaf-ui` (next to itself, then in
`sys._MEIPASS`, then `rust/target/`, then `$PATH`; `HOLAF_UI_BIN` overrides) and
runs it with `--config <next to holaf-mix>`.  If `holaf-ui` is missing it logs a
WARNING and starts the Qt UI (`HOLAF_UI=python` picks that explicitly), where
`holafd` is started on demand with `--backend pipewire` (and the classic Python
engine is the last resort, `HOLAF_USE_PYTHON=1`).  `./holafctl --help` drives the
daemon from a shell (`HOLAF_SOCKET` must match — default `/tmp/holaf-$USER.sock`).

### 1.5 Optional: system-wide install without a package

No package needed: copy the files where you want them.  Run these from the
unpacked `holaf-mix-<version>/` directory:

```bash
sudo install -m755 holaf-mix holaf-ui holafd holafctl /usr/local/bin/
sudo install -dm755 /usr/local/share/holaf-mix/icons
sudo install -m644 icons/*.png icons/*.svg /usr/local/share/holaf-mix/icons/

# The .desktop file ships in the tarball, with placeholders to substitute
sudo sed -e "s|__EXEC_PATH__|/usr/local/bin|g" \
         -e "s|__ICON_PATH__|/usr/local/share/holaf-mix/icons|g" \
         holaf-mix.desktop \
       | sudo tee /usr/local/share/applications/holaf-mix.desktop > /dev/null

sudo update-desktop-database /usr/local/share/applications
```

The app then shows up in **KDE Menu → Multimedia → Holaf-Mix**.  Uninstall with
`sudo rm` of those paths.  For a *managed* install (dependency resolution,
`pacman -Rns` uninstall, icon cache hooks) use the Arch package in §4.

---

## 2. Configuration File Location

`src/config/settings.py` automatically detects whether the app is running from
source or from a PyInstaller bundle:

- **From source**: `<repo>/config.json`
- **From bundle**: `<dir-of-binary>/config.json`

This means:
- Running `./holaf-mix` from the unpacked tarball → `./config.json` (next to it)
- Installed as an Arch package (§4) → `/usr/lib/holaf-mix/config.json` (writable
  because `/usr/lib/holaf-mix/` is owned by the package and not stripped of
  write perms in the PKGBUILD)

If you prefer the XDG standard (`~/.config/holaf-mix/config.json`), patch
`src/config/settings.py` to use:

```python
CONFIG_FILE = os.path.join(
    os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")),
    "holaf-mix", "config.json"
)
```

---

## 3. Troubleshooting (runtime)

These apply whatever the origin of the binaries — tarball from CI or Arch
package.

### "libpipewire not found" at runtime

The binary depends on the system PipeWire client library. Install:

```bash
sudo pacman -S pipewire              # libpipewire-0.3.so.0
```

That is only the *client* library: the mixer also needs a server and a session
manager, and it spawns two external commands at run time —

```bash
sudo pacman -S pipewire pipewire-audio pipewire-pulse wireplumber libpulse
# Debian: pipewire pipewire-bin pipewire-pulse wireplumber pulseaudio-utils
```

| Command | Arch package | What it powers |
|---|---|---|
| `pactl` | `libpulse` (Debian: `pulseaudio-utils`) | app list and app→bus move; needs `pipewire-pulse` to answer |
| `pw-cat` | `pipewire-audio` (Debian: `pipewire-bin`) | VU meters (one recording child per source) |
| `pw-cli`, `pw-dump`, `pw-link`, `pw-metadata`, `pw-top` | `pipewire` | Python engine, graph discovery, quantum diagnostics |

All of them degrade gracefully (meter at 0, empty app list, `pactl` errors
logged once) — a missing helper never takes the window down.

### "No LADSPA plugins found"

```bash
sudo pacman -S swh-plugins           # Debian: swh-plugins (same name)
```

Verify:

```bash
ls /usr/lib/ladspa/
# Should contain: gate_1410.so  mbeq_1197.so  sc4_1882.so  ...
```

The fourth FX, `noise_cancel`, needs `librnnoise_ladspa.so`
(label `noise_suppressor_stereo`). **No distro packages it** (Arch `rnnoise` is
the library only, without the LADSPA wrapper): build
[noise-suppression-for-voice](https://github.com/werman/noise-suppression-for-voice)
and drop `librnnoise_ladspa.so` into `/usr/lib/ladspa/`.

### Window does not open / wrong toolkit (Wayland vs X11)

`holaf-ui` is a **Wayland** client: `rust/crates/holaf-ui/Cargo.toml` compiles
`eframe` with `features = ["glow", "wayland", "default_fonts"]`, so the binary
has no X11 backend at all — that is what stops a Wayland desktop from showing
it with the "X11" application icon (or from asking for an X server through
XWayland) and what makes the width lock use `xdg_toplevel.set_min_size`.

Build requirements (needed by `winit`'s Wayland backend at compile time; the
`.so` files are `dlopen`ed at run time, so they do **not** show up in `ldd`
unless the `tray` feature is on — GTK3 then pulls `libwayland-client` and the
GDK X11 stack, which is GTK and not the window backend):

| | Arch | Debian / Ubuntu |
|---|---|---|
| client + protocols | `wayland`, `wayland-protocols` | `libwayland-dev`, `wayland-protocols`, `libwayland-bin` |
| keyboard | `libxkbcommon` | `libxkbcommon-dev` |

```bash
sudo pacman -S --needed wayland wayland-protocols libxkbcommon
sudo apt install libwayland-dev wayland-protocols libwayland-bin libxkbcommon-dev
```

An **X11-only** session still works, but only from a build that opts in:
`cargo build -p holaf-ui --features x11` (that additionally needs
`libxkbcommon-x11` / `libxkbcommon-x11-dev`).  The shipped package and the CI
artifact are Wayland-only, so Wayland support cannot regress unnoticed.

No display at all (a container, an SSH session): `run_native` reports it and
suggests the offscreen renderer,
`cargo run -p holaf-ui --bin holaf-ui-snapshot -- --out /tmp/holaf-ui.png`.

### VU meters stay dark

The meters record each strip's source with `pw-cat`, so three things must hold:

```bash
command -v pw-cat            # pipewire-audio (Debian: pipewire-bin)
holafctl meter-sources       # one source per strip, from the engine
journalctl --user -b | grep metering   # what the UI actually recorded
```

`pw-cat --target` matches a **node name**; the engine reports PulseAudio source
names (`<node>.monitor`, the vocabulary the Python `sounddevice` engine needed).
The UI strips the `.monitor` suffix and adds `stream.capture.sink=true`, which
is what makes a capture stream link to that sink's `monitor_FL/FR`.  Without
either, `pw-cat` silently records the **default source** and every bar stays at
zero — the log line `metering: recording <source> for <uid>` shows the resolved
target, and any `pw-cat` error is echoed as `metering: <uid>: …`.

### Tray icon missing (Wayland / GNOME)

The tray icon (built in by default) is a **D-Bus StatusNotifierItem**
(`ksni` backend — no GTK involved) and needs a **StatusNotifier host** to be
drawn: KDE/Plasma ships `kstatusnotifieritem`, GNOME needs
`gnome-shell-extension-appindicator`, sway needs `swaybar`'s tray, `waybar` has
one built in.  The app asks D-Bus whether `org.kde.StatusNotifierWatcher` has an
owner before claiming a tray; with no host — or no session bus, or no display —
the log says

```
tray: unavailable (no StatusNotifier host: org.kde.StatusNotifierWatcher has no owner)
      - closing the window will quit, not hide
```

and the close button then **quits** (so the app can never be hidden
unreachably).  With a host, a left click opens the menu (Show/Hide, Restart
audio engine, Quit).

### "libgtk-3.so.0: cannot open shared object file" at runtime

`holaf-ui` does **not** link GTK any more (check with
`ldd $(command -v holaf-ui) | grep -i gtk` — it must print nothing).  If that
error appears it comes from the **Qt fallback** (`HOLAF_UI=python`), whose GTK
platform theme (`PySide6/Qt/plugins/platformthemes/libqgtk3.so`) loads the
system GTK3 on purpose — the PyInstaller bundle deliberately excludes a frozen
GTK3.  Install it (Arch `sudo pacman -S gtk3`, Debian `sudo apt install
libgtk-3-0`), or drop `libqgtk3.so` / stay on the Rust UI.

### Icon doesn't appear in KDE tray

After install:

```bash
# Force a refresh of the icon cache
kbuildsycoca6 --noincremental
# or
sudo gtk-update-icon-cache -f -t /usr/share/icons/hicolor
```

### MIDI controller not detected

```bash
# Check that the user is in the 'audio' group
groups | grep audio
# If not:
sudo usermod -aG audio $USER
# Then log out and back in
```

### "Permission denied" when saving config

The PyInstaller bundle is read-only inside. The patch in `settings.py` writes
the config **next to** the binary, not inside it. If you copied the binary
to a read-only location (e.g. `/usr/bin/` directly), move it to
`/usr/local/bin/` or `/opt/holaf-mix/` instead.

---

## 4. Arch package: `packaging/PKGBUILD` (distribution option)

`packaging/PKGBUILD` is **maintained** and builds/installs Holaf-Mix as a real
Arch package: `/usr/bin/holaf-mix`, `holaf-ui`, `holafd`, `holafctl`, the
`.desktop` entry, the hicolor icons, `depends` resolution, `post_install` icon
cache refresh and a clean `pacman -Rns` uninstall.

It is **not** part of the project's delivery workflow: the shipped artifacts come
from GitHub Actions (§1), and the tarball above does not need it.  Use the
PKGBUILD when you want a package manager to own the files (own machine, or
distributing to other Arch users); it is also the reference for the runtime
dependency list in §1.3 — its `depends` array is the authoritative, verified
version of it.

```bash
cd packaging
makepkg -si          # build + install deps + install
makepkg -s           # build + install deps (no install)
makepkg --clean      # clean build dir
```

Installs:

- `/usr/bin/holaf-mix` (launcher executable)
- `/usr/bin/holaf-ui` (Rust/egui UI — the app's window)
- `/usr/bin/holafd` + `/usr/bin/holafctl` (Rust audio core, Qt fallback backend)
- `/usr/share/applications/holaf-mix.desktop` (KDE menu entry)
- `/usr/share/icons/hicolor/{16,...,512}x{16,...,512}/apps/holaf-mix.png`
- `/usr/share/icons/hicolor/scalable/apps/holaf-mix.svg`
- `/usr/share/holaf-mix/icons/` (full set)

The app shows up in **KDE Menu → Multimedia → Holaf-Mix**.

To uninstall: `sudo pacman -Rns holaf-mix`.

Building through `makepkg` needs the build dependencies of §5.1 (it runs
`scripts/build_rust.sh` + PyInstaller itself) — the `depends` array only covers
what the *installed* app needs at runtime.

---

## 5. Building locally (developers / packagers)

Everything below compiles the application from source.  It is the same sequence
`.github/workflows/build.yml` runs — which is why you don't need it to *use*
the app (§1): the CI does it on Arch Linux for every push, and archives the
result.

### 5.1 Install build dependencies

On CachyOS / Arch:

```bash
sudo pacman -S --needed \
    base-devel pkgconf clang rust \
    python-pip \
    librsvg \
    imagemagick \
    pipewire pipewire-audio pipewire-pulse wireplumber \
    libpulse alsa-lib swh-plugins

# No tray package: the tray icon is a D-Bus StatusNotifierItem (ksni), so the
# Rust build needs neither gtk3 nor libayatana-appindicator.
#
# pyinstaller is NOT packaged for Arch (no [extra], no AUR under that name):
# installing it via pip (the upstream-recommended method) is simpler and keeps
# versions reproducible.
pip install --user \
    PySide6 \
    sounddevice \
    numpy \
    mido \
    python-rtmidi \
    pyinstaller
```

Why each system package (`pkgconf`, `clang`, `pipewire`… — verified against the
binary, not guessed: `ldd rust/target/release/holaf-ui` plus a grep for the
external commands the code spawns):

| Package | Needed for |
|---|---|
| `base-devel`, `rust`, `pkgconf`, `clang` | build: cargo/rustc, `.pc` probing by the `-sys` crates, `libclang.so` for bindgen (`libspa-sys`, `pipewire-sys`) |
| `alsa-lib` | **hard link** `libasound.so.2` — ALSA sequencer backend of `midir` (MIDI Mix) |
| `pipewire` | **hard link** `libpipewire-0.3.so.0` (`holaf-ui`, `holafd`) + `pw-cli`, `pw-dump`, `pw-link`, `pw-metadata`, `pw-top` |
| `pipewire-audio` | `pw-cat` — one child process per VU-meter source (`rust/crates/holaf-ui/src/capture.rs`) |
| `pipewire-pulse` | the PulseAudio-compatible **server** `pactl` talks to (replaces `pulseaudio`, which conflicts with it) |
| `libpulse` | `/usr/bin/pactl` itself — app list / app move (`rust/crates/holaf-pw/src/sink_inputs.rs`) |
| `wireplumber` | session manager: builds the nodes/monitors the mixer renames, links and meters |
| `swh-plugins` | the LADSPA plugins the FX strips load: `gate_1410`, `mbeq_1197`, `sc4_1882` |
| `tap-plugins` *(runtime, AUR on Arch)* | `tap_tubewarmth` — the TUBE WARMTH strip (`tap-plugins` is a normal package on Debian/Fedora) |

> `ladspa-plugins` does not exist on Arch (nor on Debian): the LADSPA plugins
> come from **`swh-plugins`**. `rnnoise` ships the denoiser *library* only — the
> `noise_cancel` strip needs `librnnoise_ladspa.so`, which is not packaged
> anywhere and must be built from
> [noise-suppression-for-voice](https://github.com/werman/noise-suppression-for-voice).

> **Why is GTK3 gone from this list?** The tray icon (cargo feature `tray`,
> **on by default** in `scripts/build_rust.sh`) used to be `libappindicator`,
> i.e. GTK3 linked in and `libayatana-appindicator3.so.1` `dlopen`ed.  Since the
> 2026-11-15 migration it is **`ksni`**: a pure D-Bus
> `org.kde.StatusNotifierItem` with no GTK, no GMainLoop and no appindicator
> library — `gtk 0.18`, a closed/unmaintained line, is out of the dependency
> graph entirely (`docs/dependency-audit.md`).  Nothing to install, and
> `ldd holaf-ui | grep gtk` is empty.  `HOLAF_NO_TRAY=1` still skips the icon
> deliberately; `HOLAF_TRAY_GTK=1` builds the old libappindicator backend
> (feature `tray-gtk`) if you ever need it — that one *does* need `gtk3` +
> `libayatana-appindicator`.

> **Why no `python-pyinstaller`?** There is no such package: neither in
> `[extra]` nor in the AUR (the AUR package is plain `pyinstaller`).
> Pip install is the upstream-recommended install method (it's a build
> tool, not a runtime dep, so system-package-staleness isn't an issue).
> `packaging/PKGBUILD` therefore does not list it either — its `build()`
> stops with an explicit message when `pyinstaller` is missing.
>
> **Why no `python-pyside6`?** The Arch package is named `pyside6` (not
> `python-pyside6`), and pip gives you the exact version pinned in
> `requirements.txt` rather than whatever the distro built.
> If you'd rather use the system package, drop the `PySide6` line above
> and `sudo pacman -S pyside6`.
>
> **Why `python-sounddevice` is not in the PKGBUILD `depends`:** it is AUR-only
> (no official-repo package exists), and `makepkg` cannot resolve AUR
> dependencies. It only feeds the *Python* metering fallback; the Rust UI
> records with `pw-cat`. Install it from the AUR when you want that fallback.

On Debian / Ubuntu:

```bash
sudo apt install \
    build-essential pkg-config clang \
    python3-pip \
    librsvg2-bin \
    imagemagick \
    libpipewire-0.3-dev libspa-0.2-dev libpulse-dev \
    pipewire pipewire-bin pipewire-pulse wireplumber pulseaudio-utils \
    libasound2-dev swh-plugins tap-plugins

pip3 install --user --break-system-packages \
    PySide6 \
    sounddevice \
    numpy \
    mido \
    python-rtmidi \
    pyinstaller
```

Debian 12 notes (checked with `apt-cache policy` / `dpkg -S`):

* `pactl` lives in **`pulseaudio-utils`** (not in `libpulse0`, unlike Arch where
  it is in `libpulse`).
* `pw-cat`, `pw-cli`, `pw-dump`, `pw-link`, `pw-metadata`, `pw-top` live in
  **`pipewire-bin`**.
* There is no `ladspa-plugins` and no `librnnoise-ladspa` in bookworm — install
  `swh-plugins` for gate/mbeq/sc4 and build the rnnoise LADSPA plugin yourself.
* The `rustc` in bookworm (1.63) is far below the workspace MSRV: use `rustup`.

### 5.2 Build the icons

```bash
./assets/icons/build_icons.sh
```

Output: `dist/icons/holaf-mix-{16,22,24,32,48,64,128,256,512}.png`,
`holaf-mix.ico`, `holaf-mix.svg`.

You can preview them with:

```bash
xdg-open dist/icons/holaf-mix-256.png
```

### 5.3 Build the executable

```bash
./build.sh
```

That's it — the first build takes 2-5 minutes (PyInstaller compiles the
bootloader and analyzes imports), subsequent builds are ~30s.

This runs:
1. `scripts/build_rust.sh` → `dist/holaf-ui` (the app's window) +
   `dist/holafd` + `dist/holafctl` (Rust audio core).
   **Best effort**: without a Rust toolchain (or if the build fails) the script
   logs a warning and continues — you then get a Python+Qt bundle, and `main.py`
   starts the Qt UI instead of `holaf-ui`.
2. `pyinstaller packaging/holaf-mix.spec` → `dist/holaf-mix` (onefile binary)
   with `holaf-ui`/`holafd`/`holafctl` embedded (extracted to `sys._MEIPASS` at
   launch; the launcher restores their executable bit)
3. Copies icons next to the binary

To build only the Rust side (e.g. after a `git pull` in `rust/`):

```bash
./scripts/build_rust.sh          # cargo build --release -p holaf-ui -p holafd -p holafctl
```

### 5.4 Run it from `dist/`

```bash
./dist/holaf-mix
# or, without touching the launcher:
ln -s "$PWD/dist/holaf-mix" ~/.local/bin/holaf-mix
```

Same launcher logic and same runtime dependencies as §1.4/§1.3.

### 5.5 One-file vs Onedir Builds

`packaging/holaf-mix.spec` produces a **onefile** build by default:
- Single `dist/holaf-mix` binary (~80-120 MB)
- Slow first launch (~1-2s extraction to `/tmp`)
- Easy to distribute (one file) — this is what CI uploads

For a faster-launching **onedir** build, uncomment the `COLLECT(...)` block at
the bottom of the spec file and remove the EXE's bundled binaries/datas, or run
`./build.sh --onedir` (the CI does this when triggered with `mode=onedir|both`):

```python
exe_onedir = EXE(
    pyz, a.scripts, [],
    name='holaf-mix',
    ...
)
COLLECT(exe_onedir, a.binaries, a.zipfiles, a.datas, name='holaf-mix')
```

This produces `dist/holaf-mix/holaf-mix` + `dist/holaf-mix/_internal/`.
Faster launch, larger footprint, more files to distribute.

---

## 6. What the build script does (in order)

1. `check_dependencies()` — verifies pyinstaller, rsvg-convert, magick, Python deps
   (and *warns* if GTK3 dev is missing, which only costs the tray icon)
2. `build_icons()` — renders the SVG to 9 PNG sizes + ICO
3. `build_rust_binaries()` — `scripts/build_rust.sh`: `holaf-ui` (**with the
   tray icon**, `HOLAF_NO_TRAY=1` to drop it) + `holafd` + `holafctl` → `dist/`
4. `build_executable()` — runs PyInstaller on `holaf-mix.spec` (embeds the three
   binaries and strips the system-provided GTK3/GLib + PipeWire client libs)
5. (optional) `make_tarball()` — bundles everything for distribution

Run any step in isolation:

```bash
./build.sh --icons-only     # just rebuild icons
./build.sh --executable     # skip icon check
./build.sh --clean          # nuke build/ and dist/
./build.sh --tarball        # also produce dist/holaf-mix-<ver>.tar.gz
```
