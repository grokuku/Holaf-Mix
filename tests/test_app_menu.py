"""Tests for the right-click application menu (replaces the old shortcut).

What is covered:

* the menu content — exactly ``Restart audio engine``, ``Debug window``,
  *separator*, ``Open project page``, in that order (the same menu everywhere);
* the right-click plumbing — a ``QContextMenuEvent`` sent to the window (the
  ``contextMenuEvent`` path) and to a child widget (the event-filter path) opens
  exactly one menu, at the click position;
* the actions themselves:
  - *Open project page* → ``QDesktopServices.openUrl`` with the project URL,
    including the „no browser" failure path;
  - *Restart audio engine* → ``shutdown()`` then ``start_engine(strips)`` on a
    fake engine, **on a worker thread** (the UI thread returns immediately),
    status-bar feedback, failure reported without raising, non-reentrant, and
    the Rust-daemon variant (a daemon we own is stopped and respawned, one we do
    not own is left running);
* the old ``Ctrl+Shift+D`` shortcut is really gone (no ``QShortcut`` carries
  that sequence anymore, and pressing it opens nothing).

The real ``MainWindow`` is built here (offscreen) with ``settings`` and
``pipewire_utils`` patched: no config file is read or written, no PipeWire call
is made, and both timers are stopped in ``tearDown``.

Run with::

    python -m unittest discover -s tests -v
    python tests/test_app_menu.py            # direct run also works
"""

import os
import sys
import time
import unittest
from contextlib import contextmanager
from unittest import mock

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if REPO_ROOT not in sys.path:
    sys.path.insert(0, REPO_ROOT)

# Offscreen Qt: these tests must also pass in a container with no display.
os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

try:  # PySide6 is optional in this project (main.py imports Qt lazily)
    from PySide6.QtCore import QPoint
    from PySide6.QtGui import QContextMenuEvent, QKeySequence, QShortcut
    from PySide6.QtWidgets import QApplication, QLabel, QMenu, QWidget
    QT_AVAILABLE = True
except Exception:  # noqa: BLE001 - missing Qt (or a missing libEGL) is fine
    QT_AVAILABLE = False

EXPECTED_ENTRIES = ["Restart audio engine", "Debug window", "", "Open project page"]


# =========================================================================
# fakes
# =========================================================================
class FakeEngine:
    """Duck-typed audio engine: records the restart sequence.

    ``delay`` simulates the blocking PipeWire work (so "did this run on the UI
    thread?" becomes measurable) and ``fail_at`` makes one step raise, like a
    daemon that refuses to come back.
    """

    def __init__(self, delay=0.0, fail_at=None, rust=False, owned_daemon=True):
        self.calls = []
        self.delay = delay
        self.fail_at = fail_at
        if rust:
            # Presence of this attribute is what marks a Rust (IpcAudioEngine)
            # backend for MainWindow; its value is the daemon-ownership flag.
            self._rust_daemon_spawned_by_us = owned_daemon

    # -- engine surface used by the UI around a restart -------------------
    def get_meter_levels(self):
        return {}

    def shutdown(self):
        self.calls.append("shutdown")
        if self.delay:
            time.sleep(self.delay)
        if self.fail_at == "shutdown":
            raise RuntimeError("shutdown boom")

    def start_engine(self, strips):
        self.calls.append(("start_engine", list(strips)))
        if self.delay:
            time.sleep(self.delay)
        if self.fail_at == "start_engine":
            raise RuntimeError("start_engine boom")

    # -- helpers ---------------------------------------------------------
    def step_names(self):
        return [c if isinstance(c, str) else c[0] for c in self.calls]


class FakeSignal:
    """Minimal Qt-like signal: ``connect(slot)`` + ``emit()``."""

    def __init__(self):
        self._slots = []

    def connect(self, slot):
        self._slots.append(slot)

    def emit(self):
        for slot in list(self._slots):
            slot()


class FakeAction:
    """Stand-in for QAction: text/isSeparator/triggered/trigger."""

    def __init__(self, text="", separator=False):
        self._text = text
        self._separator = separator
        self.triggered = FakeSignal()

    def text(self):
        return self._text

    def isSeparator(self):
        return self._separator

    def trigger(self):
        self.triggered.emit()


