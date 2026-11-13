"""Diagnostics support for Holaf-Mix (hidden diagnostic window).

Two independent halves, on purpose:

* :mod:`src.diagnostics.collector` — **pure** collection/formatting.  No Qt,
  no global state: it takes the audio engine (either backend) plus an options
  dict and returns a plain report dict, so it is unit-testable on a machine
  without PySide6 (headless CI / container).
* :mod:`src.ui.diagnostics_dialog` — the Qt window (Ctrl+Shift+D).  It only
  *displays* what the collector produced and never touches PipeWire or the
  engine directly, so a collection bug can never freeze the UI thread.

:mod:`src.diagnostics.logbuffer` holds the in-memory log ring buffer the
"Journal" tab reads (installed by ``main.py``).
"""

from src.diagnostics.collector import (
    collect_report,
    detect_findings,
    format_report_text,
    format_sections,
)
from src.diagnostics.logbuffer import InMemoryLogHandler, get_log_lines, install_log_handler

__all__ = [
    "collect_report",
    "detect_findings",
    "format_report_text",
    "format_sections",
    "InMemoryLogHandler",
    "get_log_lines",
    "install_log_handler",
]
