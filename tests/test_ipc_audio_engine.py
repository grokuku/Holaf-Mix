"""Tests for the J4 IPC client (IpcAudioEngine) and the daemon launcher.

Run without PipeWire and without a real Rust daemon: a scripted in-process
NDJSON daemon (a threaded Unix socket) answers requests according to a preset
script, so we can assert exactly which method/params IpcAudioEngine sends and
how it parses responses.

Run with:

    python -m unittest discover -s tests -v
    # or, from the repo root:
    python tests/test_ipc_audio_engine.py
"""

import json
import logging
import os
import shutil
import socket
import sys
import tempfile
import threading
import unittest
from typing import Any, Dict, List, Optional, Tuple

# Allow running the file directly or via discovery from anywhere.
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if REPO_ROOT not in sys.path:
    sys.path.insert(0, REPO_ROOT)

from src.backend.ipc_audio_engine import (  # noqa: E402
    IpcAudioEngine,
    IpcEngineError,
    METHOD_APPLY_SNAPSHOT,
    METHOD_STOP,
    METHOD_STATUS,
    METHOD_STRIP_SET_VOLUME,
    METHOD_FX_SET_ACTIVE,
    METHOD_FX_SET_PARAMS,
    _strip_to_rust_dict,
)
from src.backend.rust_launcher import (  # noqa: E402
    daemon_socket_path,
    ensure_daemon_running,
    stop_daemon,
)
from src.models.strip_model import Strip, StripType, StripMode  # noqa: E402

logging.disable(logging.CRITICAL)  # Keep unittest output clean.


class ScriptedFakeDaemon:
    """A minimal NDJSON Unix-socket daemon driven by a script.

    Accepts connections on a temp socket; every request answered ``ok`` with a
    generic result (params echoed + a couple of method-specific fields), unless
    its method is listed in ``error_methods`` (then ``ok:false``).  Optionally
    pushes ``events`` *before* each response to exercise the client's
    event-ignoring logic.  Methods listed in ``close_once_methods`` get their
    *first* occurrence answered by dropping the connection without a response
    (simulating a transient failure so the client's retry logic can be
    exercised); later occurrences answer normally.  Records every request for
    assertions.
    """

    def __init__(
        self,
        error_methods: Tuple[str, ...] = (),
        events: Tuple[str, ...] = (),
        close_once_methods: Tuple[str, ...] = (),
    ):
        self.error_methods = set(error_methods)
        self.events = list(events)
        self.close_once_methods = set(close_once_methods)
        self._close_count: Dict[str, int] = {}
        self.requests: List[Dict[str, Any]] = []
        self._tmpdir = tempfile.mkdtemp(prefix="holaf-ipc-test-")
        self.path = os.path.join(self._tmpdir, "daemon.sock")
        self._listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._listener.bind(self.path)
        self._listener.listen(16)
        self._stop = threading.Event()
        self._handlers: List[threading.Thread] = []
        self._accept = threading.Thread(target=self._accept_loop, daemon=True)
        self._accept.start()

    # --- server behaviour -------------------------------------------------
    def _accept_loop(self):
        self._listener.settimeout(0.1)
        while not self._stop.is_set():
            try:
                conn, _ = self._listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            handler = threading.Thread(
                target=self._handle_connection, args=(conn,), daemon=True
            )
            handler.start()
            self._handlers.append(handler)

    def _handle_connection(self, conn: socket.socket):
        # Raw recv line reading with a fully blocking socket (no timeout, no
        # socket.makefile).  The handler exits when the peer closes: recv then
        # returns b"", the inner loop breaks and no complete line is left.
        with conn:
            buffer = b""
            while True:
                while b"\n" not in buffer:
                    chunk = conn.recv(4096)
                    if not chunk:
                        break
                    buffer += chunk
                if b"\n" not in buffer:
                    break  # peer closed without a complete line
                line, buffer = buffer.split(b"\n", 1)
                if not line.strip():
                    continue
                try:
                    request = json.loads(line.decode("utf-8"))
                except ValueError:
                    continue
                self.requests.append(request)
                method = request.get("method")
                # Drop the connection on the first occurrence of a
                # ``close_once_methods`` entry, without replying — the client
                # sees a truncated connection (retryable).
                if method in self.close_once_methods and not self._close_count.get(method, 0):
                    self._close_count[method] = 1
                    return  # closes ``conn`` via the ``with`` block
                out = [json.dumps({"event": e, "params": {}}) for e in self.events]
                out.append(json.dumps(self._respond(request)))
                try:
                    conn.sendall(("\n".join(out) + "\n").encode("utf-8"))
                except OSError:
                    break

    def _respond(self, request: Dict[str, Any]) -> Dict[str, Any]:
        method = request.get("method")
        request_id = request.get("id")
        if method in self.error_methods:
            return {
                "id": request_id,
                "ok": False,
                "error": {
                    "code": "test_error",
                    "message": f"intentional failure for {method}",
                },
            }
        params = request.get("params") or {}
        if method == METHOD_APPLY_SNAPSHOT:
            result = {"applied": len(params.get("strips", [])), "running": True}
        elif method == METHOD_STATUS:
            result = {"running": True}
        elif method == "device.list":
            result = {"devices": []}
        elif method == "app.list":
            result = {"apps": []}
        else:
            # Generic echo for simple mutations (like the real daemon's
            # ``{ok, uid, value, …}`` replies).
            result = {"ok": True}
            result.update(params)
        return {"id": request_id, "ok": True, "result": result}

    # --- lifecycle --------------------------------------------------------
    def close(self):
        self._stop.set()
        try:
            self._listener.close()
        except OSError:
            pass
        for t in self._handlers:
            t.join(timeout=1)
        self._accept.join(timeout=1)
        shutil.rmtree(self._tmpdir, ignore_errors=True)


