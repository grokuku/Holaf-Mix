# Holaf-Mix — Installing and Building

Holaf-Mix is a **100 % Rust** application: `holaf-ui` (the app), `holafd` (the
audio daemon, an alternative/automation backend) and `holafctl` (its CLI).
There is no Python, no PyInstaller bundle and no venv anywhere — see
[`docs/python-removal.md`](docs/python-removal.md).

Two distinct paths. Do not mix them:

| I want to… | Section |
|---|---|
| **use Holaf-Mix** | **§1 — install from a CI build** (download the GitHub Actions artifact, install the runtime dependencies, run `./holaf-ui`) |
| check a runtime problem | §3 — troubleshooting |
| ship an Arch package | §4 — `packaging/PKGBUILD` (maintained option) |
| compile it myself | §5 — local build (developers / packagers) |

**The normal path is §1, and it requires no compilation at all**: the three
binaries are built by `.github/workflows/build.yml` on Arch Linux and published
as a downloadable artifact.  §5 (local build) is kept for developers and
packagers; `packaging/PKGBUILD` (§4) stays maintained for whoever wants to
distribute Holaf-Mix as an Arch package, but the current delivery workflow does
not use it.

---

## 1. Install from a CI build (normal path)

### 1.1 Download the artifact

GitHub → **Actions** → workflow **Build Holaf-Mix** → the run for the commit or
tag you want → **Artifacts** (bottom of the run page).

Each run publishes **one** artifact:

| Artifact | Content |
|---|---|
| `holaf-mix-<sha>-<arch>` | `holaf-mix-<version>-<arch>.tar.gz` (the distributable bundle) + `SHA256SUMS` |

### 1.2 What the tarball contains

The tarball extracts to a single `holaf-mix-<version>/` directory:

| File | Role |
|---|---|
| `holaf-ui` | **the application** — the entry point (Rust/egui); drives PipeWire **in its own process** |
| `holafd` | Rust audio daemon — alternative/automation backend (not needed for the window) |
| `holafctl` | CLI that drives the daemon from a shell (`holafctl status`, …) |
| `holaf-mix` | a 20-line **shell** script that only does `exec ./holaf-ui "$@"` (keeps the familiar command) |
| `USAGE.txt` | the 10 lines above, in the tarball itself |
| `icons/` | the icon set (`holaf-mix-{16..512}.png`, `.svg`, `.ico`) |
| `holaf-mix.desktop` | the desktop entry, with `__EXEC_PATH__` / `__ICON_PATH__` placeholders |
| `BUILD.md`, `README.md`, `ROADMAP.md`, `docs/` | the docs of the released revision |

No Rust toolchain, no compiler, no Python and no Python packages are needed on
the target machine — only the runtime dependencies below.

### 1.3 Runtime dependencies (install these — this is all you need)

The binaries from CI link against and `exec` the files below.  It is the same
set `packaging/PKGBUILD` declares as `depends`: installing it is all the
downloaded tarball needs.  None of the §5 build dependencies (compiler,
headers, Rust) is required.

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
  icon), `mesa` (GL driver), `xdg-utils` (open links from the context menus).

Missing helpers degrade gracefully (`pactl` errors logged once, meter at 0,
empty app list) but the two hard links (`libpipewire-0.3.so.0`, `libasound.so.2`)
are not optional.

### 1.4 Unpack and run

```bash
# optional but cheap: the artifact ships SHA256SUMS next to the tarball
sha256sum -c SHA256SUMS

tar xzf holaf-mix-<version>-<arch>.tar.gz -C ~/apps/
cd ~/apps/holaf-mix-<version>
./holaf-ui                     # the app
# or the familiar command (same binary):
./holaf-mix
```

The first launch creates `config.json` (§2).  `./holaf-ui --help` lists the
options: `--config FILE`, `--midi PORT`, `--midi-list`,
`--always-on-top` / `--no-always-on-top`.

`holafd` / `holafctl` are there for automation and diagnostics:
`./holafctl --help`, `HOLAF_SOCKET` must match (default
`/tmp/holaf-$USER.sock`).  The window never needs them: `holaf-ui` drives
PipeWire in-process.

### 1.5 Optional: system-wide install without a package

