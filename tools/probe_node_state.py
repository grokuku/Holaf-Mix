#!/usr/bin/env python3
"""tools/probe_node_state.py — watch PipeWire nodes flip suspended ↔ running.

Why
---
The reported "chopping" (audio on/off, fast) on a USB headset has one visible
signature in `pw-top`: the node rows stop being scheduled — `QUANT 0`, `RATE 0`
— and come back, and its `ERR` counter climbs by one per restart.  A real report
showed, while the user was hearing the headset:

    alsa_output.…pro-output-0   ERR 1   quant=0 rate=0   (not scheduled)
    alsa_input.…pro-input-0     ERR 12  quant=1024 rate=48000

i.e. the outputs were *not running* at sampling time.  `pw-top` itself is
unusable for scripted measurement on PipeWire 0.3.65 (no `-b` batch mode, and it
draws nothing without a real terminal), so this tool samples `pw-dump` instead
and prints the same three numbers plus the transitions between them.

What it is for
--------------
1. On the machine that reproduces the bug: confirm whether the device node
   really flaps (`suspended → running → suspended …`) while audio plays, and
   correlate the flips with the audible chopping.
2. Measure the effect of `node.passive` on the FX chain's playback stream: with
   `node.passive = true` (what the daemon builds today, see
   `holaf-pw::spa_config`) a bench sub-graph fed only by passive streams is
   reported `suspended` **while its audio flows**; without it, everything reads
   `running`.  That flag is therefore the first thing to check on a device that
   chops.  (Measured headless on a null-sink: the audio itself was unaffected —
   0 holes, THD −81.7 dB — so the flag must be judged on the real USB device.)

Usage
-----
    tools/probe_node_state.py                          # every node, 10 s
    tools/probe_node_state.py --match pro-output --seconds 20
    tools/probe_node_state.py --links                  # also print each link
                                                       # feeding the matched node

Exit status: 0 = sampled at least one node, 1 = no match / no PipeWire.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time


def pw_dump() -> list:
    try:
        out = subprocess.run(["pw-dump"], capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.SubprocessError) as exc:
        print(f"probe_node_state: cannot run pw-dump: {exc}", file=sys.stderr)
        return []
    try:
        return json.loads(out.stdout)
    except json.JSONDecodeError as exc:
        print(f"probe_node_state: pw-dump produced no JSON: {exc}", file=sys.stderr)
        return []


def node_rows(document: list) -> dict:
    """`{node name: {state, quantum, rate}}` for every node in a dump.

    `node.latency` ("1024/48000") is the only place a *running* node publishes
    its quantum: on PipeWire 0.3.65 `pw-dump` does not expose `ProcessLatency`
    for the adapter nodes, and a node that is not scheduled publishes nothing —
    which is exactly why it prints as `0/0` here (and as `quant=0 rate=0` in
    `pw-top`).
    """
    rows = {}
    for obj in document:
        info = obj.get("info") or {}
        props = info.get("props") or {}
        name = props.get("node.name")
        if not name:
            continue
        quantum = rate = 0
        latency = str(props.get("node.latency") or "")
        if "/" in latency:
            head, _, tail = latency.partition("/")
            quantum = int(head) if head.isdigit() else 0
            rate = int(tail) if tail.isdigit() else 0
        rows[name] = {"state": info.get("state") or "?", "quantum": quantum, "rate": rate}
    return rows


def links_of(document: list, node_ids: set) -> list[str]:
    """Human-readable links touching the given node ids (port names included)."""
    names = {}
    ports = {}
    for obj in document:
        info = obj.get("info") or {}
        props = info.get("props") or {}
        if props.get("node.name"):
            names[obj["id"]] = props["node.name"]
        if obj.get("type") == "PipeWire:Interface:Port":
            ports[obj["id"]] = props.get("port.name")
    out = []
    for obj in document:
        if obj.get("type") != "PipeWire:Interface:Link":
            continue
        props = (obj.get("info") or {}).get("props") or {}
        try:
            out_node = int(props.get("link.output.node", 0))
            in_node = int(props.get("link.input.node", 0))
        except (TypeError, ValueError):
            continue
        if out_node not in node_ids and in_node not in node_ids:
            continue
        out_port = ports.get(int(props.get("link.output.port", 0) or 0), "?")
        in_port = ports.get(int(props.get("link.input.port", 0) or 0), "?")
        out.append(
            f"{names.get(out_node, out_node)}:{out_port} → "
            f"{names.get(in_node, in_node)}:{in_port}"
        )
    return sorted(out)


def describe(row: dict) -> str:
    return f"{row['state']:9s} q={row['quantum']:<5d} r={row['rate']}"


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--match", default="", metavar="SUBSTR",
                    help="only nodes whose name contains this substring")
    ap.add_argument("--seconds", type=float, default=10.0)
    ap.add_argument("--interval", type=float, default=0.15)
    ap.add_argument("--links", action="store_true",
                    help="print the links feeding the matched nodes")
    args = ap.parse_args()

    first = pw_dump()
    if not first:
        return 1
    rows = node_rows(first)
    matched = sorted(name for name in rows if args.match in name)
    if not matched:
        print(f"probe_node_state: no node matches '{args.match}'", file=sys.stderr)
        return 1

    print(f"watching {len(matched)} node(s) for {args.seconds:g}s "
          f"(every {args.interval * 1000:.0f} ms)\n")
    history = {name: [rows[name]] for name in matched}
    flips = {name: 0 for name in matched}
    started = time.monotonic()
    while time.monotonic() - started < args.seconds:
        time.sleep(args.interval)
        sample = node_rows(pw_dump())
        for name in matched:
            row = sample.get(name)
            if row is None:
                continue
            previous = history[name][-1]
            if row["state"] != previous["state"]:
                flips[name] += 1
                elapsed = time.monotonic() - started
                print(f"  {elapsed:6.2f}s  {name}: {previous['state']} → {row['state']}")
            history[name].append(row)

    print()
    for name in matched:
        samples = history[name]
        states = {}
        for row in samples:
            states[row["state"]] = states.get(row["state"], 0) + 1
        quanta = sorted({row["quantum"] for row in samples})
        rates = sorted({row["rate"] for row in samples})
        summary = " ".join(f"{state}×{count}" for state, count in sorted(states.items()))
        print(f"{name}")
        print(f"   samples={len(samples)} transitions={flips[name]} {summary}")
        print(f"   quantum seen={quanta} rate seen={rates}")
        if flips[name]:
            print("   ^ the node is flip-flopping: that is what the ear hears as "
                  "on/off chopping, and every resume costs one ERR")

    if args.links:
        ids = {obj["id"] for obj in first
               if ((obj.get("info") or {}).get("props") or {}).get("node.name") in matched}
        print("\nlinks:")
        for line in links_of(first, ids):
            print("  " + line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
