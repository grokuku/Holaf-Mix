"""Tests for the hidden diagnostics window's collection layer (+ the Qt window).

The diagnostic feature is split on purpose (see
``src/diagnostics/collector.py``): the **collection** is pure Python (no Qt, no
PipeWire, no global state) so it can be asserted here with fake engines and
synthetic ``pw-dump``/``pw-top`` output, and the **window** is a thin renderer
on top of it.

What is covered:

* a healthy fake Rust engine  -> complete report, no finding;
* a degraded fake engine (pending graph, in-process FX host, respawns, legacy
  bus names) -> the matching findings, with the expected severities;
* an engine that raises on ``status()``/``get_meter_levels()`` -> the report is
  still produced, with "unavailable" reasons;
* ``format_report_text`` -> findings first, stable output, readable lines;
* a missing ``PATH`` -> every external tool degrades without raising;
* the pure parsers (``pw-dump``, ``pw-top``) and the rule engine;
* the in-memory log ring buffer;
* the Qt window itself (opened, refreshed off the UI thread, report copied) —
  skipped automatically when PySide6 is not installed.

Run with::

    python -m unittest discover -s tests -v
    python tests/test_diagnostics.py            # direct run also works
"""

import logging
import os
import re
import sys
import time
import unittest

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if REPO_ROOT not in sys.path:
    sys.path.insert(0, REPO_ROOT)

from src.diagnostics import collector  # noqa: E402
from src.diagnostics.logbuffer import (  # noqa: E402
    InMemoryLogHandler,
    get_log_lines,
    install_log_handler,
)

# Offscreen Qt: these tests must also pass in a container with no display.
os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

try:  # PySide6 is optional in this project (main.py imports Qt lazily)
    from PySide6.QtCore import QThreadPool  # noqa: E402
    from PySide6.QtWidgets import QApplication  # noqa: E402

    QT_AVAILABLE = True
except Exception:  # noqa: BLE001 - missing Qt (or a missing libEGL) is fine
    QT_AVAILABLE = False

# Fast, deterministic collection: never touch the real PipeWire graph, /proc or
# the log buffer unless a test asks for it explicitly.
OFFLINE_OPTIONS = {
    "include_graph": False,
    "include_system": False,
    "include_logs": False,
    "subprocess_timeout": 2.0,
}


# =========================================================================
# fakes
# =========================================================================
class FakeStrip(dict):
    """A strip as ``to_dict()`` would return it (the collector accepts both)."""


def _strip(uid, label, kind="input", mode="virtual", node_name=None,
           volume=0.8, mute=False, is_mono=False, device_name=None, effects=None):
    return FakeStrip(
        uid=uid,
        label=label,
        kind=kind,
        mode=mode,
        node_name=node_name,
        volume=volume,
        mute=mute,
        is_mono=is_mono,
        device_name=device_name,
        effects=effects or {},
        routes=[],
    )


HEALTHY_STATUS = {
    "running": True,
    "strips": [
        _strip("uid-desk", "Desktop", node_name="Holaf_Bus_Desktop"),
        _strip("uid-out", "Speakers", kind="output", mode="physical",
               node_name="Holaf_Bus_Speakers", volume=0.65, is_mono=True,
               device_name="alsa_output.usb-PreSonus.analog-stereo",
               effects={"eq": {"active": True, "params": {"50Hz gain (low shelving)": 3.0}}}),
    ],
    "routes": [{"source_uid": "uid-desk", "target_uid": "uid-out"}],
    "apps": [],
    "default_sink": "Holaf_Bus_Desktop",
    "holaf_nodes": ["Holaf_Bus_Desktop", "Holaf_Bus_Speakers"],
    "fx_chains": [{"uid": "uid-desk", "chain": False}, {"uid": "uid-out", "chain": True}],
    "meter_sources": {
        "uid-desk": "Holaf_Bus_Desktop.monitor",
        "uid-out": "Holaf_Bus_Speakers.monitor",
    },
    "mainloop_pumps": 42,
    "fx_host": {"mode": "external", "pid": 4242, "respawns": 0, "chains": ["uid-out"]},
    "degraded": False,
    "pending": [],
}

DEGRADED_STATUS = {
    "running": True,
    "strips": [
        # Legacy name + an effect declared but no chain: the stale-binary and
        # missing-chain signatures.
        _strip("uid-mic", "Micro", mode="physical", node_name="Holaf_Strip_uid-mic",
               device_name="alsa_input.usb-Broken.device",
               effects={"gate": {"active": True}}),
        _strip("uid-out", "Speakers", kind="output", node_name="Holaf_Bus_Speakers",
               volume=1.0),
    ],
    "routes": [],
    "apps": [],
    "default_sink": None,
    "holaf_nodes": ["Holaf_Strip_uid-mic", "Holaf_Bus_Speakers"],
    "fx_chains": [{"uid": "uid-mic", "chain": False}],
    "meter_sources": {},
    "mainloop_pumps": 0,
    "fx_host": {"mode": "inprocess", "pid": None, "respawns": 2, "chains": ["uid-mic"]},
    "degraded": True,
    "pending": [{"key": "device:alsa_input.usb-Broken.device",
                 "message": "device not enumerated yet"}],
}