No package needed: copy the files where you want them.  Run these from the
unpacked `holaf-mix-<version>/` directory:

```bash
sudo install -m755 holaf-ui holafd holafctl holaf-mix /usr/local/bin/
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

There is no Python resolver any more: the path is decided by
`holaf_core::config::default_config_path()` (`rust/crates/holaf-core/src/config.rs`),
in this order:

1.  **`--config FILE`** — highest priority, used as-is;
2.  **`$HOLAF_CONFIG`** — explicit override, used as-is (may not exist yet);
3.  **next to the executable** — `<dir of holaf-ui>/config.json`, when that file
    exists.  This is the tarball case (the binary sits in the unpacked folder)
    and an installed binary started from a `.desktop` file, whose working
    directory is `$HOME`;
4.  **`./config.json`** in the current working directory, when it exists;
5.  **`$XDG_CONFIG_HOME/holaf-mix/config.json`** — default
    `~/.config/holaf-mix/config.json`, the location a fresh system install
    creates on first run.

(The source tree is not special-cased: `cargo run`/`cargo test` run from inside
the repo, whose root holds `.git/`, so rule 2 of the resolver resolves to
`<repo>/config.json` — same file as before.)

Consequences worth knowing:

* Running `./holaf-ui` from the unpacked tarball → `./config.json` of that
  folder (created on first save).  Moving the folder moves the config with it.
* A system install (`/usr/local/bin`, Arch package) → `~/.config/holaf-mix/`
  (rules 3/4 find nothing writable in `/usr/bin`), so the same user keeps the
  same mixer across upgrades.
* To pin a file explicitly: `HOLAF_CONFIG=/path/to/config.json ./holaf-ui`.

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
| `pw-cli`, `pw-dump`, `pw-link`, `pw-metadata`, `pw-top` | `pipewire` | graph discovery, linking, quantum diagnostics |

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

An **X11-only** session still works, but only from a build that opts in:
`cargo build -p holaf-ui --features x11` (that additionally needs
`libxkbcommon-x11` / `libxkbcommon-x11-dev`).  The shipped package and the CI
artifact are Wayland-only, so Wayland support cannot regress unnoticed.

No display at all (a container, an SSH session): `holaf-ui` says so on stderr
and exits 1 instead of panicking; the offscreen renderer is
`cargo run -p holaf-ui --bin holaf-ui-snapshot -- --out /tmp/holaf-ui.png`.

### VU meters stay dark

The meters record each strip's source with `pw-cat`, so three things must hold:

```bash
command -v pw-cat            # pipewire-audio (Debian: pipewire-bin)
holafctl meter-sources       # one source per strip, from the engine
journalctl --user -b | grep metering   # what the UI actually recorded
```

`pw-cat --target` matches a **node name**; the engine reports PulseAudio source
names (`<node>.monitor`).  The UI strips the `.monitor` suffix and adds
`stream.capture.sink=true`, which is what makes a capture stream link to that
sink's `monitor_FL/FR`.  Without either, `pw-cat` silently records the
**default source** and every bar stays at zero — the log line
`metering: recording <source> for <uid>` shows the resolved target, and any
`pw-cat` error is echoed as `metering: <uid>: …`.

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

`./holaf-ui --midi-list` prints the exact port names `--midi` accepts.

### "Permission denied" when saving config

The config is written atomically next to where it was resolved (§2).  If you
copied the binary alone into a read-only directory (e.g. `/usr/bin/`), the
resolver falls through to `~/.config/holaf-mix/config.json` — which is always
writable.  If you *pinned* a read-only path with `--config` or `$HOLAF_CONFIG`,
that is the error: choose a writable file.

---

## 4. Arch package: `packaging/PKGBUILD` (distribution option)

`packaging/PKGBUILD` is **maintained** and builds/installs Holaf-Mix as a real
Arch package: `/usr/bin/holaf-ui`, `holafd`, `holafctl`, the `holaf-mix` shell
launcher, the `.desktop` entry, the hicolor icons, `depends` resolution,
`post_install` icon cache refresh and a clean `pacman -Rns` uninstall.

It is **not** part of the project's delivery workflow: the shipped artifact comes
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

- `/usr/bin/holaf-ui` (the app — the entry point)
- `/usr/bin/holafd` + `/usr/bin/holafctl` (audio daemon + CLI, for automation)
- `/usr/bin/holaf-mix` (the shell launcher, same as the tarball's)
- `/usr/share/applications/holaf-mix.desktop` (KDE menu entry)
- `/usr/share/icons/hicolor/{16,...,512}x{16,...,512}/apps/holaf-mix.png`
- `/usr/share/icons/hicolor/scalable/apps/holaf-mix.svg`
- `/usr/share/holaf-mix/icons/` (full set)

The app shows up in **KDE Menu → Multimedia → Holaf-Mix**.  Its config lives in
`~/.config/holaf-mix/config.json` (§2).

To uninstall: `sudo pacman -Rns holaf-mix`.

Building through `makepkg` needs the build dependencies of §5.1 (it runs
`scripts/build_rust.sh` itself) — the `depends` array only covers what the
*installed* app needs at runtime.

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
    librsvg imagemagick \
    pipewire alsa-lib \
    wayland wayland-protocols libxkbcommon

# No tray package: the tray icon is a D-Bus StatusNotifierItem (ksni), so the
# Rust build needs neither gtk3 nor libayatana-appindicator.
```