def sample_strips() -> List[Strip]:
    mic = Strip(label="Micro", kind=StripType.INPUT, mode=StripMode.PHYSICAL)
    mic.volume = 0.8
    mic.is_mono = True
    mic.routes = ["out-1"]
    mic.device_name = "alsa_input.usb-mic"
    speakers = Strip(label="Speakers", kind=StripType.OUTPUT, mode=StripMode.VIRTUAL)
    speakers.volume = 0.5
    return [mic, speakers]


class StripConversionTest(unittest.TestCase):
    def test_projects_strip_onto_rust_schema(self):
        strip = sample_strips()[0]
        strip.effects["eq"]["active"] = True
        strip.effects["eq"]["params"]["100Hz gain"] = -3.0

        data = _strip_to_rust_dict(strip)

        self.assertEqual(data["uid"], strip.uid)
        self.assertEqual(data["label"], "Micro")
        self.assertEqual(data["kind"], "input")
        self.assertEqual(data["volume"], 0.8)
        self.assertEqual(data["mute"], False)
        self.assertEqual(data["is_mono"], True)
        self.assertEqual(data["routes"], ["out-1"])
        self.assertEqual(data["device_name"], "alsa_input.usb-mic")
        self.assertEqual(data["is_default"], False)

        # UI-only fields are NOT projected onto the Rust schema.
        self.assertNotIn("mode", data)
        self.assertNotIn("assigned_apps", data)
        self.assertNotIn("midi_volume", data)

        # Effects use snake_case keys with {active, params}.
        self.assertEqual(
            sorted(data["effects"].keys()),
            ["compressor", "eq", "gate", "noise_cancel", "tube"],
        )
        self.assertTrue(data["effects"]["eq"]["active"])
        self.assertEqual(data["effects"]["eq"]["params"]["100Hz gain"], -3.0)
        self.assertFalse(data["effects"]["gate"]["active"])

    def test_upgrades_legacy_boolean_effect(self):
        strip = Strip("Legacy", StripType.INPUT)
        strip.effects["gate"] = True  # old boolean format
        strip.effects["compressor"] = False
        data = _strip_to_rust_dict(strip)
        self.assertEqual(data["effects"]["gate"], {"active": True, "params": {}})
        self.assertEqual(
            data["effects"]["compressor"], {"active": False, "params": {}}
        )

    def test_output_kind_round_trips(self):
        data = _strip_to_rust_dict(sample_strips()[1])
        self.assertEqual(data["kind"], "output")


