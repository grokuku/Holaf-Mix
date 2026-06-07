#!/usr/bin/env python3
# ============================================================================
#  hook-email.py — PyInstaller runtime hook
# ============================================================================
#  Forces the `email` stdlib package to be importable in the bundled
#  executable on Python 3.12+, where it has been split into lazy-imported
#  subpackages that PyInstaller's static analysis + collect_submodules()
#  both miss.
#
#  This script runs BEFORE main.py. It pre-imports the email submodules
#  (which the bundle DOES include thanks to collect_submodules) and then
#  rebinds them under the `email` parent package in sys.modules.
#
#  Why this works:
#  PyInstaller's bundle contains the submodules (email.message,
#  email.parser, etc.) as standalone .pyc files, but it doesn't always
#  create the `email` parent __init__.py properly. When importlib.metadata
#  tries `import email`, Python can't find the parent, so it errors.
#  By forcing the parent into sys.modules ourselves, the submodules
#  become reachable via `email.message`, `email.parser`, etc.
#
#  Install:
#    Place this file anywhere and pass it to PyInstaller via the
#    `runtime_hooks=` argument in the .spec, or via --runtime-hook on
#    the CLI.
# ============================================================================

import sys


def _ensure_email_package():
    """If `import email` would fail, seed sys.modules with a fake parent
    package and rebind the bundled submodules under it."""
    try:
        import email  # noqa: F401
        return  # Already importable, nothing to do
    except ImportError:
        pass

    import importlib
    import types

    # Create a synthetic parent package for email
    email_pkg = types.ModuleType('email')
    email_pkg.__path__ = []  # Mark as a package, not a module
    sys.modules['email'] = email_pkg

    # Submodules that importlib.metadata needs to find. Importing them
    # populates sys.modules with the real modules; we just need the
    # parent package to exist as an anchor.
    submodules = (
        'email.message',
        'email.parser',
        'email.feedparser',
        'email._policybase',
        'email.contentmanager',
        'email.header',
        'email.charset',
        'email.encoders',
        'email.errors',
        'email.utils',
        'email._encoded_words',
        'email._header_value_parser',
        'email.iterators',
        'email.generator',
        'email.base64mime',
        'email.quoprimime',
    )
    for name in submodules:
        try:
            importlib.import_module(name)
        except ImportError:
            # Submodule not bundled; that's OK, importlib.metadata may
            # not need this specific one.
            pass


# Run at import time (this is a runtime hook, it executes as soon as
# PyInstaller bootstraps the interpreter, BEFORE main.py).
_ensure_email_package()