class FakeMenu:
    """Stand-in for QMenu: no event loop, records where ``exec`` was called."""

    instances = []

    def __init__(self, parent=None):
        self.parent = parent
        self.entries = []
        self.exec_positions = []
        FakeMenu.instances.append(self)

    def addAction(self, action):
        """Accept ``addAction("text")`` (main window) *and* an existing action."""
        if isinstance(action, str):
            action = FakeAction(action)
        self.entries.append(action)
        return action

    def addSeparator(self):
        self.entries.append(FakeAction("", separator=True))

    def actions(self):
        return list(self.entries)

    def exec(self, pos=None):
        self.exec_positions.append(pos)

    def setObjectName(self, name):
        self.object_name = name

    def setContextMenu(self, _menu):  # used by the tray icon
        pass


def _strip_stub(uid="uid-in", label="Desk", kind=None):
    """A real ``Strip`` model object (no Qt), as ``refresh_ui`` expects."""
    from src.models.strip_model import Strip, StripType

    return Strip(uid=uid, label=label, kind=kind or StripType.INPUT)


# =========================================================================
# tests
# =========================================================================
@unittest.skipUnless(QT_AVAILABLE, "PySide6 not installed")
class AppMenuTestBase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = QApplication.instance() or QApplication([])

    def setUp(self):
        self.window = None
        self._menu_patch = None

    def tearDown(self):
        if self.window is not None:
            self.window.meter_timer.stop()
            self.window.enforce_timer.stop()
            self.window._restart_timer.stop()
            self.window.hide()
            self.window.deleteLater()
            QApplication.processEvents()
            self.window = None
        if self._menu_patch is not None:
            self._menu_patch.stop()
            self._menu_patch = None

    # -- helpers ---------------------------------------------------------
    def make_window(self, engine=None, strips=()):
        """Build a real MainWindow with every external side effect patched."""
        from src.ui import main_window as mw

        strips = list(strips)
        patchers = [
            mock.patch.object(mw.settings, "load_config", return_value=(strips, None)),
            # A test must never rewrite the user's config.json.
            mock.patch.object(mw.settings, "save_config"),
            mock.patch.object(mw.pipewire_utils, "get_audio_nodes", return_value=[]),
            mock.patch.object(mw.pipewire_utils, "get_sink_inputs", return_value=[]),
        ]
        for patcher in patchers:
            patcher.start()
            # Stop in reverse order, before tearDown deletes the window.
            self.addCleanup(patcher.stop)

        window = mw.MainWindow(audio_engine=engine, midi_engine=None)
        self.window = window
        return window

    def patch_menu(self):
        """Make every ``QMenu(...)`` of the main window a recordable FakeMenu."""
        from src.ui import main_window as mw

        FakeMenu.instances = []
        self._menu_patch = mock.patch.object(mw, "QMenu", FakeMenu)
        self._menu_patch.start()
        return FakeMenu

    @staticmethod
    @contextmanager
    def patch_message_box():
        """Replace the main window's ``QMessageBox`` (never show a modal box).

        Patching the whole module-level name (not the Shiboken
        ``QMessageBox.warning`` static method, which a patch cannot really
        replace) keeps the failure path observable.  Yields the recorded
        ``warning`` mock: ``call_args[0] == (parent, title, message)``.
        """
        with mock.patch("src.ui.main_window.QMessageBox") as box:
            yield box.warning

    def send_right_click(self, widget, pos=QPoint(5, 5), global_pos=QPoint(100, 120)):
        event = QContextMenuEvent(QContextMenuEvent.Mouse, pos, global_pos)
        QApplication.sendEvent(widget, event)
        return event

    @staticmethod
    def wait_until(predicate, timeout=3.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            QApplication.processEvents()
            if predicate():
                return True
            time.sleep(0.01)
        QApplication.processEvents()
        return bool(predicate())


class MenuContentTest(AppMenuTestBase):
    def test_menu_has_exactly_the_expected_entries_in_order(self):
        window = self.make_window()
        menu = window.build_app_context_menu()

        self.assertEqual([a.text() for a in menu.actions()], EXPECTED_ENTRIES)
        self.assertTrue(menu.actions()[2].isSeparator(), "third entry must be the separator")
        # A separator is not an entry a user can trigger.
        self.assertEqual(len([a for a in menu.actions() if not a.isSeparator()]), 3)

    def test_extending_a_local_menu_appends_after_its_own_entries(self):
        """The strip label keeps "Rename…" and gains the application entries."""
        window = self.make_window()
        menu = QMenu()
        menu.addAction("Rename...")
        window.build_app_context_menu(menu)

        self.assertEqual([a.text() for a in menu.actions()],
                         ["Rename...", "", *EXPECTED_ENTRIES])


class RightClickTest(AppMenuTestBase):
    def test_right_click_on_the_window_opens_the_menu(self):
        window = self.make_window()
        window.show()
        fake_menu = self.patch_menu()

        self.send_right_click(window, global_pos=QPoint(140, 220))

        self.assertEqual(len(fake_menu.instances), 1, "exactly one menu per click")
        self.assertEqual(fake_menu.instances[0].exec_positions, [QPoint(140, 220)])
        self.assertEqual([a.text() for a in fake_menu.instances[0].actions()],
                         EXPECTED_ENTRIES)

    def test_right_click_on_a_child_widget_opens_the_menu(self):
        """A strip/label/VU meter must open the menu too (event filter path)."""
        window = self.make_window(strips=[_strip_stub()])
        window.show()
        label = window.findChild(QLabel)
        self.assertIsNotNone(label)
        fake_menu = self.patch_menu()

        self.send_right_click(label, global_pos=QPoint(11, 22))

        self.assertEqual(len(fake_menu.instances), 1, "exactly one menu per click")
        self.assertEqual(fake_menu.instances[0].exec_positions, [QPoint(11, 22)])

    def test_two_right_clicks_open_two_menus_one_at_a_time(self):
        window = self.make_window()
        window.show()
        fake_menu = self.patch_menu()

        self.send_right_click(window)
        self.send_right_click(window)

        self.assertEqual(len(fake_menu.instances), 2)
        # The reentrance guard must not stay stuck after exec returned.
        self.assertFalse(getattr(window, "_context_menu_open", False))

    def test_right_click_on_a_strip_opens_the_menu(self):
        """Strip bodies, meters, sliders… are covered by the event filter."""
        window = self.make_window(strips=[_strip_stub()])
        window.show()
        strip_widget = next(iter(window.widgets.values()))
        fake_menu = self.patch_menu()

        self.send_right_click(strip_widget, global_pos=QPoint(7, 9))

        self.assertEqual(len(fake_menu.instances), 1, "exactly one menu per click")
        self.assertEqual(fake_menu.instances[0].exec_positions, [QPoint(7, 9)])

    def test_right_click_on_a_strip_name_keeps_rename_and_adds_app_entries(self):
        """The label owns its menu: it must gain the application entries too."""
        from src.ui.widgets import strip_widget as sw

        window = self.make_window(strips=[_strip_stub()])
        strip_widget = next(iter(window.widgets.values()))
        FakeMenu.instances = []

        with mock.patch.object(sw, "QMenu", FakeMenu):
            strip_widget._on_label_context_menu(QPoint(3, 4))

        self.assertEqual(len(FakeMenu.instances), 1)
        menu = FakeMenu.instances[0]
        self.assertEqual([a.text() for a in menu.actions()],
                         ["Rename...", "", *EXPECTED_ENTRIES])
        self.assertEqual(len(menu.exec_positions), 1, "the menu must be shown once")

    def test_menus_are_not_opened_in_qt_popups(self):
        """A popup is a separate window: keep it native (no app menu)."""
        from PySide6.QtCore import Qt

        window = self.make_window()
        window.show()
        fake_menu = self.patch_menu()
        popup = QWidget()
        popup.setWindowFlags(Qt.Popup)

        self.send_right_click(popup)

        self.assertEqual([m for m in fake_menu.instances if m.exec_positions], [])


class OpenProjectPageTest(AppMenuTestBase):
    def test_action_opens_the_project_page_in_the_browser(self):
        window = self.make_window()
        with mock.patch("src.ui.main_window.QDesktopServices") as desktop:
            desktop.openUrl.return_value = True
            window.open_project_page()

        desktop.openUrl.assert_called_once()
        url = desktop.openUrl.call_args[0][0]
        self.assertEqual(url.toString(), "https://github.com/grokuku/Holaf-Mix")

    def test_failure_to_open_the_browser_is_reported(self):
        window = self.make_window()
        with mock.patch("src.ui.main_window.QDesktopServices") as desktop:
            desktop.openUrl.return_value = False
            with self.patch_message_box() as warning:
                window.open_project_page()  # must not raise

        warning.assert_called_once()
        self.assertIn("project page", warning.call_args[0][1].lower())

    def test_openUrl_exception_is_reported(self):
        window = self.make_window()
        with mock.patch("src.ui.main_window.QDesktopServices") as desktop:
            desktop.openUrl.side_effect = RuntimeError("no xdg-open")
            with self.patch_message_box() as warning:
                window.open_project_page()

        warning.assert_called_once()


class RestartEngineTest(AppMenuTestBase):
    def _trigger_restart(self, window):
        """Trigger the menu entry (not the slot) — the wiring is part of the test."""
        menu = window.build_app_context_menu()
        action = next(a for a in menu.actions() if a.text() == "Restart audio engine")
        action.trigger()

    def test_restart_runs_shutdown_then_start_engine_off_the_ui_thread(self):
        engine = FakeEngine(delay=0.05)
        window = self.make_window(engine=engine, strips=[_strip_stub("a"), _strip_stub("b")])
        self.assertIsNotNone(window.statusBar())

        start = time.monotonic()
        self._trigger_restart(window)
        elapsed = time.monotonic() - start

        # A 0.1 s engine on a worker: the UI thread must not wait for it.
        self.assertLess(elapsed, 0.2, "restart_audio_engine blocked the UI thread")
        self.assertTrue(window._full_restart_running)
        self.assertEqual(window.statusBar().currentMessage(), "Audio engine restarting…")
        self.assertEqual(engine.calls, [], "the work must not start synchronously")

        self.assertTrue(self.wait_until(lambda: engine.step_names() == ["shutdown", "start_engine"]))
        self.assertTrue(self.wait_until(lambda: not window._full_restart_running))
        self.assertEqual(engine.calls[0], "shutdown")
        self.assertEqual([s.uid for s in engine.calls[1][1]], ["a", "b"])
        self.assertEqual(window.statusBar().currentMessage(), "Audio engine restarted")

    def test_second_click_while_restarting_is_ignored(self):
        engine = FakeEngine(delay=0.2)
        window = self.make_window(engine=engine, strips=[_strip_stub()])

        self._trigger_restart(window)
        self._trigger_restart(window)  # must not queue a second restart
        self.assertIn("already in progress", window.statusBar().currentMessage())

        self.assertTrue(self.wait_until(lambda: not window._full_restart_running))
        self.assertEqual(engine.step_names(), ["shutdown", "start_engine"])

    def test_failure_is_reported_and_the_app_keeps_running(self):
        engine = FakeEngine(fail_at="start_engine")
        window = self.make_window(engine=engine)
        with self.patch_message_box() as warning:
            self._trigger_restart(window)
            self.assertTrue(self.wait_until(lambda: not window._full_restart_running))
            self.assertTrue(self.wait_until(lambda: warning.called))

        self.assertIn("restart failed", warning.call_args[0][1].lower())
        self.assertIn("start_engine boom", window.statusBar().currentMessage())
        self.assertFalse(window._wait_cursor_shown, "the wait cursor must be restored")
        # The window is still usable: a second attempt is allowed.
        self.assertFalse(window._full_restart_running)

    def test_shutdown_failure_is_reported_too(self):
        engine = FakeEngine(fail_at="shutdown")
        window = self.make_window(engine=engine)
        with self.patch_message_box() as warning:
            self._trigger_restart(window)
            self.assertTrue(self.wait_until(lambda: warning.called))

        self.assertEqual(engine.step_names(), ["shutdown"])
        self.assertFalse(window._wait_cursor_shown)

    def test_no_engine_reports_instead_of_crashing(self):
        window = self.make_window(engine=None)
        with self.patch_message_box() as warning:
            self._trigger_restart(window)

        warning.assert_called_once()
        self.assertFalse(window._full_restart_running)


class RustDaemonRestartTest(AppMenuTestBase):
    def test_daemon_we_own_is_stopped_and_respawned(self):
        engine = FakeEngine(rust=True, owned_daemon=True)
        window = self.make_window(engine=engine, strips=[_strip_stub()])
        order = []
        engine.shutdown = lambda: order.append("shutdown")
        engine.start_engine = lambda strips: order.append("start_engine")

        handle = mock.Mock(ok=True, spawned_by_us=True, reason="")
        with mock.patch("src.backend.rust_launcher.stop_daemon",
                        side_effect=lambda: order.append("stop_daemon") or True):
            with mock.patch("src.backend.rust_launcher.ensure_daemon_running",
                            side_effect=lambda *a, **k: order.append("ensure") or handle):
                window.restart_audio_engine()
                self.assertTrue(self.wait_until(lambda: not window._full_restart_running))

        self.assertEqual(order, ["shutdown", "stop_daemon", "ensure", "start_engine"])
        self.assertTrue(window.audio_engine._rust_daemon_spawned_by_us)
        self.assertEqual(window.statusBar().currentMessage(), "Audio engine restarted")

    def test_daemon_we_do_not_own_is_left_running(self):
        engine = FakeEngine(rust=True, owned_daemon=False)
        window = self.make_window(engine=engine, strips=[_strip_stub()])
        order = []
        engine.shutdown = lambda: order.append("shutdown")
        engine.start_engine = lambda strips: order.append("start_engine")

        handle = mock.Mock(ok=True, spawned_by_us=False, reason="")
        with mock.patch("src.backend.rust_launcher.stop_daemon",
                        side_effect=lambda: order.append("stop_daemon") or True) as stop:
            with mock.patch("src.backend.rust_launcher.ensure_daemon_running",
                            side_effect=lambda *a, **k: order.append("ensure") or handle):
                window.restart_audio_engine()
                self.assertTrue(self.wait_until(lambda: not window._full_restart_running))

        stop.assert_not_called()
        self.assertEqual(order, ["shutdown", "ensure", "start_engine"])
        self.assertFalse(window.audio_engine._rust_daemon_spawned_by_us)

    def test_daemon_that_does_not_come_back_fails_cleanly(self):
        engine = FakeEngine(rust=True, owned_daemon=True)
        window = self.make_window(engine=engine, strips=[_strip_stub()])

        handle = mock.Mock(ok=False, spawned_by_us=False, reason="holafd died on startup")
        with mock.patch("src.backend.rust_launcher.stop_daemon", return_value=True):
            with mock.patch("src.backend.rust_launcher.ensure_daemon_running",
                            return_value=handle):
                with self.patch_message_box() as warning:
                    window.restart_audio_engine()
                    self.assertTrue(self.wait_until(lambda: warning.called))

        self.assertEqual(engine.calls, ["shutdown"])
        self.assertIn("holafd died on startup", warning.call_args[0][2])
        self.assertFalse(window._full_restart_running)


class DebugWindowMenuTest(AppMenuTestBase):
    def test_debug_window_entry_opens_the_diagnostics_dialog(self):
        """The real window path (lazy import + parent + reuse), one click."""
        from src.ui import diagnostics_dialog as dd

        window = self.make_window(strips=[_strip_stub()])
        window.show()
        menu = window.build_app_context_menu()
        action = next(a for a in menu.actions() if a.text() == "Debug window")

        # Never collect anything here: only the window plumbing is under test.
        with mock.patch.object(dd.DiagnosticsDialog, "refresh"):
            action.trigger()
            self.assertIsNotNone(window._diagnostics_dialog)
            self.assertTrue(window._diagnostics_dialog.isVisible())
            first = window._diagnostics_dialog
            action.trigger()
            self.assertIs(window._diagnostics_dialog, first, "reopen the same window")
        window._diagnostics_dialog.close()


class ShortcutRemovedTest(AppMenuTestBase):
    def test_no_ctrl_shift_d_shortcut_is_installed(self):
        window = self.make_window()
        target = QKeySequence("Ctrl+Shift+D")
        installed = [sc.key().toString() for sc in window.findChildren(QShortcut)]
        self.assertNotIn(target.toString(), installed)
        self.assertFalse(hasattr(sys.modules["src.ui.main_window"], "DIAGNOSTICS_SHORTCUT"))

    def test_pressing_ctrl_shift_d_opens_nothing(self):
        from PySide6.QtCore import Qt
        from PySide6.QtTest import QTest

        window = self.make_window()
        window.show()
        QTest.qWaitForWindowActive(window, 1000)
        window.activateWindow()

        QTest.keyClick(window, Qt.Key_D, Qt.ControlModifier | Qt.ShiftModifier)
        QApplication.processEvents()

        self.assertIsNone(window._diagnostics_dialog)


if __name__ == "__main__":
    unittest.main(verbosity=2)