class FakeRustEngine:
    """Stand-in for :class:`IpcAudioEngine` (``status()`` + metering)."""

    def __init__(self, status=None, levels=None, fail=()):
        self._status = dict(status or HEALTHY_STATUS)
        self._levels = levels if levels is not None else {
            "uid-desk": (0.31, 0.29),
            "uid-out": (0.52, 0.48),
        }
        self._fail = set(fail)

    def status(self):
        if "status" in self._fail:
            raise RuntimeError("daemon_not_running: socket refused")
        return dict(self._status)

    def get_meter_levels(self):
        if "meter" in self._fail:
            raise RuntimeError("sounddevice: no default device")
        return dict(self._levels)

    def strip_node_name(self, uid):
        for strip in self._status["strips"]:
            if strip["uid"] == uid:
                return strip["node_name"]
        return None


class FakePythonEngine:
    """Stand-in for the classic ``AudioEngine`` (no ``status()`` at all)."""

    class _Host:
        pid = 777

        @staticmethod
        def poll():
            return None

    def __init__(self):
        self.node_registry = {"uid-desk": 61}
        self.name_cache = {61: "Holaf_Strip_uid-desk"}
        self.fx_node_ids = {"uid-out": 62}
        self.fx_host_process = self._Host()

        class _Metering:
            _stream_sources = {"uid-desk": "Holaf_Strip_uid-desk.monitor"}

            @staticmethod
            def get_levels():
                return {"uid-desk": (0.1, 0.1)}

        self.metering = _Metering()

    def get_meter_levels(self):
        return self.metering.get_levels()


# =========================================================================
# collection
# =========================================================================
class TestHealthyReport(unittest.TestCase):
    """A healthy Rust engine must produce a full report and no finding."""

    def setUp(self):
        self.engine = FakeRustEngine()
        self.report = collector.collect_report(self.engine, dict(OFFLINE_OPTIONS))

    def test_all_sections_are_present(self):
        for key in ("environment", "engine", "metering", "graph", "system", "logs",
                    "findings", "summary", "generated_at", "schema"):
            self.assertIn(key, self.report)
        self.assertEqual(
            set(collector.format_sections(self.report)), set(collector.SECTION_ORDER)
        )

    def test_backend_is_detected_from_the_status_method(self):
        self.assertEqual(self.report["environment"]["backend"], "rust")

    def test_engine_state_is_normalised(self):
        engine = self.report["engine"]
        self.assertTrue(engine["available"])
        self.assertTrue(engine["running"])
        self.assertFalse(engine["degraded"])
        self.assertEqual(engine["pending"], [])
        self.assertEqual(engine["fx_host"]["mode"], "external")
        self.assertEqual(engine["fx_host"]["pid"], 4242)
        self.assertEqual(engine["mainloop_pumps"], 42)
        self.assertEqual([strip["uid"] for strip in engine["strips"]], ["uid-desk", "uid-out"])
        speakers = engine["strips"][1]
        self.assertEqual(speakers["node_name"], "Holaf_Bus_Speakers")
        self.assertEqual(speakers["effects"], ["eq"])
        self.assertEqual(speakers["device_name"], "alsa_output.usb-PreSonus.analog-stereo")

    def test_metering_is_collected(self):
        metering = self.report["metering"]
        self.assertTrue(metering["available"])
        self.assertEqual(metering["levels"]["uid-out"], [0.52, 0.48])
        self.assertEqual(
            metering["sources"]["uid-desk"], "Holaf_Bus_Desktop.monitor"
        )

    def test_no_finding_on_a_healthy_engine(self):
        self.assertEqual(self.report["findings"], [])
        self.assertEqual(self.report["summary"]["error"], 0)
        self.assertEqual(self.report["summary"]["warning"], 0)

    def test_report_text_contains_the_sections(self):
        text = collector.format_report_text(self.report)
        for header in ("FINDINGS", "ENVIRONMENT", "ENGINE", "METERING", "PIPEWIRE GRAPH",
                       "SYSTEM", "JOURNAL"):
            self.assertIn(header, text)
        self.assertIn("everything looks healthy", text.lower())


