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
    python-pip \
    librsvg \
    imagemagick

# pyinstaller is in AUR on Arch; installing via pip (the upstream-
# recommended method) is simpler and keeps versions reproducible.
pip install --user \
    PySide6 \
    sounddevice \
    numpy \
    mido \
    python-rtmidi \
    pyinstaller
```

> **Why no `python-pyinstaller`?** It's an AUR package, not in `[extra]`.
> Pip install is the upstream-recommended install method (it's a build
> tool, not a runtime dep, so system-package-staleness isn't an issue).
>
> **Why no `python-pyside6`?** Same — pip gives you the exact version
> pinned in `requirements.txt` rather than whatever CachyOS built.
> If you'd rather use the system package, drop the `PySide6` line above
> and `sudo pacman -S python-pyside6`.

On Debian / Ubuntu:

```bash
sudo apt install \
    python3-pip \
    librsvg2-bin \
    imagemagick \
    libpipewire-0.3-dev \
    libpulse-dev \
    ladspa-sdk \
    ladspa-plugins \
    librnnoise-ladspa

pip3 install --user --break-system-packages \
    PySide6 \
    sounddevice \
    numpy \
    mido \
    python-rtmidi
```

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
1. `pyinstaller packaging/holaf-mix.spec` → `dist/holaf-mix` (onefile binary)
2. Copies icons next to the binary

### 4. Run it

```bash
./dist/holaf-mix
```

The first launch creates `config.json` next to the binary.

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
- `/usr/bin/holaf-mix` (executable)
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
sudo pacman -S pipewire
```

### "No LADSPA plugins found"

```bash
sudo pacman -S ladspa-plugins librnnoise-ladspa
```

Verify:

```bash
ls /usr/lib/ladspa/
# Should contain: gate_1410.so  mbeq_1197.so  sc4_1882.so  ...
```

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
2. `build_icons()` — renders the SVG to 9 PNG sizes + ICO
3. `build_executable()` — runs PyInstaller on `holaf-mix.spec`
4. (optional) `make_tarball()` — bundles everything for distribution

Run any step in isolation:

```bash
./build.sh --icons-only     # just rebuild icons
./build.sh --executable     # skip icon check
./build.sh --clean          # nuke build/ and dist/
./build.sh --tarball        # also produce dist/holaf-mix-<ver>.tar.gz
```
