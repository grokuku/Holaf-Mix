# Building Holaf-Mix Standalone

This document explains how to build a standalone, ready-to-use Holaf-Mix binary
on **CachyOS / Arch Linux** (works on other Arch-derivatives and on Debian/Ubuntu
with minor adjustments).

---

## TL;DR (5 minutes)

```bash
cd ~/projects/Holaf-Mix
./build.sh
./dist/holaf-mix
```

That's it. The first build takes 2-5 minutes (PyInstaller compiles the bootloader
and analyzes imports). Subsequent builds are ~30s.

---

## Detailed Steps

### 1. Install build dependencies

On CachyOS / Arch:

```bash
sudo pacman -S --needed \
    base-devel pkgconf clang rust \
    python-pip \
    librsvg \
    imagemagick \
    pipewire pipewire-audio pipewire-pulse wireplumber \
    libpulse alsa-lib swh-plugins \
    gtk3 libayatana-appindicator

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
| `gtk3`, `libayatana-appindicator` | tray icon: GTK3 is hard-linked, the appindicator library is `dlopen`ed |

> `ladspa-plugins` does not exist on Arch (nor on Debian): the LADSPA plugins
> come from **`swh-plugins`**. `rnnoise` ships the denoiser *library* only — the
> `noise_cancel` strip needs `librnnoise_ladspa.so`, which is not packaged
> anywhere and must be built from
> [noise-suppression-for-voice](https://github.com/werman/noise-suppression-for-voice).

> **Why `gtk3` + `libayatana-appindicator`?** The tray icon (cargo feature
> `tray`) is **on by default** in `scripts/build_rust.sh`: `holaf-ui` links
> GTK3 and `dlopen`s `libayatana-appindicator3.so.1` at run time (the same two
> libraries are `depends` in `packaging/PKGBUILD`). Without them the build
> still succeeds — it retries **without** the tray icon and keeps `holaf-ui`.
> Set `HOLAF_NO_TRAY=1` to skip the icon deliberately.

> **Why no `python-pyinstaller`?** There is no such package: neither in
> `[extra]` nor in the AUR (the AUR package is plain `pyinstaller`).
> Pip install is the upstream-recommended install method (it's a build
> tool, not a runtime dep, so system-package-staleness isn't an issue).
> `packaging/PKGBUILD` therefore does not list it either — its `build()`
> stops with an explicit message when `pyinstaller` is missing.
>
> **Why no `python-pyside6`?** The Arch package is named `pyside6` (not
> `python-pyside6`), and pip gives you the exact version pinned in
> `requirements.txt` rather than whatever CachyOS built.
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
    libasound2-dev swh-plugins \
    libgtk-3-dev libayatana-appindicator3-dev

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

### 2. Build the icons

```bash
./assets/icons/build_icons.sh
```

Output: `dist/icons/holaf-mix-{16,22,24,32,48,64,128,256,512}.png`,
`holaf-mix.ico`, `holaf-mix.svg`.

You can preview them with:

```bash
xdg-open dist/icons/holaf-mix-256.png
```

### 3. Build the executable

```bash
./build.sh
```

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

### 4. Run it

```bash
./dist/holaf-mix
```

The first launch creates `config.json` next to the binary.  `holaf-mix` **is a
launcher**: it resolves `holaf-ui` (next to itself, then in `sys._MEIPASS`, then
`rust/target/`, then `$PATH`; `HOLAF_UI_BIN` overrides) and runs it with
`--config <next to holaf-mix>`.  If `holaf-ui` is missing it logs a WARNING and
starts the Qt UI (`HOLAF_UI=python` picks that explicitly), where `holafd` is
started on demand with `--backend pipewire` (and the classic Python engine is the
last resort, `HOLAF_USE_PYTHON=1`).  `./dist/holafctl --help` drives the daemon
from a shell (`HOLAF_SOCKET` must match — default `/tmp/holaf-$USER.sock`).

---

## Installation Options

### Option A: Just run from `dist/`

```bash
ln -s "$PWD/dist/holaf-mix" ~/.local/bin/holaf-mix
holaf-mix
```

### Option B: System-wide install (proper Arch way)

```bash
cd packaging
makepkg -si
```

This installs:
- `/usr/bin/holaf-mix` (launcher executable)
- `/usr/bin/holaf-ui` (Rust/egui UI — the app's window)
- `/usr/bin/holafd` + `/usr/bin/holafctl` (Rust audio core, Qt fallback backend)
- `/usr/share/applications/holaf-mix.desktop` (KDE menu entry)
- `/usr/share/icons/hicolor/{16,...,512}x{16,...,512}/apps/holaf-mix.png`
- `/usr/share/icons/hicolor/scalable/apps/holaf-mix.svg`
- `/usr/share/holaf-mix/icons/` (full set)

The app shows up in **KDE Menu → Multimedia → Holaf-Mix**.

To uninstall: `sudo pacman -Rns holaf-mix`

### Option C: Manual install (for non-Arch systems)

```bash
sudo install -m755 dist/holaf-mix /usr/local/bin/holaf-mix
sudo install -dm755 /usr/local/share/holaf-mix/icons
sudo install -m644 dist/icons/*.png /usr/local/share/holaf-mix/icons/
sudo install -m644 dist/icons/holaf-mix.svg /usr/local/share/holaf-mix/icons/

# Process the .desktop file
sudo sed -e "s|__EXEC_PATH__|/usr/local/bin|g" \
          -e "s|__ICON_PATH__|/usr/local/share/holaf-mix/icons|g" \
          packaging/holaf-mix.desktop \
        | sudo tee /usr/local/share/applications/holaf-mix.desktop > /dev/null

sudo update-desktop-database /usr/local/share/applications
```

---

## Runtime dependencies (what an installed Holaf-Mix needs)

Everything above is a *build* dependency. A user running the packaged app needs
far less — this is exactly what `packaging/PKGBUILD` declares as `depends`:

```bash
# Arch / CachyOS
sudo pacman -S pipewire pipewire-audio pipewire-pulse wireplumber libpulse \
               alsa-lib swh-plugins gtk3 libayatana-appindicator

# Debian / Ubuntu
sudo apt install pipewire pipewire-bin pipewire-pulse wireplumber \
                 pulseaudio-utils libasound2 libgtk-3-0 swh-plugins \
                 libayatana-appindicator3-1
```

* `pipewire` + `pipewire-pulse` + `wireplumber` — the audio server, the Pulse
  API shim and a session manager. **Do not install `pulseaudio` next to
  `pipewire-pulse`**: both provide `pulse-native-provider` and pacman/apt refuse
  the combination. Holaf-Mix is PipeWire-only.
* `pipewire-audio` (Debian: `pipewire-bin`) — provides `pw-cat`, used to record
  each strip's VU meter.
* `libpulse` (Debian: `pulseaudio-utils`) — provides `/usr/bin/pactl`, which
  lists and moves application streams.
* `alsa-lib` — ALSA sequencer, i.e. MIDI input from the controller.
* `swh-plugins` — the LADSPA FX (`gate_1410`, `mbeq_1197`, `sc4_1882`).
* `gtk3` + `libayatana-appindicator` — the tray icon (GTK3 is linked, the
  appindicator library is `dlopen`ed).
* Optional: `gnome-shell-extension-appindicator` (GNOME panel host for the tray
  icon), `mesa` (GL driver), `xdg-utils` (open links from the context menus).

## One-file vs Onedir Builds

`packaging/holaf-mix.spec` produces a **onefile** build by default:
- Single `dist/holaf-mix` binary (~80-120 MB)
- Slow first launch (~1-2s extraction to `/tmp`)
- Easy to distribute (one file)

For a faster-launching **onedir** build, uncomment the `COLLECT(...)` block at
the bottom of the spec file and remove the EXE's bundled binaries/datas:

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

## Configuration File Location

`src/config/settings.py` automatically detects whether the app is running from
source or from a PyInstaller bundle:

- **From source**: `<repo>/config.json`
- **From bundle**: `<dir-of-binary>/config.json`

This means:
- Running `./dist/holaf-mix` → config at `./dist/config.json`
- Installed via `pacman` → config at `/usr/lib/holaf-mix/config.json` (writable
  because `/usr/lib/holaf-mix/` is owned by the package and not stripped of
  write perms in our PKGBUILD)

If you prefer the XDG standard (`~/.config/holaf-mix/config.json`), patch
`src/config/settings.py` to use:

```python
CONFIG_FILE = os.path.join(
    os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")),
    "holaf-mix", "config.json"
)
```

---

## Troubleshooting

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

### Tray icon missing (Wayland / GNOME)

The tray icon (built in by default) needs an **appindicator host** to be drawn:
GNOME needs `gnome-shell-extension-appindicator`, KDE/Plasma ships its own,
sway needs `swaybar`'s tray.  On GNOME/Wayland the icon's **clicks are not
delivered** by `tray-icon` (X11 only) — use the menu instead: a left click opens
it (`with_menu_on_left_click`), with `Show/Hide`, `Restart audio engine`, `Quit`.
With no host — or no display at all — the log says
`tray: unavailable (<reason>) - closing the window will quit, not hide` and the
window's close button quits (so the app can never be hidden unreachably).

### "libgtk-3.so.0: cannot open shared object file" at runtime

`holaf-ui` needs the **system** GTK3: it is deliberately *not* embedded in the
PyInstaller bundle (a frozen GTK3 would shadow the user's and mismatch the
appindicator host / theme).  Install it: Arch `sudo pacman -S gtk3
libayatana-appindicator`, Debian `sudo apt install libgtk-3-0
libayatana-appindicator3-1`.  Alternatively rebuild without the icon:
`HOLAF_NO_TRAY=1 ./build.sh`.

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

## What the build script does (in order)

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
