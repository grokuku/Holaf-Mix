# -*- mode: python ; coding: utf-8 -*-
# ============================================================================
#  holaf-mix.spec — PyInstaller spec for Holaf-Mix
# ============================================================================
#  Build with:
#      pyinstaller holaf-mix.spec                       # onefile (default)
#      HOLAF_BUILD_MODE=onedir pyinstaller holaf-mix.spec
#  (`build.sh` sets HOLAF_BUILD_MODE itself; both modes are handled below.)
#
#  Produces:
#      onefile : dist/holaf-mix                (single executable; extracted to
#                                               sys._MEIPASS at launch)
#      onedir  : dist/holaf-mix/holaf-mix      (executable)
#                dist/holaf-mix/_internal/     (Python + Qt + bundled libs
#                                               + the Rust UI and daemon)
#
#  The Rust UI (`holaf-ui`), the audio daemon (`holafd`) and the CLI
#  (`holafctl`) are embedded when they exist in dist/ or
#  rust/target/release/ — see the "Bundled Rust binaries" block.  Without
#  `holaf-ui` the bundle is Python+Qt only and the launcher says so at startup
#  (`main.py` then runs the Qt interface).
# ============================================================================

from pathlib import Path
import sys
import os
import email as _stdlib_email  # the actual stdlib package, for path resolution

from PyInstaller.utils.hooks import collect_submodules

block_cipher = None

# --- Paths -------------------------------------------------------------------
# PyInstaller executes this spec file in a namespace where __file__ is NOT
# defined (it gets stripped to avoid leaking build-host paths into the bundle).
# However, PyInstaller sets sys.argv[0] to the spec file's path, so we use
# that as a reliable anchor. Falls back to CWD if the spec is fed via stdin
# (rare; not a concern for our build.sh workflow).
_spec_argv0 = sys.argv[0] if sys.argv and sys.argv[0] else ''
if _spec_argv0 and Path(_spec_argv0).is_file():
    SPEC_DIR = Path(_spec_argv0).resolve().parent
else:
    # Fallback: assume the spec is run from its own directory (e.g. when
    # `pyinstaller --clean` is invoked from packaging/).
    SPEC_DIR = Path.cwd()
ROOT = SPEC_DIR.parent
MAIN_PY = ROOT / 'main.py'
ICON_PNG = ROOT / 'dist' / 'icons' / 'holaf-mix-256.png'

# --- Hidden imports ---------------------------------------------------------
# mido/setuptools use importlib.metadata which dynamically imports the
# `email` stdlib package to parse PKG-INFO files. On Python 3.12+, this
# import is dynamic and PyInstaller's static analysis misses it. Worse,
# the setuptools vendored copy (setuptools._vendor.importlib_metadata)
# ALSO needs its own resolvable submodules, so we recursively include
# every submodule via collect_submodules.
# See: https://github.com/pyinstaller/pyinstaller/issues/7713
_hiddenimports = [
    # Audio / MIDI deps
    'sounddevice',
    '_sounddevice',
    'sounddevice._portaudio',
    'mido',
    'mido.backends.rtmidi',
    'rtmidi',
    # '_rtmidi' is NOT a top-level module: the extension lives inside the
    # package (rtmidi/_rtmidi*.so, re-exported as sys.modules['_rtmidi']).
    # PyInstaller collects it with 'rtmidi' above and warns "hidden import not
    # found" for this entry; kept as documentation only.
    '_rtmidi',
    # Stdlib `email` and all its submodules (Python 3.12+ workaround)
    'email',
    *collect_submodules('email'),
    # setuptools vendored importlib_metadata used by mido for version
    *collect_submodules('setuptools._vendor.importlib_metadata'),
]