class IpcRequestTest(unittest.TestCase):
    """Wire-level behaviour: what the client sends and how it reacts."""

    def test_start_engine_sends_apply_snapshot(self):
        daemon = ScriptedFakeDaemon()
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            engine.start_engine(sample_strips())
            self.assertEqual(len(daemon.requests), 1)
            req = daemon.requests[0]
            self.assertEqual(req["method"], METHOD_APPLY_SNAPSHOT)
            self.assertEqual(req["id"], 1)
            strips = req["params"]["strips"]
            self.assertEqual(len(strips), 2)
            self.assertEqual(strips[0]["kind"], "input")
            self.assertEqual(strips[1]["kind"], "output")
            self.assertEqual(strips[0]["volume"], 0.8)
            self.assertIn("effects", strips[0])
        finally:
            daemon.close()

    def test_mutators_send_expected_methods_and_params(self):
        daemon = ScriptedFakeDaemon()
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            engine.set_volume("mic-1", 0.42)
            engine.set_mute("mic-1", True)
            engine.set_mono("mic-1", True)
            engine.update_routing("mic-1", "out-1", True)
            engine.set_system_default("mic-1")

            methods = [r["method"] for r in daemon.requests]
            self.assertEqual(
                methods,
                [
                    "strip.set_volume",
                    "strip.set_mute",
                    "strip.set_mono",
                    "routing.set",
                    "system.set_default_sink",
                ],
            )
            self.assertEqual(daemon.requests[0]["params"],
                             {"uid": "mic-1", "value": 0.42})
            self.assertEqual(daemon.requests[1]["params"],
                             {"uid": "mic-1", "muted": True})
            self.assertEqual(daemon.requests[2]["params"],
                             {"uid": "mic-1", "mono": True})
            self.assertEqual(daemon.requests[3]["params"],
                             {"source_uid": "mic-1", "target_uid": "out-1",
                              "active": True})
            self.assertEqual(daemon.requests[4]["params"], {"uid": "mic-1"})
            # ids are sequential across calls.
            self.assertEqual([r["id"] for r in daemon.requests], [1, 2, 3, 4, 5])
        finally:
            daemon.close()

    def test_update_fx_params_sends_active_then_params(self):
        daemon = ScriptedFakeDaemon()
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            strip = sample_strips()[0]
            strip.effects["eq"]["active"] = True
            strip.effects["eq"]["params"]["100Hz gain"] = -6.0
            strip.effects["gate"]["active"] = True

            self.assertTrue(engine.update_fx_params(strip))

            methods = [r["method"] for r in daemon.requests]
            # All five effects get fx.set_active; active ones also get params.
            self.assertGreaterEqual(methods.count(METHOD_FX_SET_ACTIVE), 5)
            self.assertGreaterEqual(methods.count(METHOD_FX_SET_PARAMS), 2)

            eq_active = daemon.requests[
                methods.index(METHOD_FX_SET_ACTIVE)  # first: gate (id order)
            ]
            # gate is EFFECT_ORDER[0], so the first active is "gate".
            self.assertEqual(eq_active["params"]["effect"], "gate")
            self.assertTrue(eq_active["params"]["active"])

            # Find the eq params push.
            for r in daemon.requests:
                if (
                    r["method"] == METHOD_FX_SET_PARAMS
                    and r["params"]["effect"] == "eq"
                ):
                    self.assertEqual(
                        r["params"]["params"]["100Hz gain"], -6.0
                    )
                    break
            else:
                self.fail("fx.set_params for 'eq' was never sent")
        finally:
            daemon.close()

    def test_shutdown_sends_stop(self):
        daemon = ScriptedFakeDaemon()
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            engine.shutdown()
            self.assertEqual(len(daemon.requests), 1)
            self.assertEqual(daemon.requests[0]["method"], METHOD_STOP)
        finally:
            daemon.close()

    def test_status_result_is_parsed(self):
        daemon = ScriptedFakeDaemon()
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            result = engine._request(METHOD_STATUS)
            self.assertIsInstance(result, dict)
            self.assertTrue(result.get("running"))
        finally:
            daemon.close()

    def test_result_of_apply_snapshot_reports_applied_count(self):
        daemon = ScriptedFakeDaemon()
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            result = engine._request(
                METHOD_APPLY_SNAPSHOT,
                {"strips": [_strip_to_rust_dict(s) for s in sample_strips()]},
            )
            self.assertEqual(result["applied"], 2)
        finally:
            daemon.close()