class TestDegradedReport(unittest.TestCase):
    """Every automatic check of the degraded scenario must fire."""

    def setUp(self):
        self.engine = FakeRustEngine(status=DEGRADED_STATUS, levels={"uid-out": (0.0, 0.0)})
        self.report = collector.collect_report(self.engine, dict(OFFLINE_OPTIONS))
        self.by_id = {finding["id"]: finding for finding in self.report["findings"]}

    def test_degraded_graph_is_an_error(self):
        finding = self.by_id["graph_degraded"]
        self.assertEqual(finding["severity"], collector.SEVERITY_ERROR)
        self.assertIn("device:alsa_input.usb-Broken.device", finding["message"])
        self.assertTrue(finding["hint"])

    def test_inprocess_fx_host_is_a_warning(self):
        finding = self.by_id["fx_host_inprocess"]
        self.assertEqual(finding["severity"], collector.SEVERITY_WARNING)
        self.assertIn("inprocess", finding["message"])

    def test_fx_host_respawns_are_reported(self):
        finding = self.by_id["fx_host_respawns"]
        self.assertEqual(finding["severity"], collector.SEVERITY_WARNING)
        self.assertIn("2 time(s)", finding["message"])

    def test_legacy_bus_names_point_at_a_stale_binary(self):
        finding = self.by_id["legacy_bus_names"]
        self.assertEqual(finding["severity"], collector.SEVERITY_WARNING)
        self.assertIn("Holaf_Strip_uid-mic", finding["message"])
        self.assertIn("stale", finding["hint"])

    def test_active_effect_without_chain_is_reported(self):
        finding = self.by_id["fx_chain_missing"]
        self.assertEqual(finding["severity"], collector.SEVERITY_WARNING)
        self.assertIn("gate", finding["message"])

    def test_stalled_mainloop_is_reported(self):
        self.assertEqual(self.by_id["mainloop_pumps_stalled"]["severity"],
                         collector.SEVERITY_WARNING)

    def test_no_metering_source_is_reported(self):
        self.assertEqual(self.by_id["metering_no_source"]["severity"],
                         collector.SEVERITY_WARNING)

    def test_findings_are_sorted_errors_first(self):
        severities = [finding["severity"] for finding in self.report["findings"]]
        self.assertEqual(severities[0], collector.SEVERITY_ERROR)
        ranks = [collector._SEVERITY_RANK[severity] for severity in severities]
        self.assertEqual(ranks, sorted(ranks))

    def test_summary_counts_match_the_findings(self):
        summary = self.report["summary"]
        self.assertEqual(summary["total"], len(self.report["findings"]))
        self.assertEqual(
            summary["error"] + summary["warning"] + summary["info"], summary["total"]
        )


class TestBrokenEngine(unittest.TestCase):
    """A raising engine must never break the report."""

    def setUp(self):
        self.engine = FakeRustEngine(fail=("status", "meter"))
        self.report = collector.collect_report(self.engine, dict(OFFLINE_OPTIONS))

    def test_report_is_still_produced(self):
        self.assertEqual(self.report["schema"], collector.REPORT_SCHEMA)
        self.assertFalse(self.report["engine"]["available"])
        self.assertIn("daemon_not_running", self.report["engine"]["reason"])
        self.assertFalse(self.report["metering"]["available"])
        self.assertIn("sounddevice", self.report["metering"]["reason"])

    def test_daemon_unreachable_is_an_error_with_a_hint(self):
        finding = next(
            item for item in self.report["findings"] if item["id"] == "daemon_unreachable"
        )
        self.assertEqual(finding["severity"], collector.SEVERITY_ERROR)
        self.assertIn("holafctl status", finding["hint"])

    def test_engine_without_any_method_does_not_raise(self):
        class Nothing:
            pass

        report = collector.collect_report(Nothing(), dict(OFFLINE_OPTIONS))
        self.assertFalse(report["engine"]["available"])
        self.assertIn("unavailable", collector.format_report_text(report))

    def test_no_engine_at_all_does_not_raise(self):
        report = collector.collect_report(None, dict(OFFLINE_OPTIONS))
        self.assertEqual(report["environment"]["backend"], "unknown")
        self.assertIn("no audio engine", report["engine"]["reason"])


