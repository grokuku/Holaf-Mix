"""signal_shutdown — clean application shutdown on POSIX signals.

Problem
-------
The application's teardown (``main.shutdown_all``) used to hang off
``QApplication.aboutToQuit`` only.  That Qt signal fires on a *clean* Qt quit
(window closed, ``QApplication.quit()``) — it knows nothing about POSIX
signals.  When the process was killed by its environment (``SIGTERM`` from a
desktop-session logout or ``systemd --user`` stop, ``SIGHUP`` when the terminal
disappears, ``SIGINT`` from Ctrl-C), Python simply died and the Rust ``holafd``
daemon it had spawned was left orphaned in the background.

What this module does
---------------------
:class:`ShutdownController` turns ``SIGTERM``, ``SIGINT`` and ``SIGHUP`` into
the same teardown Qt performs on a clean quit, and guarantees an exit even
when that teardown cannot run or cannot finish:

1. the Python signal handler (main thread) logs
   ``received SIGTERM, shutting down``, runs ``cleanup`` — the very same
   idempotent function connected to ``aboutToQuit`` — then asks the UI event
   loop to unwind (``quit_ui``).  A second signal exits immediately (exit code
   130), exactly like ``holafd`` does;
2. a **watchdog thread** is armed before ``cleanup``: if the teardown has not
   completed within ``watchdog_timeout`` seconds (deadlock, stuck socket,
   blocking dialog), it runs ``hard_cleanup`` — which must be lock-free, see
   ``rust_launcher.kill_daemon_now`` — and ``os._exit``s.  No orphan daemon can
   survive a stuck teardown;
3. a **wakeup-fd reader** covers the case where CPython cannot run the Python
   handler at all: while the main thread is blocked inside C code (an idle Qt
   event loop, a blocking C call), CPython's C-level handler still writes the
   signal number to the fd set up by :func:`signal.set_wakeup_fd`.  The reader
   thread arms the watchdog as soon as that byte arrives, so even a completely
   blocked main thread still ends with a stopped daemon.

The kernel-side belt (``PR_SET_PDEATHSIG``, see
:func:`src.backend.rust_launcher._make_pdeathsig_preexec`) remains the last
line of defence for ``SIGKILL``, where nothing in this module can run.

Qt threading rules
------------------
Python only ever runs a signal handler in the **main thread**, which is also
the Qt GUI thread, so ``cleanup`` (which may touch ``QApplication``) runs where
it belongs.  Consequences that shape the code:

* nothing Qt-related happens in the watchdog or the wakeup reader — those
  threads only touch module state, ``subprocess`` and ``os``;
* ``cleanup`` is therefore not re-posted through ``QTimer.singleShot``: a
  deferred slot would need the (possibly stuck) event loop to come back, i.e.
  exactly the situation this module is meant to survive.

The trade-off is that a signal delivered while the main thread is blocked in C
code is handled by the watchdog instead of the graceful path — still a clean
result (daemon stopped, exit code 0), just without the UI-level niceties.
"""

import logging
import os
import signal
import threading
import time

logger = logging.getLogger("Shutdown")

# Signals that can end the application "from outside".
HANDLED_SIGNALS = ("SIGTERM", "SIGINT", "SIGHUP")

# How long a teardown may run before the watchdog forces the exit.  Well above
# the launcher's own stop sequence (3 s graceful + 2 s kill) so the graceful
# path always wins, and well below the 90 s TimeoutStopSec of systemd.
WATCHDOG_TIMEOUT_S = 8.0

# How long the UI event loop gets to unwind after the teardown before the
# process is forced out (only used when a ``quit_ui`` callable is given).
QUIT_TIMEOUT_S = 5.0

# Conventional "terminated by Ctrl-C" exit status, used for a second signal.
EXIT_CODE_SECOND_SIGNAL = 130


def signal_name(signum: int) -> str:
    """``signum`` → ``"SIGTERM"`` (never raises, even for unknown numbers)."""
    try:
        return signal.Signals(signum).name
    except (ValueError, AttributeError, TypeError):
        return f"signal {signum}"


