"""Frozen-bundle completeness: nothing the app imports may be missing.

Why this file exists
--------------------
The packaged binary failed in production with::

    Debug window unavailable
    The diagnostics window could not be opened:
    ModuleNotFoundError: No module named 'html'

while the source checkout worked perfectly.  Cause: ``packaging/holaf-mix.spec``
listed ``html`` in ``Analysis(excludes=[...])`` while
``src/ui/diagnostics_dialog.py`` did ``import html`` for ``html.escape()``.
``excludes`` beats both the static import analysis *and* ``hiddenimports``, so
the module was dropped from the bundle and the import only blew up at the call
site — in the frozen app, on the user's machine.

A unit test can never fully emulate PyInstaller, but the failure mode here is
purely a *policy* contradiction: "the code imports X" vs "the spec drops X".
That contradiction is what this module checks, at three levels:

1. **Static** — every top-level module imported by ``main.py`` / ``src/**`` is
   resolvable (:func:`importlib.util.find_spec`), the way PyInstaller's static
   analysis needs it to be.
2. **Spec** — no module reachable from our code appears in the spec's
   ``excludes`` list (the exact bug), with ``email`` as the single documented
   exception (shipped as data files instead).
3. **Runtime closure** — importing every module of the package must not pull in
   an excluded module *indirectly* either.  The closure deliberately includes
   what the *spec* forces into the bundle (``_hiddenimports``) plus
   ``pkg_resources``, which PyInstaller collects and imports from its
   ``pyi_rth_pkgres`` runtime hook *before* ``main.py``.  That hook is how the
   second bug of this family was found: ``plistlib`` (``pkg_resources`` →
   ``plistlib`` → ``xml.parsers.expat``) needs ``xml``, which the spec used to
   exclude, so the packaged app died during startup with
   ``ModuleNotFoundError: No module named 'xml'`` — before any window appeared.
4. **Hidden imports** — every module the spec promises to bundle must exist on
   the build host, otherwise the binary silently ships without that feature
   (``rtmidi``: no MIDI).

Plus a direct pin on the original bug: ``src.ui.diagnostics_dialog`` must import
and render its HTML summary **without** stdlib ``html`` being importable.

Run with: ``python3 -m unittest tests.test_imports_complete -v``
"""

import ast
import importlib
import json
import os
import pathlib
import subprocess
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
MAIN_PY = ROOT / "main.py"
SRC_DIR = ROOT / "src"
SPEC_FILE = ROOT / "packaging" / "holaf-mix.spec"

#: First-party packages: importing them needs no packaging decision.
FIRST_PARTY = {"src", "tools", "tests", "packaging"}

#: Modules dropped from the bundle on purpose *and* shipped another way.  Each
#: entry needs a justification here and a matching mechanism in the spec.
#:  - ``email`` : stdlib package whose submodules are pulled in dynamically by
#:    ``importlib.metadata`` (see mido); it stays out of the zip and travels as
#:    plain data files (``datas=[(_EMAIL_PKG_PATH, 'email'), ...]``) plus the
#:    ``hook-email.py`` runtime hook.
EXCLUDED_BUT_SHIPPED_AS_DATA = {"email"}

#: Marker proving the ``email`` exception is actually implemented in the spec.
_EMAIL_DATA_MARKER = "_EMAIL_PKG_PATH, 'email'"

#: Names that are real CPython packages but absent from ``sys.stdlib_module_names``.
_STDLIB_PACKAGES_NOT_LISTED = {"test"}


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------
def source_files():
    """``main.py`` + every module of the ``src`` package, deterministic order."""
    files = [MAIN_PY]
    files += [p for p in sorted(SRC_DIR.rglob("*.py")) if "__pycache__" not in p.parts]
    return files


def module_name(path):
    """``.../src/ui/main_window.py`` → ``src.ui.main_window`` (``__init__`` dropped)."""
    parts = list(path.relative_to(ROOT).with_suffix("").parts)
    if parts and parts[-1] == "__init__":
        parts.pop()
    return ".".join(parts)


def static_imports():
    """Top-level module name → list of files importing it (relative imports skipped)."""
    found = {}
    for path in source_files():
        try:
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        except SyntaxError as exc:  # pragma: no cover - would break the build too
            raise AssertionError(f"{path} does not parse: {exc}") from exc
        for node in ast.walk(tree):
            names = []
            if isinstance(node, ast.Import):
                names = [alias.name for alias in node.names]
            elif isinstance(node, ast.ImportFrom):
                # level > 0 is a relative import (inside `src`), always bundled.
                if node.level == 0 and node.module:
                    names = [node.module]
            for name in names:
                top = name.split(".")[0]
                if top and top not in FIRST_PARTY:
                    found.setdefault(top, []).append(str(path.relative_to(ROOT)))
    return found