class TestPythonEngine(unittest.TestCase):
    """The classic engine has no ``status()``: state is rebuilt from attributes."""

    def test_state_is_rebuilt_from_the_engine_and_the_ui_strips(self):
        engine = FakePythonEngine()
        strips = [
            _strip("uid-desk", "Desktop"),
            _strip("uid-out", "Speakers", kind="output", mode="physical",
                   device_name="alsa_output.builtin", effects={"compressor": {"active": True}}),
        ]
        report = collector.collect_report(
            engine, dict(OFFLINE_OPTIONS, strips=strips)
        )
        state = report["engine"]
        self.assertEqual(report["environment"]["backend"], "python")
        self.assertTrue(state["available"])
        self.assertEqual(state["source"], "Python engine attributes")
        self.assertTrue(state["running"])
        # The classic engine still names its buses after the uid.
        desktop = state["strips"][0]
        self.assertEqual(desktop["node_name"], "Holaf_Strip_uid-desk")
        # No legacy-name finding: that convention is the Python engine's own.
        self.assertNotIn(
            "legacy_bus_names", [finding["id"] for finding in report["findings"]]
        )
        # Its persistent pw-cli child is reported as an FX host.
        self.assertEqual(state["fx_host"]["mode"], "external")
        self.assertEqual(state["fx_host"]["pid"], 777)
        self.assertTrue(state["fx_host"]["alive"])
        self.assertEqual(state["meter_sources"], {"uid-desk": "Holaf_Strip_uid-desk.monitor"})

    def test_volume_drift_between_config_and_engine_is_detected(self):
        engine = FakePythonEngine()
        # The engine only knows the *graph*; the volume it applied is not in
        # the Python engine, so build the mismatch on the Rust fake instead.
        rust = FakeRustEngine()
        ui_strips = [_strip("uid-out", "Speakers", volume=0.42)]
        report = collector.collect_report(
            rust, dict(OFFLINE_OPTIONS, strips=ui_strips)
        )
        finding = next(item for item in report["findings"] if item["id"] == "volume_mismatch")
        self.assertEqual(finding["severity"], collector.SEVERITY_WARNING)
        self.assertIn("0.650", finding["message"])
        self.assertIn("0.420", finding["message"])
        # (engine kept alive for symmetry with the Python case above)
        self.assertIsNotNone(engine)


class TestOfflineEnvironment(unittest.TestCase):
    """With an empty PATH every external tool must degrade, not raise."""

    def test_missing_tools_do_not_raise(self):
        report = collector.collect_report(
            FakeRustEngine(),
            dict(OFFLINE_OPTIONS, include_graph=True, env={"PATH": ""}),
        )
        graph = report["graph"]
        self.assertFalse(graph["available"])
        self.assertIn("not found", graph["reason"])
        self.assertFalse(graph["pw_top"]["available"])
        self.assertIsNone(report["environment"]["pipewire_version"])
        self.assertIn("pipewire_version", report["environment"]["unavailable"])
        ids = [finding["id"] for finding in report["findings"]]
        self.assertIn("pipewire_graph_unavailable", ids)
        # The report is still renderable.
        self.assertIn("PIPEWIRE GRAPH", collector.format_report_text(report))

    def test_timeout_is_reported_not_raised(self):
        out, reason = collector._run(["sleep", "5"], timeout=0.05)
        self.assertIsNone(out)
        self.assertIn("timeout", reason)


# =========================================================================
# pure parsers
# =========================================================================
PW_DUMP_SAMPLE = [
    {"id": 0, "type": "PipeWire:Interface:Core",
     "info": {"props": {"core.name": "pipewire-0"}}},
    {"id": 41, "type": "PipeWire:Interface:Node",
     "info": {"state": "suspended", "n-input-ports": 0, "n-output-ports": 2,
              "props": {"node.name": "alsa_output.usb-PreSonus.analog-stereo",
                        "media.class": "Audio/Sink",
                        "node.description": "PreSonus AudioBox",
                        "node.latency": "128/48000", "node.rate": "1/48000"}}},
    {"id": 52, "type": "PipeWire:Interface:Node",
     "info": {"state": "running", "n-input-ports": 2, "n-output-ports": 2,
              "props": {"node.name": "Holaf_Bus_Mix_Out", "media.class": "Audio/Sink",
                        "node.latency": "1024/48000"}}},
    {"id": 70, "type": "PipeWire:Interface:Port",
     "info": {"props": {"port.name": "monitor_FL", "port.direction": "output"}}},
    {"id": 71, "type": "PipeWire:Interface:Port",
     "info": {"props": {"port.name": "playback_FL", "port.direction": "input"}}},
    {"id": 99, "type": "PipeWire:Interface:Link",
     "info": {"output-node-id": 52, "output-port-id": 70,
              "input-node-id": 41, "input-port-id": 71, "state": "active"}},
]

PW_TOP_SAMPLE = """\
S   ID  QUANT   RATE    WAIT    BUSY   W/Q   B/Q  ERR FORMAT           NAME
S  41   1024  48000     ---     ---   ---   ---    0    S32LE 2 48000  alsa_output.usb-PreSonus.analog-stereo
R  52    128  48000   1.2us   0.8us  0.00  0.00    3    F32LE 2 48000  Holaf_Bus_Mix_Out

S   ID  QUANT   RATE    WAIT    BUSY   W/Q   B/Q  ERR FORMAT           NAME
R  41   1024  48000   0.9us   0.5us  0.00  0.00    0    S32LE 2 48000  alsa_output.usb-PreSonus.analog-stereo
R  52    128  48000   1.1us   0.7us  0.00  0.00    3    F32LE 2 48000  Holaf_Bus_Mix_Out
"""