# --- Bundled stdlib data files ----------------------------------------------
# On Python 3.12+, the `email` package has lazy-imported submodules that
# neither PyInstaller's static analysis, collect_submodules(), nor our
# runtime hook can fully wire up. As a last resort, we copy the entire
# `email` stdlib package directory into the bundle as data files. The
# runtime hook (hook-email.py) is still useful as a first attempt, but
# the data files guarantee `import email.message` will succeed.
_EMAIL_PKG_PATH = os.path.dirname(_stdlib_email.__file__)

# --- Bundled Rust binaries (holaf-ui + holafd + holafctl) --------------------
# `holaf-ui` is the app's WINDOW now (main.py launches it), and `holafd` is the
# audio backend the Qt fallback (`HOLAF_UI=python`) uses, so the packaged app
# must ship both.  Candidates, first hit wins:
#   dist/                      <- scripts/build_rust.sh / build.sh publish here
#   rust/target/release/       <- plain `cargo build --release` (dev)
# When neither exists (no cargo on the build host) the bundle is Python+Qt
# only: the launcher logs that `holaf-ui` is missing and starts the Qt UI.
#
# Declared as `datas` and **not** `binaries`: the binaries must be shipped
# as-is (no per-build rewriting).  PyInstaller still looks into them (an ELF in
# `datas` is moved to `binaries` and its `ldd` dependencies are collected), which
# is why the PipeWire/Pulse client libraries are removed again below — a frozen
# client library older than the user's daemon breaks the audio graph.  They land
# in sys._MEIPASS (onefile extraction dir, or onedir's _internal/) and
# src/launcher.py / src/backend/rust_launcher.py restore the executable bit on
# first use.
_RUST_BIN_SOURCES = [ROOT / 'dist', ROOT / 'rust' / 'target' / 'release']
# (name, role) — none of them may make the build fail: without `holaf-ui` the
# bundle still runs (the launcher starts the Qt UI), and a build host without
# cargo must keep producing a bundle.
_RUST_BINS = (('holaf-ui', 'UI'), ('holafd', 'daemon'), ('holafctl', 'CLI'))
_rust_data = []
_missing = []
for _name, _role in _RUST_BINS:
    for _dir in _RUST_BIN_SOURCES:
        _candidate = _dir / _name
        if _candidate.is_file():
            _rust_data.append((str(_candidate), '.'))
            break
    else:
        _missing.append(_name)
        print(f'[holaf-mix.spec] WARNING: {_name} ({_role}) not found in '
              + ', '.join(str(d) for d in _RUST_BIN_SOURCES))
if 'holaf-ui' in _missing:
    print('[holaf-mix.spec] WARNING: no holaf-ui -> this bundle has NO Rust UI; '
          'main.py will fall back to the Qt interface')
if _rust_data:
    print('[holaf-mix.spec] bundling Rust binaries: '
          + ', '.join(Path(src).name for src, _ in _rust_data))

