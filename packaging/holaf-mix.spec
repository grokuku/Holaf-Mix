# -*- mode: python ; coding: utf-8 -*-
# ============================================================================
#  holaf-mix.spec — PyInstaller spec for Holaf-Mix
# ============================================================================
#  Build with:
#      pyinstaller holaf-mix.spec
#
#  Produces:
#      dist/holaf-mix/             (when --onedir, the default below)
#      dist/holaf-mix/holaf-mix    (executable)
#      dist/holaf-mix/_internal/   (Python + Qt + bundled libs)
#
#  To produce a single-file binary instead, change EXE to onefile:
#      EXE(pyz, a.scripts, a.binaries, a.datas, [], name='holaf-mix', ...)
#  and remove COLLECT. Note: onefile is slower to start (~1-2s extraction).
# ============================================================================

from pathlib import Path
import sys

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
    datas=[],
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

# --- Onefile build -----------------------------------------------------------
# Faster startup, no COLLECT needed. Good for a single-user install.
exe_onefile = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.datas,
    [],
    name='holaf-mix',
    debug=False,
    bootloader_ignore_signals=False,
    strip=True,        # strip symbols from the bootloader
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

# --- Uncomment for an onedir build (faster launch, multi-file dist) ---------
# COLLECT(
#     exe_onefile,
#     a.binaries,
#     a.zipfiles,
#     a.datas,
#     strip=False,
#     upx=True,
#     upx_exclude=[],
#     name='holaf-mix',
# )
