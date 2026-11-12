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
#                                               + the Rust daemon)
#
#  The Rust audio daemon (holafd) and CLI (holafctl) are embedded when they
#  exist in dist/ or rust/target/release/ — see the "Bundled Rust binaries"
#  block.  Without them the bundle is Python-only and falls back to the
#  classic engine at startup.
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

# --- Bundled Rust binaries (holafd + holafctl) -------------------------------
# The Rust daemon is the DEFAULT audio backend (see main.create_audio_engine),
# so the packaged app must ship it.  Candidates, first hit wins:
#   dist/                      <- scripts/build_rust.sh / build.sh publish here
#   rust/target/release/       <- plain `cargo build --release` (dev)
# When neither exists (no cargo on the build host) the bundle is Python-only:
# the app still runs and falls back to the classic engine at startup.
#
# Declared as `datas` (not `binaries`) on purpose: PyInstaller dependency-
# analyses `binaries` entries and would then copy libpipewire/libspa/libpulse
# into the bundle, where LD_LIBRARY_PATH would make the app load them instead
# of the user's system libraries — a client library older than the running
# PipeWire daemon breaks the audio graph.  The daemon must link against the
# system PipeWire, so the ELFs travel as opaque payload.  They land in
# sys._MEIPASS (onefile extraction dir, or onedir's _internal/) and
# src/backend/rust_launcher.py restores the executable bit on first use.
_RUST_BIN_SOURCES = [ROOT / 'dist', ROOT / 'rust' / 'target' / 'release']
_rust_data = []
for _name in ('holafd', 'holafctl'):
    for _dir in _RUST_BIN_SOURCES:
        _candidate = _dir / _name
        if _candidate.is_file():
            _rust_data.append((str(_candidate), '.'))
            break
if not _rust_data:
    print('[holaf-mix.spec] WARNING: no Rust binaries found in ' \
          + ', '.join(str(d) for d in _RUST_BIN_SOURCES) \
          + ' -> Python-only bundle (the app will run the classic engine)')
else:
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
    # `_rust_data` carries the Rust daemon/CLI (holafd, holafctl) — see the
    # comment block above; empty on a machine without cargo.
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
    # Exclude unused stdlib to shrink the bundle
    excludes=[
        'tkinter',
        'unittest',
        'pydoc',
        'doctest',
        'lib2to3',
        'test',
        'email',
        'html',
        'http',
        'xml',
        'pydoc_data',
    ],
    win_no_prefer_redirects=False,
    win_private_assemblies=False,
    cipher=block_cipher,
    noarchive=False,
)

pyz = PYZ(a.pure, a.zipped_data, cipher=block_cipher)

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
    # writes the executable + _internal/ next to it.  holafd/holafctl land in
    # _internal/ (sys._MEIPASS) and are found there by rust_launcher.
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