class TestParsers(unittest.TestCase):
    def test_pw_dump_nodes_and_links(self):
        parsed = collector.parse_pw_dump(PW_DUMP_SAMPLE)
        self.assertEqual(len(parsed["nodes"]), 2)
        self.assertEqual(parsed["link_count"], 1)
        device = parsed["nodes"][0]
        self.assertEqual(device["quantum"], 128)
        self.assertEqual(device["rate"], 48000)
        self.assertFalse(device["is_holaf"])
        bus = parsed["nodes"][1]
        self.assertTrue(bus["is_holaf"])
        self.assertEqual(bus["state"], "running")
        link = parsed["links"][0]
        self.assertEqual(link["output"], "Holaf_Bus_Mix_Out")
        self.assertEqual(link["output_port"], "monitor_FL")
        self.assertEqual(link["input"], "alsa_output.usb-PreSonus.analog-stereo")
        self.assertTrue(link["is_holaf"])

    def test_quantum_from_latency_params(self):
        """A running node publishes its quantum in ProcessLatency, not in props."""
        document = [
            {"id": 41, "type": "PipeWire:Interface:Node",
             "info": {"state": "running", "n-input-ports": 2, "n-output-ports": 2,
                      "props": {"node.name": "alsa_output.usb-PreSonus.analog-stereo",
                                "media.class": "Audio/Sink"},
                      "params": {"ProcessLatency": [{"quantum": 1024.0, "rate": 48000,
                                                     "ns": 21333333.0}]}}},
            {"id": 42, "type": "PipeWire:Interface:Node",
             "info": {"state": "running", "props": {"node.name": "Holaf_Bus_Mix_Out",
                                                   "media.class": "Audio/Sink"},
                      # PipeWire >= 0.3.7x wraps each param value in [id, value]
                      "params": {"Latency": [[0, {"direction": "Input",
                                                  "min_quantum": 128.0, "min_rate": 44100,
                                                  "max_quantum": 1024.0, "max_rate": 44100}]]}}},
        ]
        nodes = collector.parse_pw_dump(document)["nodes"]
        self.assertEqual((nodes[0]["quantum"], nodes[0]["rate"]), (1024, 48000))
        self.assertEqual((nodes[1]["quantum"], nodes[1]["rate"]), (128, 44100))

    def test_pw_dump_garbage_is_not_fatal(self):
        self.assertEqual(collector.parse_pw_dump("nope")["nodes"], [])
        self.assertEqual(collector.parse_pw_dump([{"bad": "object"}])["nodes"], [])

    def test_pw_top_parser_keeps_the_last_sample(self):
        parsed = collector.parse_pw_top(PW_TOP_SAMPLE)
        self.assertEqual(len(parsed["rows"]), 2)
        self.assertEqual(parsed["errors"], 6)
        by_name = {row["name"]: row for row in parsed["rows"]}
        self.assertEqual(by_name["Holaf_Bus_Mix_Out"]["quantum"], 128)
        self.assertEqual(by_name["Holaf_Bus_Mix_Out"]["err"], 3)
        # The second block wins (real measurements, first block is a warm-up).
        self.assertEqual(by_name["Holaf_Bus_Mix_Out"]["busy"], "0.7us")

    def test_pw_top_without_batch_mode(self):
        parsed = collector.parse_pw_top("pw-top: invalid option -- 'b'\n")
        self.assertEqual(parsed["rows"], [])
        self.assertEqual(parsed["errors"], 0)