# --- Analysis ----------------------------------------------------------------
a = Analysis(
    [str(MAIN_PY)],
    pathex=[str(ROOT)],
    binaries=[],
    # We don't bundle config.json into the binary. The app creates a
    # default config on first run and writes it next to the executable
    # (see _resolve_config_path() in src/config/settings.py). Bundling
    # the source config would only matter for "fresh-install" scenarios
    # where the user wants pre-existing settings, which isn't our use case.
    #
    # We DO bundle the `email` stdlib package as data files. This is the
    # nuclear option for Python 3.12+ where PyInstaller's static analysis
    # + collect_submodules() + runtime hooks all fail to make `import
    # email.message` work for importlib.metadata. The runtime hook tries
    # first (smaller bundle), but the data files guarantee resolution.
    #
    # `_rust_data` carries the Rust UI (holaf-ui) plus the daemon/CLI (holafd,
    # holafctl) — see the comment block above; empty on a machine without cargo.
    datas=[(_EMAIL_PKG_PATH, 'email'), *_rust_data],
    # sounddevice loads PortAudio via ctypes, which PyInstaller misses.
    # python-rtmidi has a dynamic backend. These hidden imports force
    # PyInstaller to include the .so modules explicitly.
    #
    # Note: entries MUST be valid Python import paths (e.g. "package.module"),
    # not filesystem paths. The actual list is computed above via
    # collect_submodules() to recursively include all submodules of `email`
    # and setuptools._vendor.importlib_metadata (needed for mido version
    # detection on Python 3.12+).
    hiddenimports=_hiddenimports,
    hookspath=[],
    hooksconfig={},
    # Runtime hooks run BEFORE main.py. The hook-email.py script
    # force-imports the `email` stdlib package and its submodules,
    # working around a PyInstaller bug on Python 3.12+ where the
    # `email` parent package isn't created even though its submodules
    # are bundled.
    runtime_hooks=[str(SPEC_DIR / 'hooks-runtime' / 'hook-email.py')],
    # Exclude unused stdlib to shrink the bundle.
    #
    # POLICY — an entry here MUST NOT be importable by main.py or src/**,
    # directly or transitively.  `excludes` wins over both the static import
    # analysis and `hiddenimports`: whatever the code says, the module is not
    # shipped, and the failure only shows up in the *packaged* binary
    # (ModuleNotFoundError at the call site, in production).  That is exactly
    # how `html` used to go missing — src/ui/diagnostics_dialog.py did
    # `import html` for html.escape() while this list dropped the package, so
    # "Debug window" died on the frozen build only.  The code now escapes
    # locally (escape_html()) and `html` is NO LONGER excluded, so anything
    # that imports it later is collected by the static analysis instead of
    # silently vanishing.  tests/test_imports_complete.py guards this list:
    # it fails if an excluded name is reachable from main.py / src/**.
    #
    # `email` is the documented exception: it stays excluded (its submodules
    # are pulled in dynamically by importlib.metadata and would drag in
    # unrelated code) and is shipped as plain data files instead — see
    # `datas=[(_EMAIL_PKG_PATH, 'email'), ...]` above plus hook-email.py.
    #
    # `xml` is NOT excluded for the same reason `html` is not: it is reachable
    # without our code asking for it.  mido needs setuptools' vendored
    # importlib_metadata (see hiddenimports), which makes PyInstaller collect
    # `pkg_resources` and install its `pyi_rth_pkgres` runtime hook; that hook
    # imports pkg_resources → plistlib → `xml.parsers.expat`, which runs
    # BEFORE main.py.  With 'xml' excluded the frozen app died at startup
    # (ModuleNotFoundError: No module named 'xml', in pyi_rth_pkgres) instead
    # of opening at all.  Verified by building this spec and launching the
    # binary.
    excludes=[
        'tkinter',
        'unittest',
        'pydoc',
        'doctest',
        'lib2to3',
        'test',
        'email',
        'http',
        'pydoc_data',
    ],
    win_no_prefer_redirects=False,
    win_private_assemblies=False,
    cipher=block_cipher,
    noarchive=False,
)

pyz = PYZ(a.pure, a.zipped_data, cipher=block_cipher)

# --- Keep the SYSTEM PipeWire/Pulse client libraries -------------------------
# PyInstaller cannot be told "do not analyse this data file": an ELF listed in
# `datas` is moved to `binaries` and its `ldd` dependencies are collected.  That
# is how `libpipewire-0.3.so.0` (and `libspa-0.2.so.0` when linked) ends up in
# the bundle even though nothing asked for it — and why the comment block above
# promises something the tool does not do.
#
# Why it must not stay: the bootloader prepends `sys._MEIPASS` to
# `LD_LIBRARY_PATH`, so a frozen copy would win over the user's library.  A
# PipeWire *client* older than the running PipeWire *daemon* can fail to
# negotiate its nodes — a silent "no audio" that looks like a mixer bug.  The
# Rust binaries are therefore compiled against the build host's headers but
# **link the system library at run time** (that is what PKGBUILD's
# `depends=('pipewire')` is for, and the same on any distro).
_PIPEWIRE_CLIENT_LIBS = frozenset(
    {
        'libpipewire-0.3.so.0',
        'libspa-0.2.so.0',
        'libpulse.so.0',
        'libpulse-simple.so.0',
    }
)
_bundled_pipewire = [
    entry[0] for entry in a.binaries if Path(entry[0]).name in _PIPEWIRE_CLIENT_LIBS
]
if _bundled_pipewire:
    a.binaries = [
        entry for entry in a.binaries if Path(entry[0]).name not in _PIPEWIRE_CLIENT_LIBS
    ]
    print(
        '[holaf-mix.spec] keeping the system PipeWire/Pulse libraries '
        '(dropped from the bundle: ' + ', '.join(sorted(_bundled_pipewire)) + ')'
    )

