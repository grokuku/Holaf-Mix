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

# --- Analysis ----------------------------------------------------------------
a = Analysis(
    [str(MAIN_PY)],
    pathex=[str(ROOT)],
    binaries=[],
    # Bundle the default config so a fresh install has something to load.
    # The user's config.json still lives next to the executable (writable).
    datas=[
        ('config.json', '.'),
    ],
    # sounddevice loads PortAudio via ctypes, which PyInstaller misses.
    # python-rtmidi has a dynamic backend. These hidden imports force
    # PyInstaller to include the .so modules explicitly.
    hiddenimports=[
        'sounddevice',
        '_sounddevice',
        'sounddevice._portaudio',
        'mido',
        'mido.backends.rtmidi',
        'mido.backends.rtmidi/UNIX_JACK',
        'rtmidi',
        '_rtmidi',
    ],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
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
