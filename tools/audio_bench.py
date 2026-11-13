#!/usr/bin/env python3
"""tools/audio_bench.py — A/B bench: WHO hosts the FX chain, and does it chop?

Motivation
----------
Users report a fast on/off "chopping" (hachage) of the audio on a USB headset
(PreSonus Studio 26c) when an EQ chain is active, **in Rust mode only**: the
Python engine hosts its chain in a dedicated long-lived `pw-cli` process and
never chops.  This bench isolates the one variable that differs — the *hosting
process* — while everything else (graph topology, SPA-JSON, tone, capture,
quantum) is byte-identical.

Topology (identical in every scenario)
--------------------------------------

    pw-cat (1 kHz tone) ─► input.Holaf_FX_b ─► [filter-chain] ─► output.Holaf_FX_b
                                                                    │
                                                    Holaf_Bus_Bench (null-sink)   ← or the
                                                                    │              N-channel device
                                            Holaf_Bus_Bench.monitor ─► pw-cat -r   (--device-channels)

Scenarios
---------
    ref      no chain at all (control: measures the bench itself)
    pwcli    chain hosted by a separate, long-lived `pw-cli` process
             → CASE A, the shape of the Python engine
    holafd   chain hosted by holafd, in-process
             → CASE B, the shape of the Rust engine (the reported bug)
    fxhost   chain hosted by `holafd --fx-host` (dedicated process, after fix)

Variants
--------
`--device-channels 4` feeds the chain into a **4-channel** null-sink
(`audio.channels=4`, `audio.position=[ FL FR RL RR ]`) instead of the strip's
own stereo bus, and captures that device's monitor.  It reproduces the topology
of a 4-output USB interface (`alsa_output.usb-…pro-output-0  s32le 4ch`), where
the engine links only FL/FR of a 4-port device — the setup the user reported
"chopping on the headset, not on the 2ch speakers".  Measured here: identical
holes/THD/steps with 2 and 4 channels, i.e. the channel count of the destination
is not what chops.

Metrics per run
---------------
* `dropouts %` — fraction of 1 ms frames whose RMS falls below 25 % of the
  median (the audible chopping), plus the number/length of gaps;
* `THD+N` — residual after least-squares removal of 1..5 kHz harmonics;
* `ERR` — the per-node xrun counter of `pw-top`'s **last** frame (0.3.65 has no
  batch mode: we run it under a pty via `script` and parse the final frame).

Usage
-----
    tools/audio_bench.py run --scenario holafd --quantum 64 --load
    tools/audio_bench.py matrix                 # full A/B table
    tools/audio_bench.py matrix --scenarios pwcli,holafd --quantums 64,32

Exit status: 0 = all runs completed, 1 = a run failed to produce a capture.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HEADLESS = os.path.join(ROOT, "scripts", "start-pipewire-headless.sh")
TONE = "/tmp/holaf-bench-tone.wav"
WORK = "/tmp/holaf-bench"
UID = "b"
STRIP_LABEL = "Bench"
# Node the chain feeds for a stereo run.  The default is the *legacy* name used
# by the classic Python engine; when the daemon is the one creating the bus it is
# refreshed from `status.strips[].node_name` (the bus is named after the label:
# `Holaf_Bus_Bench`, see `holaf_core::graph` and docs/rust-rewrite-status.md).
STRIP_NODE = f"Holaf_Strip_{UID}"
FX_IN = f"input.Holaf_FX_{UID}"
FX_OUT = f"output.Holaf_FX_{UID}"

# Number of channels of the node the chain feeds (see the "Variants" note).
DEVICE_CHANNELS = 2


def downstream_node() -> str:
    """Node the chain's playback stream must feed.

    With `--device-channels 2` (default) that is the strip's own stereo bus; with
    4 (or more) it is a dedicated multi-channel null-sink, so the graph looks
    like a strip driving a 4-output USB interface.
    """
    return STRIP_NODE if DEVICE_CHANNELS == 2 else f"BenchDev{DEVICE_CHANNELS}ch"


# Where the built binaries live (release first, then debug).
def bin_path(name: str) -> str:
    for cand in (
        os.path.join(ROOT, "dist", name),
        os.path.join(ROOT, "rust", "target", "release", name),
        os.path.join(ROOT, "rust", "target", "debug", name),
    ):
        if os.path.exists(cand):
            return cand
    raise SystemExit(f"audio_bench: binary '{name}' not found (build it first)")


def log(*a):
    print("[bench]", *a, flush=True)


# ---------------------------------------------------------------------------
# Environment
# ---------------------------------------------------------------------------

def bench_env() -> dict:
    e = dict(os.environ)
    runtime = e.get("HOLAF_PW_RUNTIME_DIR", "/tmp/pw-runtime-holaf")
    e["XDG_RUNTIME_DIR"] = runtime
    e["PIPEWIRE_RUNTIME_DIR"] = runtime
    e["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={runtime}/bus"
    return e


def run(cmd, timeout=30, **kw):
    return subprocess.run(cmd, env=bench_env(), capture_output=True, text=True,
                          timeout=timeout, **kw)


def make_tone():
    """10 s, 1 kHz, 0.5 FS, stereo WAV — generated once."""
    if os.path.exists(TONE):
        return
    import numpy as np
    import wave
    sr = 48000
    t = np.arange(sr * 10) / sr
    x = (0.5 * np.sin(2 * np.pi * 1000.0 * t)).astype("<f4")
    st = np.stack([x, x], axis=1).reshape(-1)
    w = wave.open(TONE, "wb")
    w.setnchannels(2)
    w.setsampwidth(2)
    w.setframerate(sr)
    w.writeframes((np.clip(st, -1, 1) * 32767).astype("<i2").tobytes())
    w.close()


def restart_pipewire():
    """Fresh daemon: the 0.3.65 registry stops publishing ports after many
    create/destroy cycles, so every measured cell starts from scratch."""
    subprocess.run([HEADLESS, "restart"], env=bench_env(),
                   capture_output=True, text=True)


def set_quantum(q: int):
    r = run(["pw-metadata", "-n", "settings", "0", "clock.force-quantum", str(q)])
    if r.returncode != 0:
        raise SystemExit(f"cannot force quantum {q}: {r.stderr.strip()}")


def start_cpu_load(n: int):
    """Spawn `n` busy loops.

    Deliberately configurable: saturating *every* core breaks the measurement
    itself (measured: with 12 `yes` on a 12-core box, a chain-less reference
    graph already loses 8.8 % of its frames), so the discriminating regime is
    a few processes, not all of them.
    """
    procs = [subprocess.Popen(["yes"], stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL) for _ in range(n)]
    log(f"CPU load: {n} x yes")
    return procs


def start_poller(sock: str, hz: float):
    """Emulate the Python UI's control plane: `holafctl status` at `hz`.

    This is the structural difference the whole task is about — in the Rust
    engine each of these orders is executed on the very thread that services
    the hosted filter-chain, whereas a dedicated `pw-cli` host has no orders at
    all.
    """
    ctl = bin_path("holafctl")
    script = (f"while true; do {ctl} --socket {sock} status >/dev/null 2>&1; "
              f"sleep {1.0 / hz:.4f}; done")
    proc = subprocess.Popen(["bash", "-c", script], env=bench_env(),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return proc


def stop_procs(procs):
    for p in procs:
        try:
            p.kill()
            p.wait(timeout=5)
        except Exception:
            pass


# ---------------------------------------------------------------------------
# Graph setup helpers
# ---------------------------------------------------------------------------

def node_ids() -> dict:
    out = run(["pw-dump"]).stdout
    try:
        doc = json.loads(out)
    except Exception:
        return {}
    ids = {}
    for o in doc:
        props = (o.get("info") or {}).get("props") or {}
        name = props.get("node.name")
        if name:
            ids[name] = o["id"]
    return ids


def wait_node(name, timeout=10.0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if name in node_ids():
            return True
        time.sleep(0.05)
    return False


def create_null_sink(name, channels: int = 2):
    channels = max(channels, 2)
    positions = " ".join(["FL", "FR", "RL", "RR", "SL", "SR", "TFL", "TFR",
                          "TC", "TBL", "TBR", "LFE"][:channels])
    spec = (
        "{{ factory.name=support.null-audio-sink "
        "node.name={name} node.description={name} media.class=Audio/Sink "
        "object.linger=true audio.channels={channels} "
        "audio.position=[ {positions} ] }}"
    ).format(name=name, channels=channels, positions=positions)
    run(["pw-cli", "create-node", "adapter", spec])
    if not wait_node(name, 8):
        raise SystemExit(f"null-sink {name} never appeared")


def link(src, dst):
    return run(["pw-link", src, dst])


def ensure_chain_downstream_links():
    """Chain playback → strip input, exactly as the Python engine does it
    explicitly (WirePlumber may or may not have used `target.object`)."""
    listed = run(["pw-link", "-l"]).stdout
    sink = downstream_node()
    made = 0
    for port in ("output_FL", "output_FR"):
        if f"{FX_OUT}:{port}" not in listed:
            target = f"{sink}:playback_{port.split('_')[1]}"
            r = link(f"{FX_OUT}:{port}", target)
            made += r.returncode == 0
    return made


def chain_config() -> str:
    """The exact module args holafd builds for the bench strip (EQ active)."""
    out = run([bin_path("holaf-fxcfg"), UID, "--label", "Bench", "--fx", "eq"])
    if out.returncode != 0:
        raise SystemExit(f"holaf-fxcfg failed: {out.stderr.strip()}")
    return out.stdout.strip()


def chain_graph_active() -> bool:
    return wait_node(FX_IN, 15) and wait_node(FX_OUT, 15)


# ---------------------------------------------------------------------------
# Scenarios — each returns a cleanup callable
# ---------------------------------------------------------------------------

def setup_ref(poll_hz: float = 0.0):
    create_null_sink(downstream_node(), DEVICE_CHANNELS)
    return lambda: None


def setup_pwcli(poll_hz: float = 0.0):
    """CASE A — host the chain in a separate, long-lived `pw-cli`, like the
    Python engine (`subprocess.Popen(['pw-cli'])` + `load-module …`)."""
    create_null_sink(downstream_node(), DEVICE_CHANNELS)
    cfg = chain_config()
    proc = subprocess.Popen(["pw-cli"], env=bench_env(), stdin=subprocess.PIPE,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                            text=True, bufsize=1)
    proc.stdin.write(f"load-module libpipewire-module-filter-chain {cfg}\n")
    proc.stdin.flush()
    if not chain_graph_active():
        proc.kill()
        raise SystemExit("pw-cli host: chain streams never appeared")
    time.sleep(0.3)
    ensure_chain_downstream_links()

    def cleanup():
        try:
            proc.stdin.close()
        except Exception:
            pass
        proc.kill()
        proc.wait(timeout=5)

    return cleanup


def refresh_strip_node(sock: str) -> None:
    """Adopt the public bus name the daemon published for the bench strip.

    The bus of a virtual strip is named after its **label** (`Holaf_Bus_Bench`),
    not after its uid, so the bench must not guess it — it asks `status`, the
    documented entry point (`strips[].node_name`).  A daemon that does not
    publish the field (older build) keeps the legacy default.
    """
    global STRIP_NODE
    r = run([bin_path("holafctl"), "--socket", sock, "status"])
    try:
        doc = json.loads(r.stdout)
    except Exception:
        return
    # `holafctl` prints the IPC envelope ({"id":…, "ok":…, "result":{…}}).
    doc = (doc or {}).get("result", doc) or {}
    for strip in doc.get("strips") or []:
        if strip.get("uid") == UID and strip.get("node_name"):
            STRIP_NODE = strip["node_name"]
            log(f"bench strip bus: {STRIP_NODE}")


def snapshot() -> dict:
    """The bench strip, as the daemon receives it.

    With `--device-channels > 2` the strip is backed by the multi-channel test
    device (`device_name`), so the daemon anchors the FX chain to it exactly as
    it would to a real 4-output interface; otherwise it is a plain virtual strip
    and the daemon creates its own stereo bus.
    """
    return {
        "strips": [{
            "uid": UID, "label": STRIP_LABEL, "kind": "output",
            "mode": "virtual" if DEVICE_CHANNELS == 2 else "physical",
            "volume": 1.0, "mute": False, "is_mono": False, "routes": [],
            "device_name": None if DEVICE_CHANNELS == 2 else downstream_node(),
            "assigned_apps": [], "is_default": False,
            "effects": {
                "eq": {"active": True, "params": {}},
                "compressor": {"active": False, "params": {}},
                "tube": {"active": False, "params": {}},
            },
        }]
    }


def start_holafd(fx_host_mode: str = "external"):
    sock = os.path.join(WORK, "holafd.sock")
    for p in (sock,):
        if os.path.exists(p):
            os.unlink(p)
    # The multi-channel variant needs its device to exist *before* the
    # snapshot: the daemon anchors the chain to it and defers otherwise.
    if DEVICE_CHANNELS != 2:
        create_null_sink(downstream_node(), DEVICE_CHANNELS)
    logf = open(os.path.join(WORK, "holafd.log"), "w")
    proc = subprocess.Popen(
        [bin_path("holafd"), "--backend", "pipewire", "--socket", sock,
         "--fx-host-mode", fx_host_mode],
        env=bench_env(), stdout=logf, stderr=subprocess.STDOUT)
    t0 = time.time()
    while time.time() - t0 < 15:
        if os.path.exists(sock):
            break
        time.sleep(0.05)
    else:
        proc.kill()
        raise SystemExit("holafd never created its socket")
    time.sleep(0.2)
    snap = os.path.join(WORK, "snapshot.json")
    with open(snap, "w") as f:
        json.dump(snapshot(), f)
    r = run([bin_path("holafctl"), "--socket", sock, "apply-snapshot",
             "--config", snap], timeout=180)
    if r.returncode != 0:
        log("apply-snapshot FAILED:", r.returncode, r.stdout[-400:], r.stderr[-400:])
    refresh_strip_node(sock)
    return proc, sock


def setup_holafd(poll_hz: float = 0.0):
    """CASE B — the chain is hosted **in-process** by holafd (the pre-fix
    behaviour users report the chopping with)."""
    proc, sock = start_holafd("inprocess")
    if not chain_graph_active():
        proc.kill()
        raise SystemExit("holafd: chain streams never appeared")
    time.sleep(0.3)
    poller = start_poller(sock, poll_hz) if poll_hz > 0 else None
    if poller:
        log(f"control-plane poller at {poll_hz:g} Hz (pid {poller.pid})")

    def cleanup():
        if poller:
            poller.kill()
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()

    return cleanup


def setup_fxhost(poll_hz: float = 0.0):
    """CASE C — holafd delegates hosting to the dedicated `--fx-host` child
    (the default after the fix, and the shape the Python engine uses)."""
    proc, sock = start_holafd("external")
    if not chain_graph_active():
        proc.kill()
        raise SystemExit("holafd --fx-host: chain streams never appeared")
    time.sleep(0.3)
    poller = start_poller(sock, poll_hz) if poll_hz > 0 else None
    if poller:
        log(f"control-plane poller at {poll_hz:g} Hz (pid {poller.pid})")

    def cleanup():
        if poller:
            poller.kill()
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()

    return cleanup


SETUPS = {
    "ref": setup_ref,
    "pwcli": setup_pwcli,
    "holafd": setup_holafd,
    "fxhost": setup_fxhost,
}


# ---------------------------------------------------------------------------
# Capture / measurement
# ---------------------------------------------------------------------------

def play_target(scenario: str) -> str:
    return downstream_node() if scenario == "ref" else FX_IN


def monitor_name() -> str:
    """The source to record: the monitor of the node the chain feeds."""
    return f"{downstream_node()}.monitor"


def read_wav_f32(path: str):
    """Minimal RIFF/WAVE reader for the float32 file `pw-cat -r` writes
    (Python's `wave` module refuses IEEE float)."""
    import numpy as np
    with open(path, "rb") as f:
        raw = f.read()
    if raw[:4] != b"RIFF" or raw[8:12] != b"WAVE":
        # Not a WAV (some pw-cat builds write raw): use it as-is.
        return np.frombuffer(raw, dtype="<f4")
    pos = 12
    fmt = None
    data = None
    while pos + 8 <= len(raw):
        cid = raw[pos:pos + 4]
        size = int.from_bytes(raw[pos + 4:pos + 8], "little")
        body = raw[pos + 8:pos + 8 + size]
        if cid == b"fmt ":
            fmt = body
        elif cid == b"data":
            data = body
        pos += 8 + size + (size & 1)
    if data is None:
        raise SystemExit(f"{path}: no data chunk")
    if fmt is not None and int.from_bytes(fmt[0:2], "little") == 3:
        return np.frombuffer(data, dtype="<f4")
    return np.frombuffer(data, dtype="<i2").astype("<f4") / 32768.0


def start_pwtop(path: str):
    """pw-top 0.3.65 has no batch mode: run it under a pty via `script`."""
    return subprocess.Popen(["script", "-q", "-c", "timeout 12 pw-top", path],
                            env=bench_env(), stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)


ANSI = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b[()][A-Z0-9]|\x1b[=>]")


def parse_pwtop(path: str) -> dict:
    """Return {node_name: {state, quant, rate, err, wait, busy}} from the LAST
    frame of a pw-top pty dump."""
    try:
        raw = open(path, errors="replace").read()
    except OSError:
        return {}
    txt = ANSI.sub("", raw)
    rows = {}
    # pw-top lines: <S> <ID> <QUANT> <RATE> <WAIT> <BUSY> <W-Q> <B-Q> <ERR> <FORMAT> <NAME>
    pat = re.compile(
        r"^\s*([CS])\s+(\d+)\s+(\d+)\s+(\d+)\s+"
        r"([0-9.]+/\s*[0-9.]+|[0-9.]+)\s+([0-9.]+/\s*[0-9.]+|[0-9.]+)\s+"
        r"(\S+)\s+(\S+)\s+(\d+)\s+(\S+)\s+(\S+)\s*$", re.MULTILINE)
    for m in pat.finditer(txt):
        state, nid, quant, rate, wait, busy, _wq, _bq, err, _fmt, name = m.groups()
        rows[name] = {
            "state": state, "id": int(nid), "quant": int(quant),
            "rate": int(rate), "wait": wait, "busy": busy, "err": int(err),
        }
    return rows


def holafd_pids(split: bool = False):
    """Every live `holafd` pid (daemon and/or `--fx-host` child) of this bench.

    Scanned from `/proc` rather than tracked per scenario so the metric stays
    true whatever the hosting mode spawns.
    """
    out = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/cmdline", "rb") as f:
                argv = f.read().split(b"\x00")
        except OSError:
            continue
        # Match on argv[0] only: a shell wrapper whose *command line* mentions
        # holafd must not be counted (nor its zeroed jiffies).
        exe = os.path.basename(argv[0].decode("utf-8", "replace")) if argv else ""
        if not exe.startswith("holafd"):
            continue
        tokens = [a.decode("utf-8", "replace") for a in argv if a]
        # Exact token match: `--fx-host-mode external` must not look like the
        # `--fx-host` child.
        is_child = "--fx-host" in tokens
        if "--backend" in tokens or is_child:
            out.append((int(entry), is_child))
    if split:
        # (daemon pids, fx-host child pids)
        return [p for p, is_child in out if not is_child], [
            p for p, is_child in out if is_child
        ]
    return [p for p, _ in out]


def cpu_ticks(pids: list[int]) -> int:
    """`utime + stime` (clock ticks) of the given processes; 0 for the dead."""
    total = 0
    for pid in pids:
        try:
            with open(f"/proc/{pid}/stat") as f:
                stat = f.read()
        except OSError:
            continue
        # Fields after the parenthesised comm: state=0, ppid=1, … utime=11, stime=12
        fields = stat.rsplit(") ", 1)[1].split()
        total += int(fields[11]) + int(fields[12])
    return total


def measure(scenario: str, seconds: float = 5.0, latency: str = "500ms"):
    """Play the tone through the chain and capture the strip monitor.

    Both ends of the measurement request a deliberately *large* buffer
    (`--latency 500ms`): with the default 100 ms a starved `pw-cat` client
    produced exactly the same holes as a starved chain host, so the bench could
    not tell "the chain missed its deadline" from "our own recorder missed its
    deadline" (measured: 12 `yes` on 12 cores → 8.8 % dead frames with **no
    chain at all**).
    """
    wav = os.path.join(WORK, f"rec_{scenario}.f32")
    top = os.path.join(WORK, f"pwtop_{scenario}.txt")
    for p in (wav, top):
        if os.path.exists(p):
            os.unlink(p)

    player = subprocess.Popen(["pw-cat", "-p", "--latency", latency,
                               "--target", play_target(scenario), TONE],
                              env=bench_env(), stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    time.sleep(1.5)  # let the graph start and settle
    if player.poll() is not None:
        raise SystemExit("pw-cat playback died immediately (graph not schedulable?)")

    topter = start_pwtop(top)
    daemon_pids, child_pids = holafd_pids(split=True)
    cpu_before = (cpu_ticks(daemon_pids), cpu_ticks(child_pids))
    rec = subprocess.Popen(["pw-cat", "-r", "--latency", latency,
                            "--target", monitor_name(), "--format", "f32",
                            "--rate", "48000", "--channels", "2", wav],
                           env=bench_env(), stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
    time.sleep(seconds)
    rec.send_signal(signal.SIGINT)
    try:
        rec.wait(timeout=10)
    except subprocess.TimeoutExpired:
        rec.kill()
    player.kill()
    try:
        player.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    try:
        topter.wait(timeout=15)
    except subprocess.TimeoutExpired:
        topter.kill()
    cpu_after = (cpu_ticks(daemon_pids), cpu_ticks(child_pids))
    time.sleep(0.1)
    return wav, top, (
        (cpu_after[0] - cpu_before[0], cpu_after[1] - cpu_before[1]),
        seconds,
    )


def _runs(mask):
    """[(start, end_exclusive)] of the True runs of a boolean array."""
    import numpy as np
    idx = np.nonzero(mask)[0]
    if idx.size == 0:
        return []
    out = []
    s = p = idx[0]
    for x in idx[1:]:
        if x == p + 1:
            p = x
            continue
        out.append((int(s), int(p + 1)))
        s = p = x
    out.append((int(s), int(p + 1)))
    return out


def analyse(wav: str, label: str, sr: int = 48000) -> dict:
    """Level / distortion / *holes* / *steps* of a stereo capture.

    The two metrics that matter for the reported bug are computed **per
    sample**, not per 1 ms frame (a 1 ms hole was invisible to the old
    frame-based detector unless it happened to land on a frame boundary):

    * `holes` — runs where the signal collapses to (nearly) zero, i.e. the
      chain host stopped feeding its playback stream.  Detected on a 0.33 ms
      smoothed |x| with a −34 dB threshold, minimum length 0.25 ms.
    * `steps` — sample-to-sample jumps larger than 1.5x the maximum slope the
      1 kHz test tone can physically have.  A discontinuity is an audible
      click, and it is what an xrun/underrun looks like at the splice point.
    """
    import numpy as np
    d = read_wav_f32(wav)
    if d.size < 2:
        return {"label": label, "error": "empty capture"}
    d = d[: d.size // 2 * 2].reshape(-1, 2)
    mono = d.mean(axis=1)
    nz = np.nonzero(np.abs(mono) > 1e-3)[0]
    if nz.size:
        a, b = nz[0], nz[-1] + 1
        mono = mono[a:b]
    if mono.size < sr // 2:
        return {"label": label, "error": "no sustained signal",
                "samples": int(mono.size)}

    rms = float(np.sqrt((mono ** 2).mean()))
    peak = float(np.abs(mono).max())

    # --- fundamental + harmonics by least squares (no window leakage)
    t = np.arange(mono.size) / sr
    cols = []
    for k in range(1, 6):
        for fn in (np.sin, np.cos):
            cols.append(fn(2 * np.pi * 1000.0 * k * t))
    A = np.stack(cols, axis=1)
    coef, *_ = np.linalg.lstsq(A, mono, rcond=None)
    fund = float(np.hypot(coef[0], coef[1]))
    resid = mono - A @ coef
    resid_rms = float(np.sqrt((resid ** 2).mean()))
    thd = 20 * np.log10(max(resid_rms, 1e-12) / max(fund / np.sqrt(2), 1e-12))

    # --- holes: smoothed |x| collapsing to (nearly) zero
    # Ignore the first/last 30 ms (stream start/stop transients).
    guard = int(0.03 * sr)
    body = mono[guard: mono.size - guard] if mono.size > 3 * guard else mono
    k = 16  # 0.33 ms box
    sm = np.convolve(np.abs(body), np.ones(k) / k, mode="same")
    ref = float(np.median(sm[sm > 1e-6])) if (sm > 1e-6).any() else 0.0
    holes = []
    if ref > 1e-4:
        for a, b in _runs(sm < ref * 0.02):
            if b - a >= int(0.25e-3 * sr):  # >= 0.25 ms
                holes.append(b - a)
    hole_ms = sum(holes) / sr * 1000.0

    # --- steps: discontinuities the test tone cannot produce
    max_slope = 2 * np.pi * 1000.0 / sr * max(peak, 1e-9)
    steps = 0
    if body.size > 2:
        diff = np.abs(np.diff(body))
        # 200 sample guard: the splice transient itself
        steps = int((diff > 1.5 * max_slope).sum())

    return {
        "label": label, "samples": int(mono.size), "rms": rms, "peak": peak,
        "fund": fund, "thd": thd,
        "holes": len(holes), "hole_ms": hole_ms,
        "max_hole_ms": (max(holes) / sr * 1000.0) if holes else 0.0,
        "steps": steps,
    }


def one_run(scenario: str, quantum: int, load: int = 0, seconds: float = 5.0,
            poll_hz: float = 0.0, latency: str = "500ms") -> dict:
    os.makedirs(WORK, exist_ok=True)
    make_tone()
    restart_pipewire()
    set_quantum(quantum)
    time.sleep(0.2)
    load_procs = start_cpu_load(load) if load else []
    try:
        cleanup = SETUPS[scenario](poll_hz)
        try:
            wav, top, cpu = measure(scenario, seconds, latency)
        finally:
            cleanup()
    finally:
        stop_procs(load_procs)

    res = analyse(wav, scenario)
    tops = parse_pwtop(top)
    holaf = {k: v for k, v in tops.items()
             if k.startswith(("Holaf_", "input.Holaf", "output.Holaf"))}
    res.update({
        "scenario": scenario, "quantum": quantum, "load": load,
        "poll_hz": poll_hz, "pwtop": holaf,
        "err_total": sum(v["err"] for v in holaf.values()),
        # CPU burned by the *hosting process tree* (daemon + FX host child)
        # while the tone plays: the machine-independent cost of co-hosting the
        # chain with IPC/reconciliation (in-process) versus delegating it.
        "daemon_cpu_pct": (cpu[0][0] / float(os.sysconf("SC_CLK_TCK"))) / cpu[1] * 100.0,
        "child_cpu_pct": (cpu[0][1] / float(os.sysconf("SC_CLK_TCK"))) / cpu[1] * 100.0,
        "host_cpu_pct": ((cpu[0][0] + cpu[0][1]) / float(os.sysconf("SC_CLK_TCK")))
        / cpu[1]
        * 100.0,
    })
    return res


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------

HEADER = (f"{'case':7s} {'quant':>5s} {'stress':>8s} {'holes':>5s} {'hole_ms':>7s} "
          f"{'maxgap':>6s} {'steps':>5s} {'THD+N':>7s} {'rms':>6s} {'ERR':>4s} "
          f"{'host%':>5s} {'daemon%':>7s}")


def stress_label(res: dict) -> str:
    bits = []
    if res.get("load"):
        bits.append(f"cpu{res['load']}")
    if res.get("poll_hz"):
        bits.append(f"poll{res['poll_hz']:g}")
    return "+".join(bits) or "idle"


def fmt(res: dict) -> str:
    if "error" in res:
        return f"{res['scenario']:7s} {res.get('quantum', 0):5d} ERROR {res['error']}"
    return (f"{res['scenario']:7s} {res['quantum']:5d} {stress_label(res):>8s} "
            f"{res['holes']:5d} {res['hole_ms']:7.2f} {res['max_hole_ms']:6.2f} "
            f"{res['steps']:5d} {res['thd']:+7.1f} {res['rms']:.4f} "
            f"{res['err_total']:4d} {res.get('host_cpu_pct', 0.0):5.1f} "
            f"{res.get('daemon_cpu_pct', 0.0):7.2f}")


def matrix(scenarios, quantums, load: int, poll_hz: float, seconds: float,
           repeats: int, out_json: str | None):
    results = []
    print(HEADER, flush=True)
    for q in quantums:
        for s in scenarios:
            for _ in range(repeats):
                try:
                    r = one_run(s, q, load, seconds, poll_hz)
                except SystemExit as e:
                    r = {"scenario": s, "quantum": q, "load": load,
                         "poll_hz": poll_hz, "error": str(e)}
                results.append(r)
                print(fmt(r), flush=True)
    if out_json:
        with open(out_json, "w") as f:
            json.dump(results, f, indent=1)
        log("wrote", out_json)
    return results


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd")
    for name in ("run", "matrix"):
        p = sub.add_parser(name)
        p.add_argument("--scenario", default="holafd",
                       help="ref|pwcli|holafd|fxhost (run only)")
        p.add_argument("--scenarios", default="ref,pwcli,holafd")
        p.add_argument("--quantum", type=int, default=64)
        p.add_argument("--quantums", default="64,48,32")
        p.add_argument("--load", type=int, default=0, metavar="N",
                       help="spawn N `yes` busy loops (0 = idle)")
        p.add_argument("--poll", type=float, default=0.0, metavar="HZ",
                       help="holafd control-plane poll rate in Hz (0 = none)")
        p.add_argument("--seconds", type=float, default=5.0)
        p.add_argument("--repeats", type=int, default=1)
        p.add_argument("--latency", default="500ms")
        p.add_argument("--device-channels", type=int, default=2, metavar="N",
                       help="feed the chain into an N-channel null-sink (4 = a "
                            "4-output USB interface) instead of the stereo bus")
        p.add_argument("--json", default=None)
    args = ap.parse_args()
    global DEVICE_CHANNELS
    DEVICE_CHANNELS = max(2, args.device_channels)
    if args.cmd == "run":
        r = one_run(args.scenario, args.quantum, args.load, args.seconds,
                    args.poll, args.latency)
        print(HEADER)
        print(fmt(r))
    else:
        qs = [int(x) for x in args.quantums.split(",") if x.strip()]
        sc = [x for x in args.scenarios.split(",") if x.strip()]
        matrix(sc, qs, args.load, args.poll, args.seconds, args.repeats,
               args.json)


if __name__ == "__main__":
    main()