# --- Keep the SYSTEM GTK3 stack (tray icon + Qt's GTK platform theme) -------
# Same rule as the PipeWire block below, and the same *reason*: the bootloader
# prepends `sys._MEIPASS` to `LD_LIBRARY_PATH`, so anything frozen wins over the
# user's copy.  For a toolkit that is much worse than for a client library.
#
# **Measured, not assumed** (onedir bundle, holaf-ui built with `--features tray`,
# Debian 12): PyInstaller put the whole GTK3 stack in `_internal/` —
# libgtk-3.so.0, libgdk-3.so.0, libgdk_pixbuf-2.0.so.0, libpango*, libcairo*,
# libatk*, libatspi, libepoxy, libharfbuzz, plus GLib.  Two entry points pull it:
#   1. `holaf-ui` with the `tray` feature NEEDs libgtk-3.so.0 (tray-icon/gtk);
#   2. PySide6 ships `plugins/platformthemes/libqgtk3.so`, which NEEDs it too
#      (so the stack was already frozen *before* the tray — pre-existing bug for
#      the Qt fallback, now also fatal for holaf-ui).
#
# Why a frozen GTK3 breaks the app:
#   * a GTK3 older than the user's GLib/GDK cannot load: its dlopened theme
#     engine (`libadwaita`, `libpxbuf`…), input methods and GSettings schemas
#     are looked up from the *system* and version-checked against the *frozen*
#     library → "cannot open shared object", "undefined symbol", or a silent
#     theme fallback with mismatched CSS;
#   * an appindicator that talks to the host panel through a frozen GTK/GDK
#     cannot own a StatusNotifier item that the panel's libayatana accepts.
# GLib is included on purpose: a frozen GLib + a system GTK3 is exactly the
# mismatch above (GType system, GSettings, gio modules are shared state).  Qt6's
# Core/Gui also NEED libglib (QEventDispatcherGlib), so letting *both* use the
# system copy is the coherent choice — that is what `PKGBUILD depends=('glib2'
# via gtk3)` is for.
#
# Deliberately NOT dropped (needed by Qt itself, whose frozen copy is the
# pre-existing status quo and whose ABI is far narrower than GTK's):
# libX11, libxkbcommon, libdbus-1, libfontconfig, libfreetype, libpng16, libz.
#
# `libayatana-appindicator3.so.1` never appears here: `libappindicator-sys`
# dlopen()s it at run time, so it is a PKGBUILD runtime dep, not a bundle entry.
_GTK_SYSTEM_LIBS = frozenset(
    {
        # GTK toolkit + its rendering/text stack
        'libgtk-3.so.0',
        'libgdk-3.so.0',
        'libgdk_pixbuf-2.0.so.0',
        'libpango-1.0.so.0',
        'libpangocairo-1.0.so.0',
        'libpangoft2-1.0.so.0',
        'libcairo.so.2',
        'libcairo-gobject.so.2',
        'libatk-1.0.so.0',
        'libatk-bridge-2.0.so.0',
        'libatspi.so.0',
        'libepoxy.so.0',
        'libharfbuzz.so.0',
        # support libraries only those pull in (orphaned once GTK leaves)
        'libthai.so.0',
        'libdatrie.so.1',
        'libfribidi.so.0',
        'libgraphite2.so.3',
        'libpixman-1.so.0',
        'libjpeg.so.62',
        'libXcomposite.so.1',
        'libXcursor.so.1',
        'libXdamage.so.1',
        'libXext.so.6',
        'libXfixes.so.3',
        'libXi.so.6',
        'libXinerama.so.1',
        'libXrandr.so.2',
        'libXrender.so.1',
        # GLib family (+ its private deps): must match the system GTK3
        'libglib-2.0.so.0',
        'libgobject-2.0.so.0',
        'libgio-2.0.so.0',
        'libgmodule-2.0.so.0',
        'libgthread-2.0.so.0',
        'libpcre2-8.so.0',
        'libmount.so.1',
        'libblkid.so.1',
        'libselinux.so.1',
    }
)
_bundled_gtk = [
    entry[0] for entry in a.binaries if Path(entry[0]).name in _GTK_SYSTEM_LIBS
]
if _bundled_gtk:
    a.binaries = [
        entry for entry in a.binaries if Path(entry[0]).name not in _GTK_SYSTEM_LIBS
    ]
    print(
        '[holaf-mix.spec] keeping the system GTK3/GLib libraries '
        '(dropped from the bundle: ' + ', '.join(sorted(_bundled_gtk)) + ')'
    )