class IpcErrorTest(unittest.TestCase):
    def test_ok_false_raises_engine_error(self):
        daemon = ScriptedFakeDaemon(error_methods=(METHOD_STRIP_SET_VOLUME,))
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            with self.assertRaises(IpcEngineError) as ctx:
                engine._request(METHOD_STRIP_SET_VOLUME,
                                {"uid": "x", "value": 0.5})
            self.assertEqual(ctx.exception.code, "test_error")
            self.assertIn("strip.set_volume", ctx.exception.message)
        finally:
            daemon.close()

    def test_public_set_volume_swallows_daemon_error(self):
        daemon = ScriptedFakeDaemon(error_methods=(METHOD_STRIP_SET_VOLUME,))
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            # Like AudioEngine, lightweight mutators must not raise.
            self.assertIsNone(engine.set_volume("x", 0.5))
        finally:
            daemon.close()

    def test_daemon_error_is_not_retried_and_carries_request_id(self):
        # An ``ok:false`` reply is a definitive application error — the retry
        # machinery must not re-send it, and the error must carry the request
        # id that produced it.
        daemon = ScriptedFakeDaemon(error_methods=(METHOD_STRIP_SET_VOLUME,))
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            with self.assertRaises(IpcEngineError) as ctx:
                engine._request(METHOD_STRIP_SET_VOLUME, {"uid": "x", "value": 0.5})
            self.assertEqual(ctx.exception.code, "test_error")
            self.assertEqual(ctx.exception.request_id, 1)
            # Exactly one attempt — daemon errors are never retried.
            self.assertEqual(len(daemon.requests), 1)
        finally:
            daemon.close()

    def test_lightweight_mutator_retries_once_after_dropped_connection(self):
        # First ``strip.set_volume`` connection is dropped without a response;
        # the client must retry once (fresh request id) and succeed, for a
        # total of two attempts on the wire.
        daemon = ScriptedFakeDaemon(
            close_once_methods=(METHOD_STRIP_SET_VOLUME,)
        )
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            self.assertIsNone(engine.set_volume("mic-1", 0.5))
            methods = [r["method"] for r in daemon.requests]
            self.assertEqual(methods, [METHOD_STRIP_SET_VOLUME, METHOD_STRIP_SET_VOLUME])
            ids = [r["id"] for r in daemon.requests]
            self.assertEqual(len(set(ids)), 2)  # two distinct attempts
        finally:
            daemon.close()

    def test_apply_snapshot_never_retries(self):
        # Heavy, state-replacing orders must NOT auto-retry: a single dropped
        # connection surfaces as connection_closed with the request id set.
        daemon = ScriptedFakeDaemon(close_once_methods=(METHOD_APPLY_SNAPSHOT,))
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            with self.assertRaises(IpcEngineError) as ctx:
                engine.start_engine(sample_strips())
            self.assertEqual(ctx.exception.code, "connection_closed")
            self.assertEqual(ctx.exception.request_id, 1)
            self.assertEqual(
                [r["method"] for r in daemon.requests],
                [METHOD_APPLY_SNAPSHOT],
            )
        finally:
            daemon.close()

    def test_transport_errors_carry_request_id(self):
        missing = os.path.join(
            tempfile.gettempdir(), f"holaf-{os.getpid()}-missing2.sock"
        )
        engine = IpcAudioEngine(socket_path=missing)
        with self.assertRaises(IpcEngineError) as ctx:
            engine._request(METHOD_STATUS, retries=0)
        self.assertEqual(ctx.exception.code, "daemon_not_running")
        self.assertEqual(ctx.exception.request_id, 1)
        self.assertIn("daemon not running", ctx.exception.message)

    def test_connection_refused_gives_clear_message(self):
        missing = os.path.join(
            tempfile.gettempdir(), f"holaf-{os.getpid()}-missing.sock"
        )
        engine = IpcAudioEngine(socket_path=missing)
        with self.assertRaises(IpcEngineError) as ctx:
            engine._request(METHOD_STATUS)
        self.assertEqual(ctx.exception.code, "daemon_not_running")
        self.assertIn("daemon not running", ctx.exception.message)

    def test_mutator_is_noop_when_daemon_down(self):
        engine = IpcAudioEngine(socket_path="/tmp/holaf-does-not-exist-xyz.sock")
        self.assertIsNone(engine.set_mute("x", True))
        self.assertIsNone(engine.update_routing("a", "b", True))

    def test_reload_strip_falls_back_to_full_snapshot(self):
        # fx.set_active fails (daemon lost state) → reload_strip must resync
        # by re-applying the last snapshot and report success.
        daemon = ScriptedFakeDaemon(error_methods=(METHOD_FX_SET_ACTIVE,))
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            strips = sample_strips()
            engine.start_engine(strips)  # ok, fx not involved
            count_before = len(daemon.requests)

            self.assertTrue(engine.reload_strip(strips[0]))

            tail = daemon.requests[count_before:]
            self.assertEqual(tail[-1]["method"], METHOD_APPLY_SNAPSHOT)
            self.assertEqual(
                len(tail[-1]["params"]["strips"]), len(strips)
            )
        finally:
            daemon.close()