Why each system package (verified against the binary, not guessed: `ldd
rust/target/release/holaf-ui` plus a grep for the external commands the code
spawns):

| Package | Needed for |
|---|---|
| `base-devel`, `rust`, `pkgconf`, `clang` | build: cargo/rustc, `.pc` probing by the `-sys` crates, `libclang.so` for bindgen (`libspa-sys`, `pipewire-sys`) |
| `librsvg`, `imagemagick` | `build.sh`: `rsvg-convert` renders the icon set, `magick` bundles the `.ico` |
| `alsa-lib` | **hard link** `libasound.so.2` — ALSA sequencer backend of `midir` (MIDI Mix) |
| `pipewire` | **hard link** `libpipewire-0.3.so.0` (`holaf-ui`, `holafd`) + `pw-cli`, `pw-dump`, `pw-link`, `pw-metadata`, `pw-top` |
| `pipewire-audio` *(runtime)* | `pw-cat` — one child process per VU-meter source (`rust/crates/holaf-ui/src/capture.rs`) |
| `pipewire-pulse` *(runtime)* | the PulseAudio-compatible **server** `pactl` talks to (replaces `pulseaudio`, which conflicts with it) |
| `libpulse` *(runtime)* | `/usr/bin/pactl` itself — app list / app move (`rust/crates/holaf-pw/src/sink_inputs.rs`) |
| `wireplumber` *(runtime)* | session manager: builds the nodes/monitors the mixer renames, links and meters |
| `swh-plugins` *(runtime)* | the LADSPA plugins the FX strips load: `gate_1410`, `mbeq_1197`, `sc4_1882` |
| `tap-plugins` *(runtime, AUR on Arch)* | `tap_tubewarmth` — the TUBE WARMTH strip (`tap-plugins` is a normal package on Debian/Fedora) |
| `wayland`, `wayland-protocols`, `libxkbcommon` | build: `wayland-scanner` + protocol XMLs (winit generates its bindings) and the keyboard protocol; the `.so` are `dlopen`ed at run time |