# --- Build mode --------------------------------------------------------------
# Onefile (default, single binary) or onedir (multi-file, faster launch).
# Selected by $HOLAF_BUILD_MODE, which build.sh sets from its --onedir flag.
# Done here rather than by sed-patching this file (the previous approach
# commented out individual EXE arguments and produced an invalid spec as soon
# as its line layout changed).
BUILD_MODE = os.environ.get('HOLAF_BUILD_MODE', 'onefile').strip().lower()
if BUILD_MODE not in ('onefile', 'onedir'):
    raise SystemExit(
        f"[holaf-mix.spec] invalid HOLAF_BUILD_MODE={BUILD_MODE!r} "
        "(expected 'onefile' or 'onedir')"
    )
print(f'[holaf-mix.spec] build mode: {BUILD_MODE}')

_EXE_COMMON = dict(
    name='holaf-mix',
    debug=False,
    bootloader_ignore_signals=False,
    upx=True,          # compress with UPX if available
    upx_exclude=[],
    runtime_tmpdir=None,
    console=False,     # GUI app, no terminal window
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
    icon=str(ICON_PNG) if ICON_PNG.exists() else None,
)

if BUILD_MODE == 'onefile':
    # Everything (Python, Qt, and the Rust daemon as data files) is packed into
    # the executable and extracted to sys._MEIPASS at launch.
    # NOTE: the result MUST be bound to a name — PyInstaller finds the targets
    # to build by scanning the spec namespace for EXE/COLLECT instances.
    exe_onefile = EXE(
        pyz,
        a.scripts,
        a.binaries,
        a.datas,
        [],
        strip=True,    # strip symbols from the bootloader
        **_EXE_COMMON,
    )
else:
    # onedir: the EXE carries no payload (exclude_binaries=True), COLLECT
    # writes the executable + _internal/ next to it.  holaf-ui/holafd/holafctl
    # land in _internal/ (sys._MEIPASS) and are found there by src/launcher.py
    # and src/backend/rust_launcher.py (which also restore their +x bit).
    exe_onedir = EXE(
        pyz,
        a.scripts,
        [],
        exclude_binaries=True,
        strip=False,
        **_EXE_COMMON,
    )
    collected = COLLECT(
        exe_onedir,
        a.binaries,
        a.zipfiles,
        a.datas,
        strip=False,
        upx=True,
        upx_exclude=[],
        name='holaf-mix',
    )