class IpcEventAndStubTest(unittest.TestCase):
    def test_push_events_are_ignored_between_request_and_response(self):
        daemon = ScriptedFakeDaemon(events=("state_changed",))
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            # A read method should still return its result after events.
            result = engine._request(METHOD_STATUS)
            self.assertTrue(result.get("running"))
            # And a mutator must complete normally.
            self.assertIsNone(engine.set_mute("mic-1", False))
            self.assertEqual(daemon.requests[-1]["params"]["muted"], False)
        finally:
            daemon.close()

    def test_get_meter_levels_is_a_stub(self):
        daemon = ScriptedFakeDaemon()
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            self.assertEqual(engine.get_meter_levels(), {})
            # A stub must not touch the socket at all.
            self.assertEqual(daemon.requests, [])
        finally:
            daemon.close()

    def test_device_and_app_helpers(self):
        daemon = ScriptedFakeDaemon()
        engine = IpcAudioEngine(socket_path=daemon.path)
        try:
            self.assertEqual(engine.get_audio_nodes(), {"devices": []})
            self.assertEqual(engine.get_sink_inputs(), {"apps": []})
            self.assertTrue(engine.move_sink_input(12, "Holaf_Strip_x"))
            self.assertEqual(daemon.requests[-1]["method"], "app.move")
            self.assertEqual(
                daemon.requests[-1]["params"],
                {"app_id": "12", "sink_name": "Holaf_Strip_x"},
            )
        finally:
            daemon.close()


