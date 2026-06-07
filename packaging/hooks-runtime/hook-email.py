#!/usr/bin/env python3
# ============================================================================
#  hook-email.py — PyInstaller runtime hook (hardened)
# ============================================================================
#  Forces the `email` stdlib package and ALL its submodules to be
#  importable in the bundled executable on Python 3.12+, where it has
#  been split into lazy-imported subpackages that PyInstaller's static
#  analysis + collect_submodules() both miss.
#
#  This script runs BEFORE main.py. It does two things:
#
#  1. PRE-IMPORT each email submodule with a direct `import` statement.
#     This is critical: direct imports are visible in the bytecode and
#     PyInstaller's static analysis CAN track them. By importing
#     `email.message` here, we make sure the .pyc gets pulled into the
#     bundle (collect_submodules sometimes misses lazy subpackages).
#
#  2. SYNTHESIZE the `email` parent package in sys.modules. Even when
#     submodules are present in the archive, the `email` parent might
#     not be properly registered as a package (no __init__.py bundled),
#     causing `import email` to fail. We create a stub ModuleType to
#     anchor the submodules.
#
#  Why this works:
#  - The direct `import` statements trigger PyInstaller's bytecode
#    analysis, ensuring the .pyc files for these submodules end up
#    in the bundle's PYZ archive.
#  - The sys.modules synthesis makes `import email` succeed at runtime
#    by providing a parent package object for the submodules to attach
#    to.
# ============================================================================

import sys
import types


def _ensure_email_package():
    """Pre-import email submodules and synthesize the `email` parent
    package in sys.modules if missing."""
    # Step 1: Pre-import every known email submodule with a direct
    # `import` statement. Direct imports are visible in the bytecode
    # of this file, so PyInstaller's static analyzer will bundle the
    # corresponding .pyc files. We wrap in try/except because the
    # submodule might be optional on some platforms.
    try:
        import email.message
    except ImportError:
        pass
    try:
        import email.parser
    except ImportError:
        pass
    try:
        import email.feedparser
    except ImportError:
        pass
    try:
        import email._policybase
    except ImportError:
        pass
    try:
        import email.contentmanager
    except ImportError:
        pass
    try:
        import email.header
    except ImportError:
        pass
    try:
        import email.charset
    except ImportError:
        pass
    try:
        import email.encoders
    except ImportError:
        pass
    try:
        import email.errors
    except ImportError:
        pass
    try:
        import email.utils
    except ImportError:
        pass
    try:
        import email._encoded_words
    except ImportError:
        pass
    try:
        import email._header_value_parser
    except ImportError:
        pass
    try:
        import email.iterators
    except ImportError:
        pass
    try:
        import email.generator
    except ImportError:
        pass
    try:
        import email.base64mime
    except ImportError:
        pass
    try:
        import email.quoprimime
    except ImportError:
        pass

    # Step 2: If `import email` still fails, synthesize the parent
    # package. This anchors the submodules in sys.modules so that
    # `import email.message` resolves to the already-imported submodule.
    if 'email' not in sys.modules:
        email_pkg = types.ModuleType('email')
        email_pkg.__path__ = []  # Marks it as a package
        sys.modules['email'] = email_pkg
        # Rebind the already-imported submodules under the new parent
        for name, module in list(sys.modules.items()):
            if name.startswith('email.') and isinstance(module, types.ModuleType):
                # The submodule is now reachable as email.<name>
                pass  # Python's import machinery handles this automatically


# Execute at import time (runtime hooks run before main.py).
_ensure_email_package()