# =========================================================================
# rule engine on a hand-built report
# =========================================================================
class TestFindingsRules(unittest.TestCase):
    """`detect_findings` is a pure function of the report: assert it directly."""

    def _report(self, graph=None, system=None, logs=None):
        return {
            "environment": {"backend": "rust"},
            "engine": {
                "available": True,
                "running": True,
                "degraded": False,
                "pending": [],
                "strips": [],
                "routes": [],
                "fx_host": {"mode": "external", "pid": 1, "respawns": 0, "chains": []},
                "fx_chains": [],
                "meter_sources": {},
                "mainloop_pumps": 10,
            },
            "metering": {"available": True, "levels": {}, "sources": {}},
            "graph": graph if graph is not None else {"available": False, "reason": "disabled"},
            "system": system if system is not None else {"available": False, "reason": "disabled"},
            "logs": logs if logs is not None else {"available": False},
        }

    def test_small_quantum_is_a_warning(self):
        report = self._report(
            graph={
                "available": True,
                "nodes": [],
                "holaf_nodes": [],
                "device_nodes": [
                    {"id": 41, "name": "alsa_output.usb", "quantum": 128, "live_quantum": 64}
                ],
                "links": [],
                "link_count": 0,
                "pw_top": {"available": False, "reason": "nope", "rows": [], "errors": 0},
            }
        )
        ids = {(item["id"], item["severity"]) for item in collector.detect_findings(report)}
        self.assertIn(("small_quantum", collector.SEVERITY_WARNING), ids)
        self.assertIn(("pw_top_unavailable", collector.SEVERITY_INFO), ids)

    def test_pwt_errors_scale_with_severity(self):
        def graph(errors):
            return {
                "available": True, "nodes": [], "holaf_nodes": [], "device_nodes": [],
                "links": [], "link_count": 0,
                "pw_top": {
                    "available": True, "reason": None, "errors": errors,
                    "rows": [{"name": "Holaf_Bus_X", "err": errors}],
                },
            }

        few = collector.detect_findings(self._report(graph=graph(3)))
        many = collector.detect_findings(self._report(graph=graph(collector.PWT_ERROR_THRESHOLD)))
        self.assertEqual(
            next(item for item in few if item["id"] == "pwt_errors")["severity"],
            collector.SEVERITY_WARNING,
        )
        self.assertEqual(
            next(item for item in many if item["id"] == "pwt_errors")["severity"],
            collector.SEVERITY_ERROR,
        )

    def test_multiple_daemons_and_orphans(self):
        system = {
            "available": True,
            "daemon_count": 2,
            "processes": [
                {"pid": 10, "ppid": 1, "role": "daemon"},
                {"pid": 11, "ppid": 10, "role": "daemon"},
            ],
        }
        ids = {item["id"] for item in collector.detect_findings(self._report(system=system))}
        self.assertIn("multiple_daemons", ids)
        self.assertIn("orphan_daemon", ids)

    def test_strip_without_any_bus_name_flags_a_stale_binary(self):
        report = self._report()
        report["engine"]["strips"] = [
            _strip("uid-a", "A", node_name=None),
            _strip("uid-b", "B", node_name=None),
        ]
        finding = next(
            item for item in collector.detect_findings(report)
            if item["id"] == "strip_bus_unnamed"
        )
        self.assertEqual(finding["severity"], collector.SEVERITY_WARNING)
        self.assertIn("2 strips", finding["message"])

    def test_pending_is_summarised_not_dumped(self):
        pending = [
            {"key": "route:%d" % index, "message": "cannot link node %d" % index}
            for index in range(5)
        ]
        summary = collector._summarise_pending(pending)
        self.assertIn("route:0", summary)
        self.assertNotIn("route:3", summary)
        self.assertIn("+2 more", summary)

    def test_log_errors_are_surfaced(self):
        logs = {"available": True, "lines": ["12:00:00 INFO main: ok",
                                            "12:00:01 ERROR AudioEngine: boom"]}
        ids = {item["id"] for item in collector.detect_findings(self._report(logs=logs))}
        self.assertIn("log_errors", ids)


# =========================================================================
# text report stability
# =========================================================================
class TestReportText(unittest.TestCase):
    def _report(self):
        engine = FakeRustEngine(status=DEGRADED_STATUS)
        return collector.collect_report(engine, dict(OFFLINE_OPTIONS))

    def test_findings_come_first(self):
        text = collector.format_report_text(self._report())
        self.assertLess(text.index("[ERROR  ]"), text.index("ENVIRONMENT"))
        self.assertLess(text.index("FINDINGS"), text.index("ENVIRONMENT"))

    def test_output_is_stable(self):
        first = collector.format_report_text(self._report())
        second = collector.format_report_text(self._report())
        # `generated_at` is the only moving part.
        normalise = lambda text: re.sub(r"generated .*", "generated <ts>", text)  # noqa: E731
        self.assertEqual(normalise(first), normalise(second))

    def test_key_lines_are_readable(self):
        """Light golden: the lines a user actually reads."""
        text = collector.format_report_text(self._report())
        self.assertIn("Holaf-Mix — diagnostic report", text)
        self.assertIn("FINDINGS — ", text)
        self.assertIn("[ERROR  ] graph_degraded", text)
        self.assertIn("→ ", text)          # hints are prefixed with an arrow
        self.assertIn("ENGINE (engine.status())", text)
        self.assertIn("degraded       : true", text)
        self.assertIn("fx host        : mode=inprocess", text)
        self.assertIn("bus=Holaf_Strip_uid-mic", text)
        self.assertIn("JOURNAL (last 0 line(s))", text)

    def test_sections_are_the_building_blocks_of_the_report(self):
        report = self._report()
        sections = collector.format_sections(report)
        text = collector.format_report_text(report)
        for name, section in sections.items():
            if not section:
                continue
            self.assertIn(section.splitlines()[0], text, name)

    def test_empty_report_is_handled(self):
        self.assertIn("no report available", collector.format_report_text(None))
        self.assertEqual(
            set(collector.format_sections(None)), set(collector.SECTION_ORDER)
        )