def spec_source():
    return SPEC_FILE.read_text(encoding="utf-8")


def spec_excludes():
    """The literal ``excludes`` list passed to ``Analysis(...)`` in the spec."""
    tree = ast.parse(spec_source(), filename=str(SPEC_FILE))
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        func = node.func
        name = func.id if isinstance(func, ast.Name) else getattr(func, "attr", "")
        if name != "Analysis":
            continue
        for keyword in node.keywords:
            if keyword.arg == "excludes":
                try:
                    return list(ast.literal_eval(keyword.value))
                except ValueError as exc:  # pragma: no cover - spec is a plain list
                    raise AssertionError(
                        "Analysis(excludes=...) is no longer a literal list; "
                        "tests/test_imports_complete.py needs updating: %s" % exc
                    ) from exc
    raise AssertionError(  # pragma: no cover - spec always has one
        f"no Analysis(excludes=[...]) found in {SPEC_FILE}"
    )


def spec_hiddenimports():
    """Plain-string entries of the spec's ``_hiddenimports`` list.

    Starred entries (``*collect_submodules('email')``) need PyInstaller to be
    resolved, so they are skipped: their parent package is in the list anyway.
    """
    tree = ast.parse(spec_source(), filename=str(SPEC_FILE))
    for node in tree.body:
        if not isinstance(node, ast.Assign) or not node.targets:
            continue
        target = node.targets[0]
        if getattr(target, "id", None) != "_hiddenimports":
            continue
        if not isinstance(node.value, ast.List):
            break
        names = []
        for element in node.value.elts:
            if isinstance(element, ast.Constant) and isinstance(element.value, str):
                names.append(element.value)
        return names
    raise AssertionError(  # pragma: no cover - spec always has one
        f"no `_hiddenimports = [...]` list found in {SPEC_FILE}"
    )


#: Modules PyInstaller pulls into the graph on its own and imports from a
#: runtime hook *before* ``main.py`` — so a missing one breaks the app before any
#: window exists, with no traceback the user can act on.
#:  - ``pkg_resources``: bundled because the spec hidden-imports setuptools'
#:    vendored ``importlib_metadata``; its runtime hook (``pyi_rth_pkgres``)
#:    imports it, and ``pkg_resources`` → ``plistlib`` → ``xml.parsers.expat``.
BUNDLE_FORCED_MODULES = ("pkg_resources",)

#: ``_hiddenimports`` entries that are not real modules; PyInstaller logs
#: "Hidden import not found" for them and bundles the parent package instead.
#: Documented here (not silently dropped) so the day one of them becomes real,
#: the closure test starts covering it.
#:  - ``_rtmidi``: the C extension *inside* the ``rtmidi`` package
#:    (``rtmidi/_rtmidi*.so``, re-exported as ``sys.modules['_rtmidi']``);
#:  - ``sounddevice._portaudio``: does not exist — sounddevice loads PortAudio
#:    through ctypes, hence the ``_sounddevice`` / ``sounddevice`` entries.
LEGACY_HIDDENIMPORTS = {"_rtmidi", "sounddevice._portaudio"}


def bundle_module_names():
    """Modules the bundle loads on its own: spec hidden imports + hook-forced.

    Legacy entries (:data:`LEGACY_HIDDENIMPORTS`) are excluded — they cannot be
    imported by definition.
    """
    names = spec_hiddenimports() + list(BUNDLE_FORCED_MODULES)
    return [name for name in names if name not in LEGACY_HIDDENIMPORTS]


def is_importable(name):
    """True when ``import name`` could succeed here (no import side effects)."""
    try:
        return importlib.util.find_spec(name) is not None
    except (ImportError, AttributeError, ValueError):
        return False


