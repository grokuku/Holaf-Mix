from PySide6.QtWidgets import (QMainWindow, QWidget, QVBoxLayout, QHBoxLayout, 
                                QLabel, QScrollArea, QFrame, QPushButton, QInputDialog, QMessageBox,
                                QDialog, QCheckBox, QDialogButtonBox, QSizePolicy, QSystemTrayIcon, QMenu, QApplication, QStyle)
from PySide6.QtCore import (Qt, QThreadPool, QRunnable, Slot, QTimer, QByteArray, Signal,
                            QEvent, QUrl)
from PySide6.QtGui import QAction, QIcon, QDesktopServices, QCursor
from src.config import settings
from src.models.strip_model import Strip, StripType, StripMode
from src.ui.widgets.strip_widget import StripWidget

# Updated import: pipewire_utils is now located in src.backend
from src.backend import pipewire_utils
import logging

logger = logging.getLogger("MainWindow")

# --- UI timing constants ---
# Debounce window for engine restarts: rapid FX toggles are coalesced into one
# restart if they happen within this window.
DEBOUNCE_ENGINE_RESTART_MS = 200
# VU meter refresh rate: 40ms = 25 FPS, smooth without overloading the CPU.
METER_POLL_INTERVAL_MS = 40
# How often the app re-asserts app→strip routing, to catch apps started after
# the last enforcement or whose routing was changed externally.
ENFORCE_ROUTING_INTERVAL_MS = 5000
# Background thread pool size for backend tasks.
THREAD_POOL_MAX_WORKERS = 4
# Initial MIDI feedback settle time after port open (controllers need this
# before they'll accept LED update messages).
MIDI_FEEDBACK_SETTLE_MS = 200

# --- Application context menu (right click anywhere) ---
# The menu replaced the old ``Ctrl+Shift+D`` shortcut: a hidden keyboard
# shortcut is undiscoverable (users reported "nothing happens"), whereas a
# right click is what they try first.  Same entries everywhere in the window,
# so the order and the labels live here — one place to read and to test.
MENU_RESTART_ENGINE = "Restart audio engine"
MENU_DEBUG_WINDOW = "Debug window"
MENU_OPEN_PROJECT_PAGE = "Open project page"

# Project home page opened by "Open project page" (bug reports, docs, releases).
PROJECT_PAGE_URL = "https://github.com/grokuku/Holaf-Mix"

# How long a success notice stays in the status bar (an error notice stays).
RESTART_NOTICE_TIMEOUT_MS = 8000

# --- Custom Dialog for App Selection ---
class AppSelectionDialog(QDialog):
    def __init__(self, running_apps, assigned_apps, parent=None):
        super().__init__(parent)
        self.setWindowTitle("Assign Applications")
        self.resize(300, 400)
        self.selected_apps = []
        
        layout = QVBoxLayout(self)
        
        info = QLabel("Select applications to route to this strip:")
        info.setWordWrap(True)
        layout.addWidget(info)
        
        self.scroll = QScrollArea()
        self.scroll.setWidgetResizable(True)
        self.scroll_content = QWidget()
        self.scroll_layout = QVBoxLayout(self.scroll_content)
        self.scroll.setWidget(self.scroll_content)
        layout.addWidget(self.scroll)
        
        self.check_boxes = []
        
        all_names = set([app['name'] for app in running_apps])
        all_names.update(assigned_apps)
        
        for name in sorted(all_names):
            cb = QCheckBox(name)
            if name in assigned_apps:
                cb.setChecked(True)
            self.scroll_layout.addWidget(cb)
            self.check_boxes.append(cb)
            
        self.scroll_layout.addStretch()
        
        buttons = QDialogButtonBox(QDialogButtonBox.Ok | QDialogButtonBox.Cancel)
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        layout.addWidget(buttons)

    def get_selected_apps(self):
        return [cb.text() for cb in self.check_boxes if cb.isChecked()]

# --- Worker for ThreadPool ---
class BackendWorker(QRunnable):
    def __init__(self, func, *args, **kwargs):
        super().__init__()
        self.func = func
        self.args = args
        self.kwargs = kwargs

    @Slot()
    def run(self):
        try:
            self.func(*self.args, **self.kwargs)
        except Exception as e:
            print(f"Backend Task Error: {e}")