class TestMergeReport(unittest.TestCase):
    def test_live_update_keeps_the_expensive_sections(self):
        base = collector.collect_report(FakeRustEngine(), dict(OFFLINE_OPTIONS))
        base["graph"] = {"available": True, "nodes": [{"name": "Holaf_Bus_Desktop"}],
                         "holaf_nodes": [], "device_nodes": [], "links": [],
                         "link_count": 0, "pw_top": {}}
        base["system"] = {"available": True, "daemon_count": 1, "processes": []}

        other = FakeRustEngine(status=dict(DEGRADED_STATUS))
        live = collector.collect_report(other, dict(OFFLINE_OPTIONS))

        merged = collector.merge_report(base, live)
        self.assertEqual(merged["graph"]["nodes"][0]["name"], "Holaf_Bus_Desktop")
        self.assertEqual(merged["system"]["daemon_count"], 1)
        self.assertTrue(merged["engine"]["degraded"])
        # Findings are recomputed on the merged report (the kept graph is used).
        self.assertIn("graph_degraded", [item["id"] for item in merged["findings"]])

    def test_merge_without_base_recomputes_findings(self):
        live = collector.collect_report(FakeRustEngine(), dict(OFFLINE_OPTIONS))
        merged = collector.merge_report(None, live)
        self.assertEqual(merged["summary"]["total"], len(merged["findings"]))


# =========================================================================
# log ring buffer
# =========================================================================
class TestLogBuffer(unittest.TestCase):
    def setUp(self):
        self.logger = logging.getLogger("DiagnosticsTest")
        self.logger.setLevel(logging.INFO)
        # Other test modules call ``logging.disable(CRITICAL)`` at import time;
        # these tests need the records to actually reach the handler.
        self._disabled = logging.root.manager.disable
        logging.disable(logging.NOTSET)

    def tearDown(self):
        logging.disable(self._disabled)

    def test_ring_buffer_is_bounded_and_ordered(self):
        handler = InMemoryLogHandler(capacity=3)
        handler.setFormatter(logging.Formatter("%(message)s"))
        record_logger = logging.getLogger("DiagnosticsTestRing")
        record_logger.propagate = False  # keep unittest output clean
        record_logger.addHandler(handler)
        record_logger.setLevel(logging.INFO)
        try:
            for index in range(6):
                record_logger.info("line %d", index)
        finally:
            record_logger.removeHandler(handler)
        self.assertEqual(handler.lines(), ["line 3", "line 4", "line 5"])
        self.assertEqual(handler.lines(1), ["line 5"])

    def test_multi_line_records_stay_on_one_entry(self):
        handler = InMemoryLogHandler(capacity=5)
        handler.setFormatter(logging.Formatter("%(message)s"))
        record_logger = logging.getLogger("DiagnosticsTestMulti")
        record_logger.propagate = False  # keep unittest output clean
        record_logger.addHandler(handler)
        record_logger.setLevel(logging.INFO)
        try:
            record_logger.info("first\nsecond")
        finally:
            record_logger.removeHandler(handler)
        self.assertEqual(len(handler.lines()), 1)
        self.assertIn("first\n    second", handler.lines()[0])

    def test_install_is_idempotent_and_feeds_the_report(self):
        # Detach the other root handlers so the test does not print to stderr.
        root = logging.getLogger()
        saved = list(root.handlers)
        for saved_handler in saved:
            root.removeHandler(saved_handler)
        try:
            handler = install_log_handler()
            self.assertIs(install_log_handler(), handler)
            self.logger.info("diagnostics ring buffer works")
            lines = get_log_lines(50)
            self.assertTrue(any("diagnostics ring buffer works" in line for line in lines))
        finally:
            for saved_handler in saved:
                root.addHandler(saved_handler)

        report = collector.collect_report(
            FakeRustEngine(),
            {"include_graph": False, "include_system": False},
        )
        self.assertTrue(report["logs"]["available"])
        self.assertTrue(any("diagnostics ring buffer works" in line
                            for line in report["logs"]["lines"]))
        self.assertIn("JOURNAL (last ", collector.format_report_text(report))

    def test_get_log_lines_without_handler_is_empty(self):
        # The function must not care whether a handler was installed.
        self.assertIsInstance(get_log_lines(10), list)


