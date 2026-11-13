"""Hidden diagnostics window (Ctrl+Shift+D) — the Qt half of the diagnostics.

Why
---
When the mix misbehaves, the information needed to understand it is spread
over the engine state, the PipeWire graph and a dozen shell commands.  This
window gathers it in one place, with an automatic findings list on top and a
single **Copy report** button so the user can paste one block into a bug
report (or ask an agent for help) instead of running ``pactl`` / ``pw-cli`` /
``pw-link`` / ``pw-top`` / ``holafctl status`` by hand.

Separation of concerns
----------------------
This module contains **no** collection logic and **no** PipeWire call: it
calls :func:`src.diagnostics.collector.collect_report` on a worker thread and
renders the resulting dict with
:func:`src.diagnostics.collector.format_sections`.  That keeps the collection
unit-testable without Qt (``tests/test_diagnostics.py``) and guarantees the
tab text and the copied report can never diverge.

Threading rules (same contract as the main window)
--------------------------------------------------
* Collection always runs in a ``QThreadPool`` worker (``ReportWorker``); the
  UI thread only assigns the resulting strings to widgets.
* Requests are numbered: a slow report can never overwrite a newer one (the
  auto-refresh ticks every 2 s while a full refresh may take longer, e.g. a
  ``pw-dump`` timeout).
* The 2 s auto-refresh collects only the *live* sections (engine + metering)
  and merges them into the last full report (:func:`merge_report`), so it never
  re-runs ``pw-dump`` or the ``/proc`` scan.

The window is deliberately not reachable from any menu or button: it exists
only through the **Ctrl+Shift+D** shortcut installed by ``MainWindow``.
"""

import datetime
import html
import logging
import os
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, Union

from PySide6.QtCore import QObject, QRunnable, Qt, QThreadPool, QTimer, Signal, Slot
from PySide6.QtGui import QFont, QFontDatabase, QGuiApplication, QTextCursor
from PySide6.QtWidgets import (
    QApplication,
    QCheckBox,
    QDialog,
    QFileDialog,
    QHBoxLayout,
    QLabel,
    QMessageBox,
    QPlainTextEdit,
    QPushButton,
    QSizePolicy,
    QTabWidget,
    QTextBrowser,
    QVBoxLayout,
    QWidget,
)

from src.diagnostics import collector

logger = logging.getLogger("DiagnosticsDialog")

#: Auto-refresh cadence for the live part (engine + metering).
AUTO_REFRESH_INTERVAL_MS = 2000
#: Default geometry: the window is meant to be read, so it opens wide.
DEFAULT_SIZE = (1180, 760)
MINIMUM_SIZE = (720, 460)
#: Severity → colour of the summary lines (chosen to stay readable on the
#: dark theme the strip widgets use).
SEVERITY_COLORS = {
    collector.SEVERITY_ERROR: "#ff6b6b",
    collector.SEVERITY_WARNING: "#ffb86b",
    collector.SEVERITY_INFO: "#8ab4f8",
}

StripsSource = Union[None, Sequence[Any], Callable[[], Sequence[Any]]]


class _ReportSignals(QObject):
    """Signal bridge for :class:`ReportWorker` (QRunnable cannot emit).

    Both signals carry the *request id* so the receiver can drop a result that
    a newer refresh has already superseded (the 2 s auto-refresh keeps running
    while a slow full collection is in flight).
    """

    finished = Signal(int, object)
    failed = Signal(int, str)


class ReportWorker(QRunnable):
    """Collect one report on a pool thread, then emit it.

    Mirrors the main window's ``BackendWorker`` convention (a ``QRunnable``
    submitted to a ``QThreadPool``) but hands its result back by signal, since
    the dialog must redraw from the UI thread.  The dialog keeps a reference to
    the worker until the result arrives: the signal bridge must outlive the
    pool's execution of :meth:`run`, otherwise the queued emission is lost.
    """

    def __init__(self, audio_engine: Any, options: Mapping[str, Any], request_id: int = 0):
        super().__init__()
        self.audio_engine = audio_engine
        self.options = dict(options)
        self.request_id = request_id
        self.signals = _ReportSignals()

    @Slot()
    def run(self) -> None:
        try:
            report = collector.collect_report(self.audio_engine, self.options)
        except Exception as exc:  # noqa: BLE001 - a worker must never crash the app
            logger.exception("diagnostics collection failed")
            self.signals.failed.emit(self.request_id, "%s: %s" % (exc.__class__.__name__, exc))
            return
        self.signals.finished.emit(self.request_id, report)