class ShutdownController:
    """Idempotent, signal-driven shutdown of the app and of its Rust daemon.

    Args:
        cleanup: full teardown, run on the main thread from the signal handler.
            Must be idempotent — ``main.shutdown_all`` is also connected to
            ``QApplication.aboutToQuit``.
        hard_cleanup: minimal, **lock-free** teardown for the watchdog and for
            a second signal (``rust_launcher.kill_daemon_now``).  Must not
            touch Qt nor take a lock: it runs precisely when those are suspect.
        quit_ui: optional callable asking the UI event loop to unwind (usually
            ``QApplication.quit``).  When omitted the process exits directly
            once ``cleanup`` returned (headless use, tests).
        exit_code: status used for the final ``os._exit``.
        watchdog_timeout: seconds granted to ``cleanup`` before ``hard_cleanup``
            + forced exit.  ``0`` disables the watchdog (tests only).
        quit_timeout: seconds granted to the UI loop after ``cleanup`` before
            the process is forced out.  ``0`` disables it (tests only).
        signal_names: names of the signals to intercept.
        use_wakeup_fd: install the wakeup-fd reader described in the module
            docstring.
    """

    def __init__(
        self,
        cleanup,
        hard_cleanup=None,
        quit_ui=None,
        exit_code: int = 0,
        watchdog_timeout: float = WATCHDOG_TIMEOUT_S,
        quit_timeout: float = QUIT_TIMEOUT_S,
        signal_names=HANDLED_SIGNALS,
        use_wakeup_fd: bool = True,
    ):
        self._cleanup = cleanup
        self._hard_cleanup = hard_cleanup
        self._quit_ui = quit_ui
        self._exit_code = exit_code
        self._watchdog_timeout = watchdog_timeout
        self._quit_timeout = quit_timeout
        self._signal_names = tuple(signal_names)
        self._use_wakeup_fd = use_wakeup_fd

        # Guards the "a shutdown was requested" decision (first signal wins).
        self._request_lock = threading.Lock()
        self._requested = False
        self._requested_by = None  # signal number of the first request
        # Set once cleanup() returned: cancels the watchdog and tells the
        # wakeup reader to stay quiet.
        self._cleanup_done = threading.Event()
        # Only one path may perform the forced exit.
        self._exiting_lock = threading.Lock()
        # Installed handlers (signum -> previous) and wakeup-fd plumbing.
        self._previous = {}
        self._wakeup_read_fd = None
        self._wakeup_write_fd = None
        self._watchdog_armed = False

    # -- installation -------------------------------------------------------

    def install(self) -> dict:
        """Install the handlers.  Returns ``{signum: previous_handler}``.

        Must run in the main thread (a CPython requirement for
        :func:`signal.signal`); a failure is logged rather than raised so the
        app still starts, and :meth:`restore` can always be called.
        """
        self._install_wakeup_fd()
        for name in self._signal_names:
            signum = getattr(signal, name, None)
            if signum is None:  # e.g. SIGHUP on a platform without it
                logger.debug("%s is not available here; not intercepting it", name)
                continue
            try:
                self._previous[signum] = signal.getsignal(signum)
                signal.signal(signum, self._handle)
                logger.debug("%s handler installed", name)
            except (ValueError, OSError) as exc:
                logger.warning(
                    "cannot intercept %s (%s); a %s will not trigger the "
                    "application teardown",
                    name,
                    exc,
                    name,
                )
        return dict(self._previous)

    def restore(self) -> None:
        """Undo :meth:`install` (previous handlers, wakeup fd, threads)."""
        for signum, handler in self._previous.items():
            try:
                signal.signal(signum, handler)
            except (ValueError, OSError, TypeError):
                pass  # Interpreter shutting down / not the main thread.
        self._previous = {}
        if self._wakeup_write_fd is not None:
            try:
                signal.set_wakeup_fd(-1)
            except (ValueError, OSError):
                pass
            for attr in ("_wakeup_write_fd", "_wakeup_read_fd"):
                fd = getattr(self, attr)
                setattr(self, attr, None)
                if fd is not None:
                    try:
                        os.close(fd)
                    except OSError:
                        pass

    @property
    def installed_signals(self) -> tuple:
        """Signal numbers currently intercepted by this controller."""
        return tuple(sorted(self._previous))

    @property
    def shutdown_requested(self) -> bool:
        return self._requested

    # -- the signal path ----------------------------------------------------

    def _handle(self, signum, frame):  # noqa: ARG002 - CPython handler signature
        """CPython signal handler (main thread only)."""
        self.request_shutdown(signum)

    def request_shutdown(self, signum) -> None:
        """Run the teardown once; a second signal exits immediately."""
        name = signal_name(signum)
        with self._request_lock:
            first = not self._requested
            if first:
                self._requested = True
                self._requested_by = signum
        if not first:
            logger.warning("received a second %s — exiting immediately", name)
            self._force_exit(EXIT_CODE_SECOND_SIGNAL, reason=f"second {name}")
            return

        logger.info("received %s, shutting down", name)
        self._arm_watchdog()
        try:
            self._cleanup()
        except BaseException:  # noqa: BLE001 - never let the teardown abort the exit
            logger.exception("the %s teardown raised; continuing to exit", name)
        finally:
            self._cleanup_done.set()

        if self._quit_ui is None:
            # Headless / no event loop: nothing left to unwind.
            self._force_exit(self._exit_code)
            return
        try:
            self._quit_ui()
        except Exception:  # noqa: BLE001 - a broken UI must not keep us alive
            logger.exception("could not ask the UI to quit after %s", name)
        self._arm_quit_watchdog()
        # Returning lets the (main) event loop unwind; ``main()`` then exits
        # normally with ``self._exit_code``.

    # -- forced exit paths --------------------------------------------------

    def _arm_watchdog(self) -> None:
        """Start the watchdog thread (idempotent)."""
        if self._watchdog_timeout <= 0 or self._watchdog_armed:
            return
        self._watchdog_armed = True
        threading.Thread(
            target=self._watchdog, name="shutdown-watchdog", daemon=True
        ).start()

    def _watchdog(self) -> None:
        if self._cleanup_done.wait(self._watchdog_timeout):
            return  # Graceful teardown finished in time.
        logger.error(
            "%.1fs after %s the teardown has not finished — forcing the exit",
            self._watchdog_timeout,
            signal_name(self._requested_by) if self._requested_by else "the signal",
        )
        self._force_exit(self._exit_code, reason="watchdog timeout")

    def _arm_quit_watchdog(self) -> None:
        """Force the exit if the UI event loop does not unwind (best effort)."""
        if self._quit_timeout <= 0:
            return

        def _watch() -> None:
            time.sleep(self._quit_timeout)
            logger.warning(
                "the UI event loop did not unwind %.1fs after the signal — "
                "forcing the exit",
                self._quit_timeout,
            )
            self._force_exit(self._exit_code, reason="quit timeout")

        threading.Thread(
            target=_watch, name="shutdown-quit-watchdog", daemon=True
        ).start()

    def _force_exit(self, code: int, reason: str = "") -> None:
        """Run ``hard_cleanup`` then ``os._exit`` — never returns.

        ``os._exit`` (not ``sys.exit``) because this may run from a signal
        handler or a non-main thread, where Python-level unwinding would either
        be ignored or wait for threads that are themselves stuck.
        """
        if not self._exiting_lock.acquire(blocking=False):
            os._exit(code)  # Another path is already exiting.
        try:
            if self._hard_cleanup is not None:
                logger.info(
                    "exit now (%s): last-resort daemon stop",
                    reason or "no reason given",
                )
                try:
                    self._hard_cleanup()
                except BaseException:  # noqa: BLE001 - we are exiting anyway
                    logger.exception("hard cleanup failed during the forced exit")
        finally:
            try:
                logging.shutdown()  # Flush the log files before _exit.
            except Exception:  # noqa: BLE001 - never block the exit
                pass
            os._exit(code)

    # -- wakeup fd ----------------------------------------------------------

    def _install_wakeup_fd(self) -> None:
        """Learn about a signal even while the main thread sits in C code.

        CPython's C-level signal handler writes the signal number to the fd
        registered with :func:`signal.set_wakeup_fd` *at delivery time*, before
        the Python-level callable gets a chance to run.  A reader thread can
        therefore arm the watchdog while the interpreter is still blocked in,
        say, ``QApplication.exec()``.
        """
        if not self._use_wakeup_fd:
            return
        try:
            read_fd, write_fd = os.pipe()
        except OSError as exc:
            logger.debug("no wakeup-fd pipe (%s); the signal handler alone is used", exc)
            return
        try:
            # The write end is shared with C code: it must never block.
            os.set_blocking(write_fd, False)
            signal.set_wakeup_fd(write_fd, warn_on_full_buffer=False)
        except (ValueError, OSError) as exc:
            logger.debug("cannot use a wakeup fd (%s); the signal handler alone is used", exc)
            os.close(read_fd)
            os.close(write_fd)
            return
        self._wakeup_read_fd = read_fd
        self._wakeup_write_fd = write_fd
        threading.Thread(
            target=self._wakeup_reader,
            args=(read_fd,),
            name="shutdown-wakeup",
            daemon=True,
        ).start()

    def _wakeup_reader(self, read_fd: int) -> None:
        """Block on the wakeup pipe and arm the watchdog on the first signal."""
        while True:
            try:
                data = os.read(read_fd, 64)
            except OSError:
                return  # Pipe closed by restore(): stop quietly.
            if not data:
                return
            for signum in data:
                if self._cleanup_done.is_set():
                    return
                logger.info(
                    "%s delivered (seen on the wakeup fd) — arming the "
                    "shutdown watchdog",
                    signal_name(signum),
                )
                self._arm_watchdog()
                if self._watchdog_timeout <= 0:
                    return


def install(cleanup, **kwargs) -> ShutdownController:
    """Convenience helper: build a controller and install its handlers."""
    controller = ShutdownController(cleanup, **kwargs)
    controller.install()
    return controller