> `ladspa-plugins` does not exist on Arch (nor on Debian): the LADSPA plugins
> come from **`swh-plugins`**. `rnnoise` ships the denoiser *library* only — the
> `noise_cancel` strip needs `librnnoise_ladspa.so`, which is not packaged
> anywhere and must be built from
> [noise-suppression-for-voice](https://github.com/werman/noise-suppression-for-voice).

> **Why no GTK3?** The tray icon (cargo feature `tray`, **on by default** in
> `scripts/build_rust.sh`) used to be `libappindicator`, i.e. GTK3 linked in.
> Since the 2026-11-15 migration it is **`ksni`**: a pure D-Bus
> `org.kde.StatusNotifierItem` with no GTK, no GMainLoop and no appindicator
> library — `gtk 0.18`, a closed/unmaintained line, is out of the dependency
> graph entirely (`docs/dependency-audit.md`).  Nothing to install, and
> `ldd holaf-ui | grep gtk` is empty.  `HOLAF_NO_TRAY=1` still skips the icon
> deliberately; `HOLAF_TRAY_GTK=1` builds the old libappindicator backend
> (feature `tray-gtk`) if you ever need it — that one *does* need `gtk3` +
> `libayatana-appindicator`.

On Debian / Ubuntu:

```bash
sudo apt install \
    build-essential pkg-config clang \
    librsvg2-bin imagemagick \
    libpipewire-0.3-dev libspa-0.2-dev libpulse-dev \
    pipewire pipewire-bin pipewire-pulse wireplumber pulseaudio-utils \
    libasound2-dev swh-plugins tap-plugins \
    libwayland-dev wayland-protocols libwayland-bin libxkbcommon-dev
```

Debian 12 notes (checked with `apt-cache policy` / `dpkg -S`):

* `pactl` lives in **`pulseaudio-utils`** (not in `libpulse0`, unlike Arch where
  it is in `libpulse`).
* `pw-cat`, `pw-cli`, `pw-dump`, `pw-link`, `pw-metadata`, `pw-top` live in
  **`pipewire-bin`**.
* There is no `ladspa-plugins` and no `librnnoise-ladspa` in bookworm — install
  `swh-plugins` for gate/mbeq/sc4 and build the rnnoise LADSPA plugin yourself.
* The `rustc` in bookworm (1.63) is far below the workspace MSRV (1.98): use
  `rustup`.

### 5.2 Build the icons

```bash
./assets/icons/build_icons.sh
```

Output: `dist/icons/holaf-mix-{16,22,24,32,48,64,128,256,512}.png`,
`holaf-mix.ico`, `holaf-mix.svg`.  (`./build.sh` does this for you.)

### 5.3 Build the binaries

```bash
./build.sh
```

That's it.  This runs:
1. `assets/icons/build_icons.sh` → `dist/icons/`;
2. `scripts/build_rust.sh` → `dist/holaf-ui` (the app) + `dist/holafd` +
   `dist/holafctl` (one `cargo build --release`, tray feature on by default).
   A failure here is **fatal** — there is no Python fallback any more.

```bash
./build.sh --tarball        # ... and produce dist/holaf-mix-<version>-<arch>.tar.gz
./build.sh --run            # build, then launch dist/holaf-ui
./build.sh --check          # verify the build dependencies
./build.sh --clean          # remove build/ and dist/
```

To build only the Rust side (e.g. after a `git pull` in `rust/`):

```bash
./scripts/build_rust.sh     # cargo build --release -p holaf-ui -p holafd -p holafctl
```

### 5.4 Run it from `dist/`

```bash
./dist/holaf-ui
# or, without touching the build output:
ln -s "$PWD/dist/holaf-ui" ~/.local/bin/holaf-ui
```

Same runtime dependencies as §1.4/§1.3 and the same config resolution as §2
(from the repo root, the repo's own `config.json` is used).

---

## 6. What the build script does (in order)

`build.sh` is the only build entry point.  It no longer knows about
PyInstaller, `requirements.txt`, a venv or a Python launcher.

1. `check_deps()` — verifies `cargo`, `rsvg-convert`, `magick`; *warns* about
   missing runtime helpers (`pw-cat`, `pactl`) and optional LADSPA files
2. `build_icons()` — renders the SVG to 9 PNG sizes + ICO
3. `build_binaries()` — `scripts/build_rust.sh`: `holaf-ui` (**with the tray
   icon**, `HOLAF_NO_TRAY=1` to drop it) + `holafd` + `holafctl` → `dist/`
4. `make_tarball()` (with `--tarball`) — `dist/holaf-mix-<version>-<arch>.tar.gz`
   with the three binaries, the `holaf-mix` shell launcher, `icons/`, the
   `.desktop` entry, `USAGE.txt` and the docs

Run any step in isolation:

```bash
./build.sh --icons-only     # just rebuild icons
./build.sh --tarball        # build + package
./build.sh --clean          # nuke build/ and dist/
```

CI (`.github/workflows/build.yml`) runs `cargo test --workspace` as a gate, then
`./build.sh --tarball`, then unpacks the tarball and smoke-tests it: no `.py`
file inside, `holaf-ui --help` / `holaf-mix --help` run, `ldd` resolves, and a
headless launch falls back to `$XDG_CONFIG_HOME/holaf-mix/config.json`.