class RustLauncherTest(unittest.TestCase):
    """Test the launcher helpers without spawning a real daemon."""

    def test_default_socket_path_prefers_env(self):
        old = os.environ.get("HOLAF_SOCKET")
        try:
            os.environ["HOLAF_SOCKET"] = "/tmp/custom-holaf.sock"
            self.assertEqual(daemon_socket_path(), "/tmp/custom-holaf.sock")
        finally:
            if old is None:
                os.environ.pop("HOLAF_SOCKET", None)
            else:
                os.environ["HOLAF_SOCKET"] = old

    def test_binary_not_found_returns_false_without_raising(self):
        import src.backend.rust_launcher as launcher

        # Force the binary lookup to fail so the launcher has nothing to start.
        original = launcher._find_holafd_binary
        launcher._find_holafd_binary = lambda: None
        daemon = ScriptedFakeDaemon()  # just to get a temp dir pattern
        try:
            path = os.path.join(tempfile.gettempdir(),
                                f"holaf-launcher-{os.getpid()}-none.sock")
            handle = ensure_daemon_running(socket_path=path)
            self.assertIsInstance(handle, launcher.DaemonHandle)
            self.assertFalse(handle.ok)
            self.assertFalse(handle.spawned_by_us)
        finally:
            launcher._find_holafd_binary = original
            daemon.close()

    def test_invalid_holafd_bin_env_falls_back_to_repo_binary(self):
        import src.backend.rust_launcher as launcher

        old_bin = os.environ.get("HOLAFD_BIN")
        try:
            os.environ["HOLAFD_BIN"] = "/nonexistent/holafd"
            # A repo binary exists in rust/target, so lookup must still succeed.
            found = launcher._find_holafd_binary()
        finally:
            if old_bin is None:
                os.environ.pop("HOLAFD_BIN", None)
            else:
                os.environ["HOLAFD_BIN"] = old_bin
        self.assertIsNotNone(found)
        self.assertTrue(os.access(found, os.X_OK))

    def test_responding_daemon_is_detected(self):
        daemon = ScriptedFakeDaemon()
        try:
            handle = ensure_daemon_running(socket_path=daemon.path)
            self.assertTrue(handle.ok)
            # The daemon was already answering (started by the test), so this
            # call must NOT claim ownership: the app must not stop it on exit.
            self.assertFalse(handle.spawned_by_us)
        finally:
            daemon.close()

    def test_concurrent_ensure_spawns_exactly_once(self):
        import src.backend.rust_launcher as launcher

        class _FakeProc:
            """Minimal stand-in for subprocess.Popen."""

            def __init__(self, pid):
                self.pid = pid
                self.returncode = None

            def poll(self):
                return self.returncode

            def terminate(self):
                self.returncode = -15

            def kill(self):
                self.returncode = -9

            def wait(self, timeout=None):
                return self.returncode

        orig_find = launcher._find_holafd_binary
        orig_popen = launcher.subprocess.Popen
        orig_respond = launcher._daemon_responding
        orig_proc = launcher._process
        orig_sock = launcher._launched_socket
        path = os.path.join(
            tempfile.gettempdir(), f"holaf-spawn-{os.getpid()}.sock"
        )
        spawned: List[List[str]] = []
        try:
            launcher._find_holafd_binary = lambda: "/usr/bin/false"

            def counting_popen(cmd, *args, **kwargs):
                spawned.append(cmd)
                return _FakeProc(pid=1000 + len(spawned))

            launcher.subprocess.Popen = counting_popen
            # A daemon "answers" as soon as the first (fake) spawn happened.
            launcher._daemon_responding = (
                lambda path_, timeout=0.5: bool(spawned)
            )

            n = 8
            barrier = threading.Barrier(n)
            results: List[Any] = []

            def worker():
                barrier.wait()
                results.append(
                    ensure_daemon_running(socket_path=path)
                )

            threads = [threading.Thread(target=worker) for _ in range(n)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=10)

            self.assertEqual(len(spawned), 1)  # exactly one double-spawn guard
            self.assertEqual(len(results), n)
            self.assertTrue(all(h.ok for h in results))  # every caller saw a live daemon
            # Every caller observed the daemon this module spawned, so each
            # owns its teardown.
            self.assertTrue(all(h.spawned_by_us for h in results))
            # The module still tracks the (fake) process it spawned.
            self.assertTrue(stop_daemon())
            self.assertFalse(stop_daemon())  # nothing left to stop
        finally:
            launcher._find_holafd_binary = orig_find
            launcher.subprocess.Popen = orig_popen
            launcher._daemon_responding = orig_respond
            launcher._process = orig_proc
            launcher._launched_socket = orig_sock
            launcher._spawning = False
            for leftover in (path, f"{path}.log"):
                try:
                    os.unlink(leftover)
                except OSError:
                    pass

    def test_dead_launched_daemon_is_cleared_for_relaunch(self):
        import src.backend.rust_launcher as launcher

        class _DeadProc:
            pid = 4242
            returncode = 1

            def poll(self):
                return 1  # not alive (equivalent to kill(pid, 0) failing)

        orig_proc = launcher._process
        try:
            launcher._process = _DeadProc()
            self.assertFalse(launcher._daemon_launched_alive())
            # The dead process is forgotten so the next ensure respawns.
            self.assertIsNone(launcher._process)
        finally:
            launcher._process = orig_proc

    def test_stop_daemon_reports_false_when_nothing_owned(self):
        import src.backend.rust_launcher as launcher

        orig_proc = launcher._process
        try:
            launcher._process = None
            self.assertFalse(stop_daemon())
        finally:
            launcher._process = orig_proc


if __name__ == "__main__":
    unittest.main(verbosity=2)