class MainWindow(QMainWindow):
    # Emitted by the restart worker (any thread); the connected slot runs on the
    # UI thread because the receiver lives there (Qt auto → queued connection).
    engine_restart_done = Signal(bool, str)

    def __init__(self, audio_engine=None, midi_engine=None):
        super().__init__()
        self.setWindowTitle("Holaf-Mix")
        
        # --- WINDOW FLAGS (Always on Top) ---
        self.setWindowFlags(self.windowFlags() | Qt.WindowStaysOnTopHint)
        
        self.audio_engine = audio_engine
        self.midi_engine = midi_engine
        self.thread_pool = QThreadPool()
        self.thread_pool.setMaxThreadCount(THREAD_POOL_MAX_WORKERS)

        # Engine restart coalescing (prevents stacking multiple restarts)
        self._restart_pending = False
        self._restart_timer = QTimer(self)
        self._restart_timer.setSingleShot(True)
        self._restart_timer.setInterval(DEBOUNCE_ENGINE_RESTART_MS)
        self._restart_timer.timeout.connect(self._do_engine_restart)
        
        self.midi_lookup = {}
        self.widgets = {}
        
        # --- Load Config (Strips + Geometry) ---
        self.strips, window_geo_hex = settings.load_config()
        if window_geo_hex:
            self.restoreGeometry(QByteArray.fromHex(window_geo_hex.encode()))

        self._rebuild_midi_lookup()
        
        self.central_widget = QWidget()
        self.setCentralWidget(self.central_widget)
        
        self.main_layout = QHBoxLayout(self.central_widget)
        self.main_layout.setSpacing(0)
        self.main_layout.setContentsMargins(0, 0, 0, 0)
        
        self.inputs_container = self._create_section("INPUTS", "#2b2b2b", StripType.INPUT)
        self.main_layout.addWidget(self.inputs_container)

        self.outputs_container = self._create_section("OUTPUTS", "#1e1e1e", StripType.OUTPUT)
        self.main_layout.addWidget(self.outputs_container)
        
        self.refresh_ui()
        self._init_midi()
        self._init_tray_icon()

        # Timer for routing enforcement
        self.enforce_timer = QTimer(self)
        self.enforce_timer.timeout.connect(self._enforce_app_routing)
        self.enforce_timer.start(ENFORCE_ROUTING_INTERVAL_MS)

        # Timer for VU Meters (High frequency)
        self.meter_timer = QTimer(self)
        self.meter_timer.setInterval(METER_POLL_INTERVAL_MS)
        self.meter_timer.timeout.connect(self._update_meters)
        self.meter_timer.start()

        # Hidden diagnostics window: reachable from the context menu
        # ("Debug window") only.  It is a debugging tool, not a feature.
        self._diagnostics_dialog = None
        # True while a context menu is exec()ing (one menu at a time).
        self._context_menu_open = False

        # --- Full "Restart audio engine" state (context menu) ---
        # One restart at a time: a second click while a restart is running is
        # ignored (and said so in the status bar) instead of stacking workers
        # that would race on the same audio graph.
        self._full_restart_running = False
        self._wait_cursor_shown = False
        # Emitted from the worker thread; the slot runs on the UI thread
        # (queued connection), which is how the worker reports back without
        # touching widgets from its own thread.
        self.engine_restart_done.connect(self._on_engine_restart_done)

        # --- Right click anywhere opens the application menu ---
        # Two complementary mechanisms, because a widget tree has two ways of
        # receiving a right click:
        #
        # * ``contextMenuEvent`` on the window itself — the background, the
        #   empty areas, and (by Qt propagation of an ignored context event)
        #   anything that does not handle it itself;
        # * an event filter on every widget of this window whose policy is
        #   ``Qt.DefaultContextMenu`` — installed in
        #   :meth:`_install_context_menu_filters` and refreshed after each
        #   ``refresh_ui`` (strips are recreated there), so a right click on a
        #   strip, a VU meter or a section header opens the menu even if that
        #   widget would have swallowed the event.
        #
        # The filter consumes the event when it opens the menu (returns True),
        # so one click can never open the menu twice.  Widgets that *do* own a
        # context menu (strip name label → "Rename…", FX buttons → effect
        # parameters) have ``Qt.CustomContextMenu`` and are left alone: they
        # append the application entries to their own menu instead (see
        # ``StripWidget.app_menu_provider``).
        self._install_context_menu_filters()

    # -------------------------------------------------- application menu
    def _install_context_menu_filters(self):
        """Route the right clicks of every child widget to :meth:`eventFilter`.

        Cheap and idempotent (a few dozen widgets, called again after each
        ``refresh_ui``): installing on the whole QApplication instead would put
        a Python call in front of *every* event of the process, including the
        25 Hz VU-meter repaints.
        """
        for widget in self.findChildren(QWidget):
            if widget.contextMenuPolicy() not in (Qt.DefaultContextMenu, Qt.NoContextMenu):
                continue  # owns its menu (CustomContextMenu) or has none at all
            widget.removeEventFilter(self)  # idempotent install
            widget.installEventFilter(self)

    def build_app_context_menu(self, menu=None):
        """Return the application context menu (right click anywhere).

        Entries, in this order:

        1. **Restart audio engine** — full reset without quitting the app;
        2. **Debug window** — the diagnostics window (used to be `Ctrl+Shift+D`);
        3. *separator*;
        4. **Open project page** — the GitHub home page.

        ``menu`` (optional) is an existing QMenu to extend: child widgets that
        own a local context menu (strip label, FX buttons) pass theirs so the
        application entries are appended after their own, separated by a
        separator line.
        """
        own = menu is None
        if own:
            menu = QMenu(self)
        else:
            menu.addSeparator()

        action_restart = menu.addAction(MENU_RESTART_ENGINE)
        action_restart.triggered.connect(self.restart_audio_engine)
        action_debug = menu.addAction(MENU_DEBUG_WINDOW)
        action_debug.triggered.connect(self.open_diagnostics)
        menu.addSeparator()
        action_project = menu.addAction(MENU_OPEN_PROJECT_PAGE)
        action_project.triggered.connect(self.open_project_page)
        if own:
            # Not part of the visible entries, but handy for tests/tools that
            # need to find an entry back from the menu.
            menu.setObjectName("appContextMenu")
        return menu

    def _show_app_context_menu(self, global_pos):
        """Open the application menu at ``global_pos`` (one at a time)."""
        if self._context_menu_open:
            return
        self._context_menu_open = True
        try:
            self.build_app_context_menu().exec(global_pos)
        finally:
            self._context_menu_open = False

    def _is_own_context_menu_target(self, obj):
        """Should a right click on ``obj`` open the application menu?

        True for every child widget of this window that does not already own a
        context menu (the window itself is covered by
        :meth:`contextMenuEvent`).  Qt popups (menus, combo-box lists) and
        widgets of another window are left alone.
        """
        if not isinstance(obj, QWidget):
            return False
        try:
            if obj.window() is not self:
                return False
        except RuntimeError:  # widget already deleted by the C++ side
            return False
        return obj.contextMenuPolicy() in (Qt.DefaultContextMenu, Qt.NoContextMenu)

    def eventFilter(self, obj, event):
        """Open the application menu on any right click inside this window.

        See the installation site in ``__init__`` for why a filter is used on
        top of ``contextMenuEvent``.  Returning True consumes the event, so the
        menu can never be opened twice for one click (once by the filter, once
        by the widget's own ``contextMenuEvent`` propagation).
        """
        if event.type() == QEvent.ContextMenu and self._is_own_context_menu_target(obj):
            self._show_app_context_menu(self._event_global_pos(event))
            return True
        return super().eventFilter(obj, event)

    def contextMenuEvent(self, event):  # noqa: N802 (Qt API)
        """Right click on the window itself (background, empty areas)."""
        self._show_app_context_menu(self._event_global_pos(event))
        event.accept()

    @staticmethod
    def _event_global_pos(event):
        """Global position of a context-menu event (mouse fallback)."""
        global_pos = getattr(event, "globalPos", None)
        if callable(global_pos):
            try:
                return global_pos()
            except Exception:  # noqa: BLE001 - non-mouse context event
                pass
        return QCursor.pos()

    # ------------------------------------------------------- menu actions
    def open_project_page(self):
        """Menu action: open the Holaf-Mix GitHub page in the browser."""
        opened = False
        try:
            opened = QDesktopServices.openUrl(QUrl(PROJECT_PAGE_URL))
        except Exception as exc:  # noqa: BLE001 - a browser failure must not crash
            logger.error("openUrl(%s) failed: %s", PROJECT_PAGE_URL, exc)
        if not opened:
            logger.warning("could not open the project page (%s)", PROJECT_PAGE_URL)
            self._report_failure(
                "Could not open the project page",
                f"No browser could be launched for:\n{PROJECT_PAGE_URL}",
            )

    def restart_audio_engine(self):
        """Menu action: restart the audio engine without leaving the app.

        The engine work (``shutdown`` → fresh daemon / ``start_engine``) blocks
        for as long as PipeWire needs, so it always runs on a
        ``BackendWorker`` thread: the UI stays responsive, the cursor turns
        into a wait cursor and the status bar narrates the operation.

        Non-reentrant: while a restart is in flight, further clicks are
        ignored with an explicit status message (two concurrent restarts would
        race on the same graph).  Works with both engines (Rust daemon and
        classic Python engine).
        """
        if not self.audio_engine:
            self._report_failure(
                "No audio engine",
                "This window was started without an audio engine — nothing to restart.",
            )
            return
        if self._full_restart_running:
            logger.info("audio engine restart already in progress — click ignored")
            self._show_status("Audio engine restart already in progress…",
                              RESTART_NOTICE_TIMEOUT_MS)
            return

        self._full_restart_running = True
        if not self._wait_cursor_shown:
            QApplication.setOverrideCursor(Qt.WaitCursor)
            self._wait_cursor_shown = True
        self._show_status("Audio engine restarting…")
        logger.info("audio engine restart requested (context menu)")
        # Snapshot of the strips: the UI thread may add/remove/rename strips
        # while the worker runs, and start_engine() must see one coherent list.
        self._run_in_background(self._restart_engine_task, list(self.strips))

    def _restart_engine_task(self, strips):
        """Worker: restart the engine, then report back on the UI thread.

        Never raises (a worker traceback would only land in the app's stderr,
        which a desktop-launched app does not have): the outcome is emitted as
        ``engine_restart_done(ok, message)`` — ``message`` is the notice on
        success and the *reason* on failure — and rendered by
        :meth:`_on_engine_restart_done`.
        """
        try:
            self._restart_engine_now(strips)
        except Exception as exc:  # noqa: BLE001 - reported, never raised
            logger.exception("audio engine restart failed")
            reason = str(exc) or exc.__class__.__name__
            self.engine_restart_done.emit(False, reason)
            return
        logger.info("audio engine restarted")
        self.engine_restart_done.emit(True, "Audio engine restarted")

    def _restart_engine_now(self, strips):
        """Blocking half of the restart (worker thread only).

        Rust engine
            ``shutdown()`` → ``stop_daemon()`` when this process owns the
            daemon (a fresh daemon is the actual point of the reset: it clears
            a stuck graph or a misbehaving daemon) → ``ensure_daemon_running()``
            (idempotent: it also revives a daemon that died) →
            ``start_engine(strips)``.  A daemon started by the user by hand is
            never killed — exactly the ownership rule used at app quit — but
            the graph is rebuilt all the same.

        Python engine
            ``shutdown()`` then ``start_engine(strips)``.

        In both cases ``start_engine`` re-reads the daemon's meter sources
        (``IpcAudioEngine._sync_meter_sources``) or rebuilds its own capture
        streams, so the VU meters come back without extra work here.
        """
        engine = self.audio_engine
        owned_daemon = bool(getattr(engine, "_rust_daemon_spawned_by_us", False))
        is_rust = hasattr(engine, "_rust_daemon_spawned_by_us")

        engine.shutdown()  # best effort on both engines

        if is_rust:
            from src.backend import rust_launcher

            if owned_daemon:
                # A stop failure must not abort the restart: the next step
                # (ensure_daemon_running) reuses whatever is still alive.
                try:
                    if not rust_launcher.stop_daemon():
                        logger.info("restart: daemon was already gone — respawning it")
                except Exception:  # noqa: BLE001 - revive below regardless
                    logger.warning("restart: could not stop the daemon", exc_info=True)
            handle = rust_launcher.ensure_daemon_running()
            if not getattr(handle, "ok", False):
                reason = getattr(handle, "reason", "") or "holafd is not answering"
                raise RuntimeError(reason)
            # Ownership may have changed: a daemon we did not have to stop is
            # no longer ours, and the app-quit teardown must not kill it.
            engine._rust_daemon_spawned_by_us = bool(getattr(handle, "spawned_by_us", False))

        engine.start_engine(strips)

    @Slot(bool, str)
    def _on_engine_restart_done(self, ok, message):
        """UI thread: end the wait cursor and report the restart outcome."""
        self._full_restart_running = False
        if self._wait_cursor_shown:
            QApplication.restoreOverrideCursor()
            self._wait_cursor_shown = False
        if ok:
            self._show_status(message, RESTART_NOTICE_TIMEOUT_MS)
        else:
            self._show_status(f"Audio engine restart failed: {message}")
            self._report_failure(
                "Audio engine restart failed",
                f"{message}\n\nThe app keeps running. The log and the current "
                "graph state are in the Debug window (right click → Debug window).",
            )

    def _show_status(self, message, timeout_ms=0):
        """Show ``message`` in the status bar (never raises)."""
        try:
            self.statusBar().showMessage(message, timeout_ms)
        except Exception:  # noqa: BLE001 - the notice is best effort
            logger.debug("could not show '%s' in the status bar", message)

    def _report_failure(self, title, message):
        """Tell the user something failed, without ever raising.

        Logged first (the log feed is shown in the diagnostics window) then a
        single modal warning: a failing menu action must be visible, not
        silent — that is what made the old hidden shortcut feel broken.
        """
        logger.error("%s: %s", title, message)
        try:
            QMessageBox.warning(self, title, message)
        except Exception:  # noqa: BLE001 - a modal failure must not crash either
            logger.debug("could not show the '%s' warning box", title)

    def open_diagnostics(self):
        """Show (or raise) the diagnostics window — "Debug window" menu entry.

        The dialog collects the whole state on a worker thread of this
        window's pool, so opening it never blocks the UI even when the daemon
        or PipeWire is unresponsive.  The dialog module is imported here (not
        at module level) so a run that never opens it does not pay for the
        import — and a failed import is reported to the user instead of dying
        as a traceback on a stderr nobody sees (a lazy import inside a frozen
        bundle is exactly where that can happen).
        """
        try:
            from src.ui.diagnostics_dialog import DiagnosticsDialog

            if self._diagnostics_dialog is None:
                self._diagnostics_dialog = DiagnosticsDialog(
                    audio_engine=self.audio_engine,
                    # Callable, not a snapshot: the strip list is live data.
                    strips=lambda: self.strips,
                    thread_pool=self.thread_pool,
                    parent=self,
                )
            dialog = self._diagnostics_dialog
            dialog.show()
            dialog.raise_()
            dialog.activateWindow()
        except Exception as exc:  # noqa: BLE001 - reported, never raised
            logger.exception("could not open the diagnostics window")
            self._report_failure(
                "Debug window unavailable",
                "The diagnostics window could not be opened:\n"
                f"{exc.__class__.__name__}: {exc}",
            )
            return
        # The dialog re-collects itself in showEvent(), so a reopened window
        # always shows fresh state (a stale report is worse than none).

    def _init_tray_icon(self):
        """Sets up the System Tray Icon for minimize/restore behavior."""
        self.tray_icon = QSystemTrayIcon(self)
        
        # We need an icon. Using standard system icon.
        style = QApplication.style()
        # FIX: Access SP_MediaVolume via QStyle class, not instance
        icon = style.standardIcon(QStyle.SP_MediaVolume)
        self.tray_icon.setIcon(icon)
        
        # Tray Menu
        tray_menu = QMenu()
        restore_action = QAction("Show/Hide", self)
        restore_action.triggered.connect(self._toggle_window)
        tray_menu.addAction(restore_action)
        
        quit_action = QAction("Quit", self)
        quit_action.triggered.connect(self._force_quit)
        tray_menu.addAction(quit_action)
        
        self.tray_icon.setContextMenu(tray_menu)
        self.tray_icon.activated.connect(self._on_tray_activated)
        self.tray_icon.show()

    def _toggle_window(self):
        if self.isVisible():
            self.hide()
        else:
            self.showNormal()
            self.activateWindow()

    def _on_tray_activated(self, reason):
        if reason == QSystemTrayIcon.Trigger:
            self._toggle_window()

    def _force_quit(self):
        """Actually close the app."""
        self._save_state()
        QApplication.quit()

    def closeEvent(self, event):
        """
        Override close event to minimize instead of quitting,
        BUT save state just in case.
        """
        self._save_state()
        if self.tray_icon.isVisible():
            self.hide()
            event.ignore()
        else:
            event.accept()

    def _save_state(self):
        """Saves current strips config and window geometry."""
        geo_hex = self.saveGeometry().toHex().data().decode()
        settings.save_config(self.strips, geo_hex)

    def _init_midi(self):
        if not self.midi_engine: return
        self.midi_engine.mapping_detected.connect(self.on_midi_mapping_detected)
        self.midi_engine.message_received.connect(self.on_midi_message_received)
        ports = self.midi_engine.get_input_names()
        if ports:
            target = next((p for p in ports if "mix" in p.lower()), ports[0])
            if self.midi_engine.open_port(target):
                    # Wait a tiny bit then sync LEDs on startup
                    QTimer.singleShot(MIDI_FEEDBACK_SETTLE_MS, self._sync_initial_midi_leds)

    def _sync_initial_midi_leds(self):
        """Updates controller LEDs to match current app state."""
        for strip in self.strips:
            self._send_midi_feedback(strip, "mute", strip.mute)
            self._send_midi_feedback(strip, "mono", strip.is_mono)

    def _send_midi_feedback(self, strip, prop_name, is_active):
        """Helper to send feedback to the controller."""
        if not self.midi_engine: return
        
        mapping = None
        if prop_name == "mute":
            mapping = strip.midi_mute
        elif prop_name == "mono":
            mapping = strip.midi_mono
            
        if mapping:
            self.midi_engine.send_feedback(mapping, is_active)

    def _rebuild_midi_lookup(self):
        self.midi_lookup.clear()
        for strip in self.strips:
            # Volume
            if strip.midi_volume:
                m = strip.midi_volume
                key = (m['type'], m['channel'], m.get('control') or m.get('note'))
                self.midi_lookup[key] = (strip.uid, "volume")
            # Mute
            if strip.midi_mute:
                m = strip.midi_mute
                key = (m['type'], m['channel'], m.get('control') or m.get('note'))
                self.midi_lookup[key] = (strip.uid, "mute")
            # Mono
            if strip.midi_mono:
                m = strip.midi_mono
                key = (m['type'], m['channel'], m.get('control') or m.get('note'))
                self.midi_lookup[key] = (strip.uid, "mono")

    def on_midi_message_received(self, msg):
        identifier = getattr(msg, 'control', None) if msg.type == 'control_change' else getattr(msg, 'note', None)
        if identifier is None: return
        
        key = (msg.type, msg.channel, identifier)
        if key in self.midi_lookup:
            uid, prop = self.midi_lookup[key]
            strip = next((s for s in self.strips if s.uid == uid), None)
            if not strip: return
            
            # --- ACTION LOGIC ---
            if prop == "volume":
                strip.volume = msg.value / 127.0
            
            elif prop == "mute":
                val = getattr(msg, 'velocity', 127) if hasattr(msg, 'velocity') else msg.value
                # Toggle only on button press (value > 0), ignore release (value 0)
                if val > 0: 
                    strip.mute = not strip.mute
                    self._run_in_background(self.audio_engine.set_mute, strip.uid, strip.mute)
                    self._send_midi_feedback(strip, "mute", strip.mute)

            elif prop == "mono":
                val = getattr(msg, 'velocity', 127) if hasattr(msg, 'velocity') else msg.value
                # Toggle only on button press
                if val > 0:
                    strip.is_mono = not strip.is_mono
                    self._run_in_background(self.audio_engine.set_mono, strip.uid, strip.is_mono)
                    self._send_midi_feedback(strip, "mono", strip.is_mono)

            # Refresh UI
            widget = self.widgets.get(uid)
            if widget:
                QTimer.singleShot(0, widget.update_ui_from_model)

    def on_midi_mapping_detected(self, uid, prop, mapping):
        strip = next((s for s in self.strips if s.uid == uid), None)
        if strip:
            if prop == "volume": 
                strip.midi_volume = mapping
            elif prop == "mute": 
                strip.midi_mute = mapping
                # Sync LED immediately
                self._send_midi_feedback(strip, "mute", strip.mute)
            elif prop == "mono": 
                strip.midi_mono = mapping
                # Sync LED immediately
                self._send_midi_feedback(strip, "mono", strip.is_mono)
                
            self._save_state()
        
        widget = self.widgets.get(uid)
        if widget: widget.set_learning(False)
        self._rebuild_midi_lookup()

    def on_midi_learn_requested(self, uid, prop):
        if self.midi_engine:
            widget = self.widgets.get(uid)
            if widget: widget.set_learning(True)
            self.midi_engine.start_learning(uid, prop)

    def _create_section(self, title, bg_color, strip_kind):
        container = QWidget()
        container.setStyleSheet(f"background-color: {bg_color};")
        container.setSizePolicy(QSizePolicy.Preferred, QSizePolicy.Expanding)
        
        layout = QVBoxLayout(container)
        layout.setContentsMargins(0, 0, 0, 0)
        
        header_widget = QWidget()
        header_layout = QHBoxLayout(header_widget)
        header_layout.setContentsMargins(10, 10, 10, 0)
        
        label = QLabel(title)
        label.setStyleSheet("font-weight: bold; color: white; font-size: 14px;")
        
        btn_add = QPushButton("+")
        btn_add.setFixedSize(30, 30)
        btn_add.setCursor(Qt.PointingHandCursor)
        btn_add.setStyleSheet("""
            QPushButton { background-color: #444; color: white; border-radius: 15px; font-weight: bold; }
            QPushButton:hover { background-color: #666; }
        """)
        btn_add.clicked.connect(lambda: self.on_add_clicked(strip_kind))

        header_layout.addWidget(label)
        header_layout.addSpacing(10)
        header_layout.addWidget(btn_add)
        header_layout.addStretch() 
        
        layout.addWidget(header_widget)

        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setFrameShape(QFrame.NoFrame)
        scroll.setStyleSheet("background: transparent;")
        
        content_widget = QWidget()
        content_layout = QHBoxLayout(content_widget)
        content_layout.setAlignment(Qt.AlignTop | Qt.AlignLeft)
        content_layout.setSpacing(10)
        content_layout.setContentsMargins(10, 10, 10, 10) 
        content_widget.setLayout(content_layout)
        scroll.setWidget(content_widget)
        layout.addWidget(scroll)
        container.content_layout = content_layout 
        return container

    def refresh_ui(self):
        self._clear_layout(self.inputs_container.content_layout)
        self._clear_layout(self.outputs_container.content_layout)
        self.widgets.clear()
        
        output_strips = [s for s in self.strips if s.kind == StripType.OUTPUT]

        for strip in self.strips:
            widget = StripWidget(strip)
            self.widgets[strip.uid] = widget
            # Right click on a strip name / FX button keeps its local menu but
            # gains the application entries (see MainWindow.build_app_context_menu).
            widget.app_menu_provider = self.build_app_context_menu
            
            # --- SIGNAL CONNECTIONS ---
            widget.volume_changed.connect(self.on_strip_volume_changed)
            widget.mute_changed.connect(self.on_strip_mute_changed)
            widget.mono_changed.connect(self.on_strip_mono_changed) 
            widget.label_changed.connect(self.on_strip_label_changed)
            widget.delete_requested.connect(self.on_strip_delete_requested)
            widget.route_changed.connect(self.on_strip_route_changed)
            widget.midi_learn_requested.connect(self.on_midi_learn_requested)
            widget.device_changed.connect(self.on_strip_device_changed)
            widget.app_selection_requested.connect(self.on_app_selection_requested)
            widget.default_changed.connect(self.on_strip_default_changed) 
            widget.effect_toggled.connect(self.on_strip_effect_toggled)
            widget.effect_params_changed.connect(self.on_strip_effect_params_changed)
            
            if strip.kind == StripType.INPUT:
                widget.set_routing_targets(output_strips)
                self.inputs_container.content_layout.addWidget(widget)
            else:
                self.outputs_container.content_layout.addWidget(widget)
        
        self._populate_devices()

        # Strips were just (re)created: route their right clicks to the
        # application menu (see _install_context_menu_filters).
        self._install_context_menu_filters()
        
        # Increase delay to ensure layout count is accurate
        QTimer.singleShot(50, self._adjust_window_size)

    def _adjust_window_size(self):
        """
        Calculates required width and forces distribution via Stretch Factors.
        """
        STRIP_WIDTH = 100
        SPACING = 10
        MARGIN_TOTAL_PER_SECTION = 22
        MIN_SECTION_WIDTH = 160 
        
        def calculate_section_width(layout):
            count = layout.count()
            if count == 0:
                return MIN_SECTION_WIDTH
            total = (count * STRIP_WIDTH) + (max(0, count - 1) * SPACING) + MARGIN_TOTAL_PER_SECTION
            return max(total, MIN_SECTION_WIDTH)

        width_inputs = calculate_section_width(self.inputs_container.content_layout)
        width_outputs = calculate_section_width(self.outputs_container.content_layout)
        
        self.main_layout.setStretch(0, width_inputs)
        self.main_layout.setStretch(1, width_outputs)

        total_width = width_inputs + width_outputs
        
        self.setMinimumWidth(0)
        self.setFixedWidth(total_width)
    
    def _update_meters(self):
        """
        Polls the audio engine for VU meter levels and updates the widgets.
        """
        if not self.audio_engine: return
        
        levels = self.audio_engine.get_meter_levels()
        for uid, (left, right) in levels.items():
            widget = self.widgets.get(uid)
            if widget:
                widget.update_vumeter(left, right)

    def _populate_devices(self):
        nodes = pipewire_utils.get_audio_nodes()
        
        # Sort nodes alphabetically by description for better UX
        nodes.sort(key=lambda x: x.get('description', '').lower())
        
        sinks = [n for n in nodes if n.get('media_class') == 'Audio/Sink']
        sources = [n for n in nodes if n.get('media_class') == 'Audio/Source']

        for uid, widget in self.widgets.items():
            if widget.strip.kind == StripType.OUTPUT:
                widget.set_device_list(sinks)
            else:
                widget.set_device_list(sources)

    def _clear_layout(self, layout):
        while layout.count():
            item = layout.takeAt(0)
            if item.widget(): item.widget().deleteLater()

    # --- Actions ---

    def on_add_clicked(self, kind):
        name, ok = QInputDialog.getText(self, "Add Strip", "Enter name for new strip:")
        if ok and name:
            new_strip = Strip(label=name, kind=kind, mode=StripMode.VIRTUAL)
            self.strips.append(new_strip)
            self._save_state()
            if self.audio_engine:
                self._run_in_background(self.audio_engine.start_engine, self.strips)
            self.refresh_ui()

    def on_strip_delete_requested(self, uid):
        strip_to_remove = next((s for s in self.strips if s.uid == uid), None)
        if not strip_to_remove: return
        reply = QMessageBox.question(self, "Confirm Delete", 
            f"Are you sure you want to remove '{strip_to_remove.label}'?",
            QMessageBox.Yes | QMessageBox.No, QMessageBox.No)
        if reply == QMessageBox.Yes:
            self.strips.remove(strip_to_remove)
            if strip_to_remove.kind == StripType.OUTPUT:
                for s in self.strips:
                    if s.kind == StripType.INPUT and strip_to_remove.uid in s.routes:
                        s.routes.remove(strip_to_remove.uid)
            self._save_state()
            self._schedule_engine_restart()
            self.refresh_ui()

    def on_app_selection_requested(self, uid):
        strip = next((s for s in self.strips if s.uid == uid), None)
        if not strip: return
        running_apps = pipewire_utils.get_sink_inputs()
        dlg = AppSelectionDialog(running_apps, strip.assigned_apps, self)
        if dlg.exec():
            selected = dlg.get_selected_apps()
            strip.assigned_apps = selected
            self._save_state()
            self._move_apps_to_strip(strip, running_apps)

    def _strip_sink_name(self, strip):
        """Name of the sink an application must be moved to for `strip`.

        The naming rule lives in the daemon: a virtual strip's bus is named
        after its **label** (`Holaf_Bus_Mix_Out`, `_2` on a label collision —
        see `holaf_core/graph.rs` and docs/rust-rewrite-status.md), so the name
        survives the strip being deleted and recreated, and an external program
        can target it.  Ask the engine first (the Rust daemon publishes it as
        ``status.strips[].node_name``) and fall back to the classic Python
        engine's uid-based name when there is no daemon to ask.
        """
        getter = getattr(self.audio_engine, "strip_node_name", None)
        if callable(getter):
            try:
                name = getter(strip.uid)
            except Exception as exc:  # never break app routing on a name lookup
                logger.debug("strip_node_name(%s) failed: %s", strip.uid, exc)
                name = None
            if name:
                return name
        return f"Holaf_Strip_{strip.uid}"

    def _move_apps_to_strip(self, strip, running_apps=None):
        if not running_apps:
            running_apps = pipewire_utils.get_sink_inputs()
        target_sink_name = self._strip_sink_name(strip)
        for app in running_apps:
            if app['name'] in strip.assigned_apps:
                pipewire_utils.move_sink_input(app['id'], target_sink_name)

    def _enforce_app_routing(self):
        running_apps = pipewire_utils.get_sink_inputs()
        for strip in self.strips:
            if strip.kind == StripType.INPUT and strip.mode == StripMode.VIRTUAL:
                    if strip.assigned_apps and not strip.is_default:
                        self._move_apps_to_strip(strip, running_apps)

    # --- Backend Interfacing ---
    def _run_in_background(self, func, *args):
        if self.audio_engine:
            worker = BackendWorker(func, *args)
            self.thread_pool.start(worker)

    def _schedule_engine_restart(self):
        """Coalesce engine restarts: if one is already pending, skip."""
        if self._restart_pending:
            return
        self._restart_pending = True
        self._restart_timer.start()

    def _do_engine_restart(self):
        """Execute a single engine restart after debounce period."""
        self._restart_pending = False
        if not self.audio_engine:
            return
        def reload():
            self.audio_engine.shutdown()
            self.audio_engine.start_engine(self.strips)
        self._run_in_background(reload)

    def on_strip_volume_changed(self, uid, new_vol):
        self._run_in_background(self.audio_engine.set_volume, uid, new_vol)

    def on_strip_mute_changed(self, uid, is_muted):
        self._run_in_background(self.audio_engine.set_mute, uid, is_muted)
        # Update Controller LEDs
        strip = next((s for s in self.strips if s.uid == uid), None)
        if strip:
            self._send_midi_feedback(strip, "mute", is_muted)

    def on_strip_mono_changed(self, uid, is_mono):
        self._run_in_background(self.audio_engine.set_mono, uid, is_mono)
        self._save_state()
        # Update Controller LEDs
        strip = next((s for s in self.strips if s.uid == uid), None)
        if strip:
            self._send_midi_feedback(strip, "mono", is_mono)
    
    def on_strip_label_changed(self, uid, new_label):
        self._save_state()

    def on_strip_route_changed(self, source_uid, target_uid, active):
        self._run_in_background(self.audio_engine.update_routing, source_uid, target_uid, active)
        self._save_state()

    def on_strip_device_changed(self, uid, device_name):
        strip = next((s for s in self.strips if s.uid == uid), None)
        if not strip: return
        strip.device_name = device_name
        if device_name:
            strip.mode = StripMode.PHYSICAL
        else:
            strip.mode = StripMode.VIRTUAL
            strip.device_name = None
        self._save_state()
        self._schedule_engine_restart()

    def on_strip_default_changed(self, uid, is_default):
        if is_default:
            for s in self.strips:
                if s.kind == StripType.INPUT and s.uid != uid:
                    s.is_default = False
                    w = self.widgets.get(s.uid)
                    if w: w.set_default_state(False)
            target = next((s for s in self.strips if s.uid == uid), None)
            if target: target.is_default = True
            self._save_state()
            if self.audio_engine:
                self._run_in_background(self.audio_engine.set_system_default, uid)

    def on_strip_effect_toggled(self, uid, effect_name, is_active):
        """Called when an FX button is clicked.

        The actual engine work is delegated to ``_apply_fx_change`` running
        on a BackendWorker thread: ``update_fx_params`` blocks while doing
        the pw-cli set-param round-trip (sleep + pipe read), so it must
        NEVER run on the UI thread.

        Inside the worker, the toggle is first applied via set-param
        (hot-reload) — switching between real params and neutral bypass
        values without recreating the filter-chain node.

        If no filter-chain exists yet or hot-reload fails, the worker falls
        back to a **partial reload** via ``reload_strip`` which only
        destroys and recreates the FX chain for *this* strip.  If that also
        fails, the last resort is a full engine restart.  Subsequent
        toggles then use hot-reload again.
        """
        self._save_state()
        strip = next((s for s in self.strips if s.uid == uid), None)
        if not strip or not self.audio_engine:
            return
        # Never call the engine directly from the UI thread — submit the
        # whole apply chain (hot-reload → partial reload → full restart)
        # to the background worker pool.
        self._run_in_background(self._apply_fx_change, strip)

    def on_strip_effect_params_changed(self, uid, effect_name):
        """Called when FX params are modified via the settings dialog."""
        self._save_state()
        strip = next((s for s in self.strips if s.uid == uid), None)
        if not strip or not self.audio_engine:
            return
        # Only apply if the modified effect is currently active
        fx_data = strip.effects.get(effect_name, {})
        is_active = fx_data.get('active', False) if isinstance(fx_data, dict) else bool(fx_data)
        if is_active:
            # Same contract as toggles: everything runs off the UI thread.
            self._run_in_background(self._apply_fx_change, strip)

    def _apply_fx_change(self, strip):
        """Background worker applying an FX change for *strip*, in order:

        1. Hot-reload via pw-cli set-param (no audio interruption).
        2. Partial reload of this strip's FX chain only (other strips are
           untouched, so WirePlumber sees no device change for them).
        3. Full engine restart as last resort.

        Every step blocks (subprocess round-trips), so this MUST run on a
        BackendWorker thread — never on the UI thread.  Each step is
        individually guarded so an exception still triggers the next
        fallback instead of silently dropping the change.
        """
        if not self.audio_engine:
            return
        label = strip.label

        # --- Step 1: hot-reload (pw-cli set-param) ---
        try:
            if self.audio_engine.update_fx_params(strip):
                logger.info(f"FX change '{label}': applied via hot-reload.")
                return
            logger.info(f"FX change '{label}': hot-reload unavailable, trying partial reload.")
        except Exception as e:
            logger.error(f"FX change '{label}': hot-reload exception: {e}; trying partial reload.")

        # --- Step 2: partial reload (this strip's FX chain only) ---
        try:
            if self.audio_engine.reload_strip(strip):
                logger.info(f"FX change '{label}': applied via partial strip reload.")
                return
            logger.warning(f"FX change '{label}': partial reload failed; falling back to full restart.")
        except Exception as e:
            logger.error(f"FX change '{label}': partial reload exception: {e}; falling back to full restart.")

        # --- Step 3: full engine restart (last resort) ---
        try:
            self.audio_engine.shutdown()
            self.audio_engine.start_engine(self.strips)
            logger.info(f"FX change '{label}': applied via full engine restart.")
        except Exception as e:
            logger.error(f"FX change '{label}': full engine restart failed: {e}")
