"""In-memory log ring buffer (last N log lines, for the diagnostics window).

The app logs to stderr today, which is useless once it runs from a desktop
launcher: the user has no terminal to copy from.  The diagnostics window
(right click → *Debug window*, see :mod:`src.ui.diagnostics_dialog`) therefore
needs the last few hundred log lines *inside the process*.

Design constraints:

* **Bounded** — a ``deque(maxlen=N)``: a long session can never grow the
  process.  Old lines are simply dropped.
* **Cheap** — formatting happens in the handler (on whatever thread logged),
  never at read time, and reading copies the deque under a lock (the UI reads
  it while worker threads write).
* **Never breaks logging** — :meth:`emit` swallows every exception (a broken
  handler must not turn a ``logger.info`` into a crash); :meth:`handleError`
  is a no-op for the same reason.
* **No Qt, no PipeWire** — this module must stay importable in a bare
  unittest run (it is only ``logging``).

``main.py`` calls :func:`install_log_handler` once at startup; the diagnostics
collector calls :func:`get_log_lines` (never the handler directly), which
returns ``[]`` when nothing was installed — a diagnostics report must still be
produced when logging was configured elsewhere (tests, CLI tools).
"""

import logging
import threading
from collections import deque
from typing import Deque, List, Optional

# How many lines the diagnostics window can show.  200 matches what the
# "Journal" tab promises; the buffer keeps a bit more so a copy/paste of the
# whole report still has context.
DEFAULT_LOG_CAPACITY = 500

# Log lines are truncated to this length: one pathological line (a pw-dump
# dump, a stack trace argument) must not be able to fill the buffer.
MAX_LINE_CHARS = 2000

# The live handler, installed once by main.py.  Guarded by the module lock
# because install_log_handler() can be called from a test thread while the UI
# reads through get_log_lines().
_HANDLER: Optional["InMemoryLogHandler"] = None
_LOCK = threading.Lock()


class InMemoryLogHandler(logging.Handler):
    """A ``logging.Handler`` that keeps the last *capacity* formatted records.

    The record is formatted immediately (``asctime`` must be the time the line
    was logged, not the time it was read) with a compact
    ``HH:MM:SS LEVEL name: message`` layout — the same information as the
    stderr formatter, minus the date (a session rarely spans midnight and the
    report header carries the full timestamp anyway).
    """

    def __init__(self, capacity: int = DEFAULT_LOG_CAPACITY, level: int = logging.INFO):
        super().__init__(level=level)
        self.capacity = max(1, int(capacity))
        self._records: Deque[str] = deque(maxlen=self.capacity)
        self._lock = threading.Lock()

    def emit(self, record: logging.LogRecord) -> None:
        try:
            line = self.format(record)
        except Exception:  # noqa: BLE001 - logging must never raise
            return
        # A multi-line message (traceback, NDJSON payload) would break the
        # one-line-per-record layout of the Journal tab: indent continuation
        # lines instead of dropping them.
        line = line.replace("\n", "\n    ")
        if len(line) > MAX_LINE_CHARS:
            line = line[:MAX_LINE_CHARS] + "…"
        with self._lock:
            self._records.append(line)

    def lines(self, limit: Optional[int] = None) -> List[str]:
        """Return a copy of the buffered lines (oldest first)."""
        with self._lock:
            if limit is None or limit <= 0:
                return list(self._records)
            return list(self._records)[-limit:]

    def clear(self) -> None:
        with self._lock:
            self._records.clear()

    def handleError(self, record: logging.LogRecord) -> None:  # noqa: N802 (logging API)
        """Swallow handler errors: the stderr handler already reported them."""


class _CompactFormatter(logging.Formatter):
    """``HH:MM:SS LEVEL name: message`` — the log line as the user sees it."""

    def formatTime(self, record, datefmt=None):  # noqa: N802 (logging API)
        return super().formatTime(record, datefmt or "%H:%M:%S")


def install_log_handler(
    capacity: int = DEFAULT_LOG_CAPACITY, level: int = logging.INFO
) -> InMemoryLogHandler:
    """Attach (once) the in-memory handler to the root logger.

    Idempotent: a second call returns the already installed handler instead of
    duplicating every line.  Returns the handler (its :meth:`lines` is handy in
    tests).
    """
    global _HANDLER
    with _LOCK:
        if _HANDLER is not None and _HANDLER in logging.getLogger().handlers:
            return _HANDLER
        handler = InMemoryLogHandler(capacity=capacity, level=level)
        handler.setFormatter(
            _CompactFormatter("%(asctime)s %(levelname)-7s %(name)s: %(message)s")
        )
        logging.getLogger().addHandler(handler)
        _HANDLER = handler
        return handler


def current_handler() -> Optional[InMemoryLogHandler]:
    """The installed handler, or ``None`` when nothing is installed."""
    return _HANDLER


def get_log_lines(limit: Optional[int] = DEFAULT_LOG_CAPACITY) -> List[str]:
    """Last *limit* buffered log lines; ``[]`` when no handler is installed.

    Never raises: the diagnostics report is generated precisely when something
    is wrong, so it must not depend on logging being healthy.
    """
    handler = _HANDLER
    if handler is None:
        return []
    try:
        return handler.lines(limit)
    except Exception:  # noqa: BLE001 - diagnostics must never raise
        return []