class DiagnosticsDialog(QDialog):
    """Non-modal diagnostics window: 5 tabs + refresh / copy / save.

    Parameters
    ----------
    audio_engine
        The engine the app is using (``IpcAudioEngine`` or the classic
        ``AudioEngine``); ``None`` is accepted and yields an honest
        "engine unavailable" report.
    strips
        The UI's strip list (or a zero-argument callable returning it) — the
        source of truth for the classic engine's strip list and the reference
        for the volume/config cross-checks.  A callable is preferred so the
        report always reflects the current configuration.
    thread_pool
        Pool to run the collection in.  The main window passes its own pool
        (``MainWindow.thread_pool``) so the diagnostics share the app's
        threading budget; a dedicated pool is created otherwise.
    """

    def __init__(
        self,
        audio_engine: Any = None,
        strips: StripsSource = None,
        thread_pool: Optional[QThreadPool] = None,
        parent: Optional[QWidget] = None,
    ):
        super().__init__(parent)
        self.audio_engine = audio_engine
        self._strips_source = strips
        self._report: Optional[Dict[str, Any]] = None
        self._request_id = 0
        self._busy = False
        # In-flight workers by request id (keeps their signals alive) and the
        # kind of collection each one asked for (full vs live).
        self._workers: Dict[int, ReportWorker] = {}
        self._requests: Dict[int, bool] = {}

        self.setWindowTitle("Holaf-Mix — Diagnostics   (Ctrl+Shift+D)")
        self.setModal(False)
        self.setSizeGripEnabled(True)
        self.resize(*DEFAULT_SIZE)
        self.setMinimumSize(*MINIMUM_SIZE)
        # Take all the width the user gives it: the graph section is wide.
        self.setSizePolicy(QSizePolicy.Expanding, QSizePolicy.Expanding)

        if thread_pool is not None:
            self.thread_pool = thread_pool
        else:
            self.thread_pool = QThreadPool(self)
            self.thread_pool.setMaxThreadCount(2)

        self._mono_font = self._monospace_font()
        self._build_ui()

        self._auto_timer = QTimer(self)
        self._auto_timer.setInterval(AUTO_REFRESH_INTERVAL_MS)
        self._auto_timer.timeout.connect(self._on_auto_refresh)

    # ------------------------------------------------------------------ UI
    def _monospace_font(self) -> QFont:
        font = QFontDatabase.systemFont(QFontDatabase.FixedFont)
        font.setStyleHint(QFont.Monospace)
        font.setPointSize(max(8, font.pointSize()))
        return font

    def _build_ui(self) -> None:
        layout = QVBoxLayout(self)
        layout.setContentsMargins(8, 8, 8, 8)
        layout.setSpacing(6)

        # --- toolbar -----------------------------------------------------
        toolbar = QHBoxLayout()
        toolbar.setSpacing(8)

        self.btn_refresh = QPushButton("Refresh")
        self.btn_refresh.setToolTip("Re-collect the whole report (Ctrl+R)")
        self.btn_refresh.clicked.connect(lambda: self.refresh(live_only=False))
        toolbar.addWidget(self.btn_refresh)

        self.btn_copy = QPushButton("Copy report")
        self.btn_copy.setToolTip(
            "Copy the full text report to the clipboard — paste it in a bug report"
        )
        self.btn_copy.clicked.connect(self._copy_report)
        toolbar.addWidget(self.btn_copy)

        self.btn_save = QPushButton("Save…")
        self.btn_save.setToolTip("Save the text report to a .txt file")
        self.btn_save.clicked.connect(self._save_report)
        toolbar.addWidget(self.btn_save)

        self.chk_auto = QCheckBox("Auto-refresh every %d s" % (AUTO_REFRESH_INTERVAL_MS // 1000))
        self.chk_auto.setToolTip("Live refresh of the engine/metering part only")
        self.chk_auto.toggled.connect(self._on_auto_toggled)
        toolbar.addWidget(self.chk_auto)

        toolbar.addStretch(1)

        self.lbl_status = QLabel("collecting…")
        self.lbl_status.setStyleSheet("color: #9aa0a6;")
        toolbar.addWidget(self.lbl_status)

        layout.addLayout(toolbar)

        # --- tabs --------------------------------------------------------
        self.tabs = QTabWidget()
        self.tabs.setDocumentMode(True)

        self.view_summary = QTextBrowser()
        self.view_summary.setOpenExternalLinks(False)
        self.view_summary.setStyleSheet("QTextBrowser { background: #16181c; }")
        self.tabs.addTab(self.view_summary, "Summary")

        self.view_engine = self._make_text_view()
        self.tabs.addTab(self.view_engine, "Engine")

        self.view_graph = self._make_text_view()
        self.tabs.addTab(self.view_graph, "Graph")

        self.view_system = self._make_text_view()
        self.tabs.addTab(self.view_system, "System")

        self.view_logs = self._make_text_view()
        self.tabs.addTab(self.view_logs, "Journal")

        layout.addWidget(self.tabs, 1)

    def _make_text_view(self) -> QPlainTextEdit:
        """Read-only monospace view: selectable, no wrapping, no wasted rows."""
        view = QPlainTextEdit()
        view.setReadOnly(True)
        view.setLineWrapMode(QPlainTextEdit.NoWrap)
        view.setFont(self._mono_font)
        view.setStyleSheet("QPlainTextEdit { background: #16181c; color: #e8eaed; }")
        view.setMinimumHeight(120)
        return view

    # -------------------------------------------------------------- options
    def _current_strips(self) -> List[Any]:
        source = self._strips_source
        if callable(source):
            try:
                return list(source() or [])
            except Exception:  # noqa: BLE001 - a broken provider must not break diagnostics
                logger.debug("strip provider failed", exc_info=True)
                return []
        return list(source or [])

    def _build_options(self, live_only: bool) -> Dict[str, Any]:
        options: Dict[str, Any] = {"strips": self._current_strips()}
        if live_only:
            # Cheap sections only: the expensive ones are kept from the last
            # full report (see collector.merge_report).
            options["include_graph"] = False
            options["include_system"] = False
            options["include_logs"] = False
        return options

    # --------------------------------------------------------------- refresh
    def refresh(self, live_only: bool = False) -> None:
        """Start one collection in the background (never blocks the UI).

        The signals are connected to *bound methods of this dialog* (a
        ``QObject`` living on the UI thread) so Qt delivers them with a queued
        connection: the widget updates always happen on the UI thread, never on
        the pool thread that ran the collection.
        """
        self._request_id += 1
        request_id = self._request_id
        self.btn_refresh.setEnabled(False)
        if not live_only:
            self.lbl_status.setText("collecting…")
        worker = ReportWorker(self.audio_engine, self._build_options(live_only), request_id)
        worker.signals.finished.connect(self._on_report)
        worker.signals.failed.connect(self._on_failed)
        self._workers[request_id] = worker
        self._requests[request_id] = live_only
        self._busy = True
        self.thread_pool.start(worker)

    @Slot(int, object)
    def _on_report(self, request_id: int, report: Any) -> None:
        live_only = self._requests.pop(request_id, False)
        self._workers.pop(request_id, None)
        self._busy = bool(self._workers)
        self.btn_refresh.setEnabled(not self._busy)
        if request_id != self._request_id:
            # A newer request superseded this one: drop the stale result.
            return
        if not isinstance(report, dict):
            self._on_failed(request_id, "collection returned no report")
            return
        if live_only and self._report is not None:
            merged = collector.merge_report(self._report, report)
        else:
            merged = report
        self._report = merged
        self._render(merged)
        self.lbl_status.setText(
            "updated %s · backend %s"
            % (
                datetime.datetime.now().strftime("%H:%M:%S"),
                (merged.get("environment") or {}).get("backend") or "?",
            )
        )

    @Slot(int, str)
    def _on_failed(self, request_id: int, message: str) -> None:
        self._requests.pop(request_id, None)
        self._workers.pop(request_id, None)
        self._busy = bool(self._workers)
        self.btn_refresh.setEnabled(not self._busy)
        if request_id and request_id != self._request_id:
            return
        logger.warning("diagnostics collection failed: %s", message)
        self.lbl_status.setText("collection failed: %s" % message)

    # --------------------------------------------------------------- render
    def _render(self, report: Mapping[str, Any]) -> None:
        sections = collector.format_sections(report)
        self.view_summary.setHtml(self._render_summary_html(report))
        self.view_engine.setPlainText(
            "\n\n".join(part for part in (sections.get("engine"), sections.get("metering")) if part)
        )
        self.view_graph.setPlainText(sections.get("graph") or "")
        self.view_system.setPlainText(
            "\n\n".join(
                part for part in (sections.get("environment"), sections.get("system")) if part
            )
        )
        self.view_logs.setPlainText(sections.get("logs") or "")
        # Keep the views at the top after a refresh: the interesting part of
        # every section is its first lines.
        for view in (self.view_engine, self.view_graph, self.view_system, self.view_logs):
            view.moveCursor(QTextCursor.Start)
        self.view_summary.moveCursor(QTextCursor.Start)

    def _render_summary_html(self, report: Mapping[str, Any]) -> str:
        """Colourized findings (errors first) — the only rich-text tab."""
        findings = [item for item in report.get("findings") or [] if isinstance(item, dict)]
        summary = report.get("summary") or {}
        parts = [
            "<html><body style='font-family:system-ui,sans-serif;color:#e8eaed;'>",
            "<p style='font-size:13px;margin:0 0 6px 0;'>"
            "<b>%d error(s)</b> · <b>%d warning(s)</b> · %d info</p>"
            % (
                summary.get(collector.SEVERITY_ERROR, 0),
                summary.get(collector.SEVERITY_WARNING, 0),
                summary.get(collector.SEVERITY_INFO, 0),
            ),
        ]
        if not findings:
            parts.append("<p style='color:#81c995;'>No problem detected.</p>")
        for finding in findings:
            severity = finding.get("severity") or collector.SEVERITY_INFO
            color = SEVERITY_COLORS.get(severity, "#e8eaed")
            parts.append(
                "<p style='margin:0 0 8px 0;'>"
                "<span style='color:%s;font-weight:bold;'>[%s]</span> "
                "<span style='color:#9aa0a6;'>%s</span><br>"
                "<span style='color:#e8eaed;'>%s</span>"
                % (
                    color,
                    html.escape(collector.severity_label(severity)),
                    html.escape(str(finding.get("id") or "?")),
                    html.escape(str(finding.get("message") or "")),
                )
            )
            if finding.get("hint"):
                parts.append(
                    "<br><span style='color:%s;'>→ %s</span>"
                    % (color, html.escape(str(finding["hint"])))
                )
            parts.append("</p>")
        if report.get("generated_at"):
            parts.append(
                "<p style='color:#5f6368;font-size:11px;'>generated %s</p>"
                % html.escape(str(report["generated_at"]))
            )
        parts.append("</body></html>")
        return "".join(parts)

    # ---------------------------------------------------------------- actions
    def report_text(self) -> str:
        """The copyable text report for the current state."""
        return collector.format_report_text(self._report)

    def _copy_report(self) -> None:
        text = self.report_text()
        try:
            clipboard = QGuiApplication.clipboard() or QApplication.clipboard()
            clipboard.setText(text)
        except Exception as exc:  # noqa: BLE001 - clipboard can be unavailable headless
            logger.warning("clipboard unavailable: %s", exc)
            self.lbl_status.setText("clipboard unavailable (use Save… instead)")
            return
        self.lbl_status.setText("report copied to the clipboard (%d lines)" % text.count("\n"))

    def _save_report(self) -> None:
        default_name = "holaf-mix-diagnostics-%s.txt" % datetime.datetime.now().strftime(
            "%Y%m%d-%H%M%S"
        )
        path, _selected = QFileDialog.getSaveFileName(
            self, "Save diagnostic report", os.path.join(os.path.expanduser("~"), default_name),
            "Text files (*.txt);;All files (*)",
        )
        if not path:
            return
        try:
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(self.report_text())
        except OSError as exc:
            QMessageBox.warning(self, "Save failed", "Could not write %s:\n%s" % (path, exc))
            return
        self.lbl_status.setText("report saved to %s" % path)

    # ---------------------------------------------------------------- refresh
    def _on_auto_toggled(self, enabled: bool) -> None:
        if enabled:
            self._auto_timer.start()
            # Give the user something to watch immediately.
            self.refresh(live_only=True)
        else:
            self._auto_timer.stop()

    def _on_auto_refresh(self) -> None:
        if self._busy:
            # Never queue collections: a slow daemon would pile workers up.
            return
        self.refresh(live_only=True)

    # ---------------------------------------------------------------- window
    def showEvent(self, event) -> None:  # noqa: N802 (Qt API)
        """Collect on every show, so the window is never stale or empty."""
        super().showEvent(event)
        if not self._busy:
            self.refresh(live_only=False)

    def closeEvent(self, event) -> None:  # noqa: N802 (Qt API)
        """Stop the auto-refresh: a hidden window must not poll forever."""
        self._auto_timer.stop()
        super().closeEvent(event)

    def keyPressEvent(self, event) -> None:  # noqa: N802 (Qt API)
        """Ctrl+R / F5 refresh, Escape closes (it is non-modal)."""
        if event.key() in (Qt.Key_F5,) or (
            event.key() == Qt.Key_R and event.modifiers() & Qt.ControlModifier
        ):
            self.refresh(live_only=False)
            return
        super().keyPressEvent(event)