# =========================================================================
# the Qt window (skipped when PySide6 is not installed)
# =========================================================================
@unittest.skipUnless(QT_AVAILABLE, "PySide6 not installed")
class TestDiagnosticsDialog(unittest.TestCase):
    """Open the window, refresh it off the UI thread, copy the report."""

    @classmethod
    def setUpClass(cls):
        cls.app = QApplication.instance() or QApplication([])

    def _dialog(self, engine=None):
        from src.ui.diagnostics_dialog import DiagnosticsDialog

        dialog = DiagnosticsDialog(
            audio_engine=engine if engine is not None else FakeRustEngine(),
            strips=lambda: [_strip("uid-desk", "Desktop")],
            thread_pool=QThreadPool(),
        )
        # Never hit the real graph/system/log in tests.
        dialog._build_options = lambda live_only: {
            "strips": dialog._current_strips(),
            "include_graph": False,
            "include_system": False,
            "include_logs": False,
        }
        return dialog

    def _wait(self, predicate, timeout=10.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.app.processEvents()
            if predicate():
                return True
            time.sleep(0.01)
        return False

    def test_window_opens_and_collects_in_the_background(self):
        dialog = self._dialog()
        dialog.show()
        self.assertTrue(
            self._wait(lambda: "ENGINE" in dialog.view_engine.toPlainText()),
            "the engine tab was not populated by the worker",
        )
        text = dialog.view_engine.toPlainText()
        self.assertIn("Holaf_Bus_Desktop", text)
        self.assertIn("METERING", text)
        self.assertIn("updated", dialog.lbl_status.text())
        dialog.close()

    def test_refresh_does_not_block_the_ui_thread(self):
        class SlowEngine(FakeRustEngine):
            def status(self):
                time.sleep(0.4)
                return super().status()

        dialog = self._dialog(SlowEngine())
        start = time.monotonic()
        dialog.refresh()
        elapsed = time.monotonic() - start
        self.assertLess(elapsed, 0.2, "refresh() blocked the UI thread")
        self.assertTrue(self._wait(lambda: "ENGINE" in dialog.view_engine.toPlainText()))
        dialog.close()

    def test_copy_report_puts_the_text_on_the_clipboard(self):
        dialog = self._dialog()
        dialog.show()
        self.assertTrue(self._wait(lambda: dialog._report is not None))
        dialog._copy_report()
        clipboard = QApplication.clipboard()
        text = clipboard.text()
        self.assertIn("Holaf-Mix — diagnostic report", text)
        self.assertIn("FINDINGS", text)
        self.assertEqual(text, dialog.report_text())
        dialog.close()

    def test_shortcut_slot_opens_the_hidden_window(self):
        """Ctrl+Shift+D wiring (step 3): the MainWindow slot creates the dialog.

        ``MainWindow`` itself is not built here: its constructor also creates a
        tray icon, a meter timer that polls PipeWire and (on close) rewrites
        ``config.json``.  A QWidget stub carrying the same attributes exercises
        exactly the code path of the shortcut handler.
        """
        from PySide6.QtGui import QKeySequence
        from PySide6.QtWidgets import QWidget

        from src.ui.diagnostics_dialog import DiagnosticsDialog
        from src.ui.main_window import DIAGNOSTICS_SHORTCUT, MainWindow

        self.assertEqual(QKeySequence(DIAGNOSTICS_SHORTCUT).toString(), "Ctrl+Shift+D")

        class _WindowStub(QWidget):
            def __init__(self):
                super().__init__()
                self.audio_engine = FakeRustEngine()
                self.strips = [_strip("uid-desk", "Desktop")]
                self.thread_pool = QThreadPool()
                self._diagnostics_dialog = None

        stub = _WindowStub()
        stub._build_options = None
        MainWindow.open_diagnostics(stub)
        dialog = stub._diagnostics_dialog
        self.assertIsInstance(dialog, DiagnosticsDialog)
        self.assertTrue(dialog.isVisible())
        # No menu entry / no button: the dialog is reachable from the shortcut
        # only, and reopening reuses the same window.
        MainWindow.open_diagnostics(stub)
        self.assertIs(stub._diagnostics_dialog, dialog)
        dialog._build_options = lambda live_only: {"include_graph": False,
                                                  "include_system": False,
                                                  "include_logs": False}
        self.assertTrue(self._wait(lambda: dialog._report is not None))
        dialog.close()
        stub.deleteLater()

    def test_auto_refresh_timer_and_close(self):
        dialog = self._dialog()
        dialog.show()
        self.assertTrue(self._wait(lambda: dialog._report is not None))
        dialog.chk_auto.setChecked(True)
        self.assertTrue(dialog._auto_timer.isActive())
        dialog.close()
        self.assertFalse(dialog._auto_timer.isActive())


if __name__ == "__main__":
    unittest.main(verbosity=2)