def _run_probe(script, *args, timeout=180):
    """Run a probe script in a clean interpreter; returns (returncode, out, err)."""
    env = dict(os.environ)
    env.setdefault("QT_QPA_PLATFORM", "offscreen")
    env["PYTHONPATH"] = str(ROOT) + os.pathsep + env.get("PYTHONPATH", "")
    env.pop("PYTHONDONTWRITEBYTECODE", None)
    proc = subprocess.run(
        [sys.executable, "-c", script, *args],
        cwd=str(ROOT),
        env=env,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return proc.returncode, proc.stdout, proc.stderr


# ---------------------------------------------------------------------------
# 1. every import is resolvable (what PyInstaller's analysis needs)
# ---------------------------------------------------------------------------
class TestStaticImportsResolvable(unittest.TestCase):
    """Every module named by ``main.py`` / ``src/**`` must be importable."""

    def test_sources_are_discovered(self):
        names = [module_name(p) for p in source_files()]
        self.assertIn("main", names)
        self.assertIn("src.ui.diagnostics_dialog", names)
        self.assertGreater(len([n for n in names if n.startswith("src.")]), 15)

    def test_no_stdlib_module_missing(self):
        """A stdlib import that does not resolve is one PyInstaller cannot ship."""
        missing = {}
        for name in sorted(static_imports()):
            if name not in sys.stdlib_module_names:
                continue
            if not is_importable(name):
                missing[name] = static_imports()[name]
        self.assertEqual(missing, {}, f"stdlib modules not importable here: {missing}")

    def test_no_dependency_missing(self):
        """Third-party imports must be installed (see requirements.txt)."""
        missing = {}
        for name, files in sorted(static_imports().items()):
            if name in sys.stdlib_module_names:
                continue
            if not is_importable(name):
                missing[name] = sorted(set(files))
        self.assertEqual(
            missing,
            {},
            f"missing dependencies ({missing}) — run: pip install -r requirements.txt",
        )

    def test_every_package_module_imports(self):
        """Importing every module of ``src`` must not raise ModuleNotFoundError."""
        failures = {}
        for path in sorted(SRC_DIR.rglob("*.py")):
            if "__pycache__" in path.parts:
                continue
            name = module_name(path)
            try:
                importlib.import_module(name)
            except ModuleNotFoundError as exc:  # the production symptom
                failures[name] = f"ModuleNotFoundError: {exc}"
            except Exception as exc:  # noqa: BLE001 - reported, not swallowed
                failures[name] = f"{type(exc).__name__}: {exc}"
        self.assertEqual(failures, {}, f"modules that fail to import: {failures}")


# ---------------------------------------------------------------------------
# 2. the spec must not contradict the code  (the actual 'html' bug)
# ---------------------------------------------------------------------------
class TestSpecExcludesAreSafe(unittest.TestCase):
    """``Analysis(excludes=[...])`` is a hard 'do not ship', so it must be exact."""

    def test_spec_declares_excludes(self):
        excludes = spec_excludes()
        self.assertIsInstance(excludes, list)
        self.assertTrue(excludes)

    def test_excluded_names_are_real_packages(self):
        """A typo in ``excludes`` is silently ignored by PyInstaller — catch it here."""
        unknown = [
            name
            for name in spec_excludes()
            if name not in sys.stdlib_module_names
            and name not in _STDLIB_PACKAGES_NOT_LISTED
        ]
        self.assertEqual(unknown, [], f"excludes entries that are not stdlib modules: {unknown}")

    def test_no_imported_module_is_excluded(self):
        """REGRESSION GUARD: ``import html`` + ``excludes=['html']`` = broken binary."""
        excludes = set(spec_excludes())
        offenders = {
            name: sorted(set(files))
            for name, files in static_imports().items()
            if name in excludes and name not in EXCLUDED_BUT_SHIPPED_AS_DATA
        }
        self.assertEqual(
            offenders,
            {},
            "the spec excludes modules the app imports — the frozen binary will "
            f"raise ModuleNotFoundError at runtime: {offenders}",
        )

    def test_documented_exception_is_shipped_as_data(self):
        """The allowlist above is only valid while the spec implements it."""
        missing = [
            name
            for name in EXCLUDED_BUT_SHIPPED_AS_DATA
            if not self._shipped_as_data(name)
        ]
        self.assertEqual(missing, [], f"excluded modules shipped nowhere: {missing}")

    @staticmethod
    def _shipped_as_data(name):
        if name == "email":
            return _EMAIL_DATA_MARKER in " ".join(spec_source().split())
        return False


# ---------------------------------------------------------------------------
# 3. the runtime import closure must not touch an excluded module either
# ---------------------------------------------------------------------------
_CLOSURE_PROBE = r"""
import importlib, json, sys

names = json.loads(sys.argv[1])
before = set(sys.modules)
failed = {}
for name in names:
    try:
        importlib.import_module(name)
    except Exception as exc:  # noqa: BLE001 - reported to the parent test
        failed[name] = "%s: %s" % (type(exc).__name__, exc)
# Second pass: some entries only become importable once their parent has been
# imported (e.g. a C extension re-exported through its package __init__).
for name in list(failed):
    try:
        importlib.import_module(name)
        failed.pop(name)
    except Exception as exc:  # noqa: BLE001
        failed[name] = "%s: %s" % (type(exc).__name__, exc)
added = sorted({m.split(".")[0] for m in set(sys.modules) - before})
print(json.dumps({"added": added, "failed": failed}))
"""


class TestRuntimeImportClosure(unittest.TestCase):
    """What the app really loads at runtime must fit in the bundle."""

    @classmethod
    def setUpClass(cls):
        names = [module_name(p) for p in source_files()]
        # Plus everything the bundle loads on its own: the spec's hidden imports
        # and PyInstaller's pkg_resources runtime hook.  A missing one shows up
        # in `failed` below, i.e. as "this dependency is not installed" — the
        # binary would have shipped without that feature.
        names += bundle_module_names()
        code, out, err = _run_probe(_CLOSURE_PROBE, json.dumps(sorted(set(names))))
        cls.result = None
        if code != 0:  # pragma: no cover - probe infra failure
            raise AssertionError(f"import-closure probe failed ({code}):\n{err}")
        try:
            cls.result = json.loads(out.strip().splitlines()[-1])
        except (ValueError, IndexError):  # pragma: no cover - probe infra failure
            raise AssertionError(f"unparsable probe output: {out!r} / {err!r}")

    def test_all_modules_import_in_a_clean_interpreter(self):
        self.assertEqual(self.result["failed"], {})

    def test_closure_excludes_scope_is_a_subset_of_the_allowlist(self):
        excludes = set(spec_excludes())
        touched = sorted(excludes.intersection(self.result["added"]))
        unexpected = [name for name in touched if name not in EXCLUDED_BUT_SHIPPED_AS_DATA]
        self.assertEqual(
            unexpected,
            [],
            "importing the app pulls in modules the spec excludes "
            f"(they will be missing from the frozen bundle): {unexpected}",
        )


# ---------------------------------------------------------------------------
# 3b. what the spec promises to bundle must exist on the build host
# ---------------------------------------------------------------------------
class TestSpecHiddenImportsAvailable(unittest.TestCase):
    """A hidden import that does not resolve ships a binary missing that feature."""

    def test_spec_declares_hiddenimports(self):
        names = spec_hiddenimports()
        self.assertIn("sounddevice", names)
        self.assertIn("mido", names)

    def test_every_hiddenimport_is_installed(self):
        missing = [name for name in bundle_module_names() if not is_importable(name)]
        self.assertEqual(
            missing,
            [],
            "the spec hidden-imports these modules but the build host does not "
            "have them, so the binary would ship without that feature "
            f"(pip install -r requirements.txt): {missing}",
        )


# ---------------------------------------------------------------------------
# 4. the diagnostics window must keep working without stdlib `html`
# ---------------------------------------------------------------------------
_HTML_BLOCKER_PROBE = r"""
import sys

BLOCKED = ("html",)


class Blocker:
    # Make `import html` fail, mimicking a bundle where it was not collected.
    def find_spec(self, fullname, path=None, target=None):
        if fullname.split(".")[0] in BLOCKED:
            raise ModuleNotFoundError("No module named %r" % fullname, name=fullname)
        return None


sys.meta_path.insert(0, Blocker())

# The lazy import done by MainWindow.open_diagnostics(): what the frozen
# "Debug window" menu entry executes.
from src.ui.diagnostics_dialog import DiagnosticsDialog, escape_html

report = {
    "summary": {"error": 1, "warning": 0, "info": 0},
    "generated_at": "2024-01-01T00:00:00 <&>",
    "findings": [
        {
            "severity": "error",
            "id": "graph<link>",
            "message": "boom & <b>bang</b>",
            "hint": "do 'x' < y",
        }
    ],
}
html_doc = DiagnosticsDialog._render_summary_html(None, report)
assert "&lt;b&gt;bang&lt;/b&gt;" in html_doc, html_doc
assert "<b>bang</b>" not in html_doc, html_doc
assert escape_html("<&>") == "&lt;&amp;&gt;", escape_html("<&>")

leaked = sorted(m for m in sys.modules if m == "html" or m.startswith("html."))
if leaked:
    raise SystemExit("stdlib html was imported: %r" % leaked)
print("ok-without-html")
"""


class TestDiagnosticsNeedsNoStdlibHtml(unittest.TestCase):
    """Pin the original bug: the window must not need the excluded `html`."""

    def test_escape_html_matches_stdlib(self):
        from src.ui.diagnostics_dialog import escape_html

        stdlib_html = importlib.import_module("html")
        samples = [
            "",
            "plain",
            "a & b",
            "<script>alert('x')</script>",
            'quote " and \' apostrophe',
            "already &amp; escaped",
            "unicode — accents éàü ✓",
            42,
            None,
        ]
        for sample in samples:
            with self.subTest(sample=sample):
                self.assertEqual(
                    escape_html(sample), stdlib_html.escape(str(sample), quote=True)
                )

    def test_dialog_module_imports_and_renders_without_html(self):
        code, out, err = _run_probe(_HTML_BLOCKER_PROBE)
        self.assertEqual(
            code,
            0,
            "src.ui.diagnostics_dialog depends on stdlib html again — that module "
            f"is missing from the frozen bundle:\n{err}",
        )
        self.assertIn("ok-without-html", out)


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
