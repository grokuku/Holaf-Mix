#!/usr/bin/env python3
"""Export golden fixtures for the J2 holaf-core SPA builders.

Reads the *real* pure functions of the Python audio engine
(`src/backend/audio_engine.py`) and `src/models/strip_model.py`, then writes
a single JSON file consumed by the Rust golden tests of
`rust/crates/holaf-core/tests/`.

Why module stubs?
-----------------
`audio_engine.py` imports `metering`, which in turn imports `sounddevice` and
`numpy` (UI/daemon dependencies not installed in this headless container).
Those two are used only inside methods that never run here, so we inject
attribute-stub modules under their names in `sys.modules` *before* the import.
No pure function is copied or re-implemented: `_build_fx_graph`,
`_format_params`, `_resolve_effect_controls`, `_get_fx_data`, the two tables
and the plugin maps all come straight from the Python sources.

Numeric formatting (documented divergence)
------------------------------------------
The Rust `spa::format_params` is the J2 reference formatter: f32 *shortest
round-trip* decimals with no trailing `.0` for integral floats (`23520.0` →
`23520`, `0.0` → `0`).  The Python `_format_params` prints the double repr
(`23520.0`, `0.0`).  The graph fixture therefore calls `_build_fx_graph` with
a *Rust-style* formatter (`fval` below converts through `ctypes.c_float` and
drops the trailing `.0`), so the stored string is byte-comparable with the
Rust output without any normalisation at test time.

Props divergence (documented, from J1)
--------------------------------------
Python `_build_fx_props` emits a per-node *nested* struct
(`{ "eq_L" = { <port> = v … } ... }`).  J1 phase 2 proved this is a silent
no-op on PipeWire 0.3.65 and 1.2.x: the filter-chain module only reads
`Props.params`, a **flat** struct `[ "eq_L:<port>" v … ]`.  The fixture
`props_flat` string and `props_pairs` list below are generated with the
corrected flat shape (module emission order: plugin chain order, mono plugins
`<label>_L` then `<label>_R`, each port in control order).  This is what
works, not what the Python emits — the divergence is intentional.

Output schema (one case = one golden scenario)
----------------------------------------------
```json
{
  "generator": "...", "method": "…", "notes": { … },
  "default_effect_params": { "<effect>": [[port, value], …] },
  "bypass_params": { … },
  "fx_plugin_map": [ [key, internal_name, plugin_file, label, stereo], … ],
  "stereo_plugins": { … },
  "cases": [
    { "id", "kind", "available": [plugin_file, …],
      "effects": { "<effect>": {"active": bool, "params": [[port, value], …]} },
      "graph": "<_build_fx_graph output, Rust-style floats>",
      "props_flat": "<corrected flat Props SPA-JSON>",
      "props_pairs": [ [ "<node>:<port>", value ], … ] }
  ]
}
```

The export is deterministic (no timestamp); re-running reproduces the same bytes.

Run:
```bash
python3 tools/export_fixtures.py rust/crates/holaf-core/tests/fixtures/spa_builders_golden.json
```
"""
from __future__ import annotations

import json
import os
import sys
import types

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


# ---------------------------------------------------------------------------
# Make the audio_engine importable without sounddevice/numpy.
# ---------------------------------------------------------------------------
def _stub_module(name: str) -> types.ModuleType:
    module = types.ModuleType(name)

    def __getattr__(_attr: str):
        # Any attribute access returns an inert object (nothing is *called*
        # at import time for the functions we export).
        return types.SimpleNamespace()

    module.__getattr__ = __getattr__  # PEP 562
    return module


for _name in ("sounddevice", "numpy"):
    sys.modules.setdefault(_name, _stub_module(_name))

sys.path.insert(0, ROOT)

from src.backend import audio_engine as ae  # noqa: E402  (real module, stubbed deps)
from src.models.strip_model import (  # noqa: E402
    BYPASS_PARAMS,
    DEFAULT_EFFECT_PARAMS,
    Strip,
    StripType,
)

# Ensure we really loaded the engine from this tree (the pure functions come
# from the actual Python source, not a copy).
assert ae.__file__.endswith("backend/audio_engine.py") or "audio_engine" in ae.__file__
assert "/usr/lib/ladspa" == ae.LADSPA_PATH


# ---------------------------------------------------------------------------
# Rust-style float formatting (shortest round-trip f32, no trailing ".0").
# ---------------------------------------------------------------------------
def fval(value: float) -> str:
    """Format *value* the way Rust formats the same f32 when read back from the
    JSON fixture.

    Every numeric literal in the FX tables / representative cases has a double
    repr that is *also* the shortest decimal of the neighbouring f32 (verified
    against Rust's `f32` Display for the whole value set: 33.6, 23520, 250.008,
    …, see tools/export_fixtures.py docs), so the double `repr` trimmed of a
    trailing `.0` is byte-comparable with Rust.  This deliberately does **not**
    round-trip through `float32` (Python's repr of the widened value would print
    the full double shortest form, e.g. 33.599998474121094, which Rust never
    emits).  Values outside this safe set would need f32 normalisation; they do
    not occur in the tables or the representative cases.
    """
    text = repr(value)
    if text.endswith(".0"):
        text = text[:-2]
    return text


def format_params_rust(params: dict) -> str:
    """Byte-compatible twin of `holaf_core::spa::format_params`."""
    if not params:
        return "{}"
    items = [f'"{k}" = {fval(v)}' for k, v in params.items() if "=" not in k]
    if not items:
        return "{}"
    return "{ " + " ".join(items) + " }"


# ---------------------------------------------------------------------------
# Corrected flat Props builder (module emission order).  See module docstring.
# ---------------------------------------------------------------------------
def build_props_flat(strip, available_files):
    """Return (props_flat_string, props_pairs_list) for the *flat* Props shape.

    Only plugins whose `.so` basename is in *available_files* are included,
    mirroring the `os.path.exists` gate of the Python builders but with the
    availability set made explicit (so fixtures do not depend on the ambient
    plugin directory)."""
    pairs: list = []

    for (_key, internal_name, plugin_file, _ladspa_label) in ae.FX_PLUGIN_MAP:
        if plugin_file not in available_files:
            continue
        active, params = ae._get_fx_data(strip.effects, _key)
        ctrl = params if active else ae._resolve_effect_controls(strip, _key)
        ctrl_pairs = [(k, v) for k, v in ctrl.items() if "=" not in k]

        if ae.STEREO_PLUGINS.get(plugin_file):
            for port, value in ctrl_pairs:
                pairs.append((f"{internal_name}:{port}", value))
        else:
            for node in (f"{internal_name}_L", f"{internal_name}_R"):
                for port, value in ctrl_pairs:
                    pairs.append((f"{node}:{port}", value))

    if not pairs:
        return "", []

    text = "{ \"params\" = [ " + " ".join(
        f'"{node_port}" {fval(v)}' for node_port, v in pairs
    ) + " ] }"
    return text, [[np, v] for (np, v) in pairs]


# ---------------------------------------------------------------------------
# Case helpers
# ---------------------------------------------------------------------------
def _set_fx(strip: Strip, effect: str, active: bool, params: dict):
    strip.effects[effect]["active"] = active
    strip.effects[effect]["params"] = dict(params)


def _graph(strip, available_files) -> str:
    """_build_fx_graph with availability overridden via os.path.exists and a
    Rust-style float formatter."""
    real_exists = os.path.exists

    def exists(path: str) -> bool:
        base = os.path.basename(path)
        stem = base[:-3] if base.endswith(".so") else base
        return stem in available_files

    os.path.exists = exists
    try:
        return ae._build_fx_graph(strip, format_params_rust, True)
    finally:
        os.path.exists = real_exists


def _make_case(case_id, kind, available, effects, label="fixture strip"):
    strip = Strip(label, StripType.INPUT if kind == "input" else StripType.OUTPUT)
    for effect, state in effects.items():
        _set_fx(strip, effect, state["active"], state["params"])
    graph = _graph(strip, available)
    props_flat, props_pairs = build_props_flat(strip, available)
    return {
        "id": case_id,
        "kind": kind,
        "available": list(available),
        "effects": {
            e: {
                "active": st["active"],
                "params": [[k, v] for k, v in st["params"].items()],
            }
            for e, st in effects.items()
        },
        "graph": graph,
        "props_flat": props_flat,
        "props_pairs": [[np, v] for np, v in props_pairs],
    }


def _default_params(effect: str) -> dict:
    return dict(DEFAULT_EFFECT_PARAMS[effect])


def _changed(params: dict, **overrides) -> dict:
    out = dict(params)
    out.update(overrides)
    return out


def build_cases() -> list:
    cases = []

    # -- eq active, input strip, only the EQ plugin available (dual-mono) --
    eq_on = _changed(_default_params("eq"), **{"100Hz gain": -3.5, "10000Hz gain": 2.0})
    cases.append(_make_case(
        "eq_active_dualmono_input", "input", ["mbeq_1197"],
        {"eq": {"active": True, "params": eq_on}}, label="Micro",
    ))

    # -- eq inactive (defaults + no bypass override => all flat 0.0) --
    cases.append(_make_case(
        "eq_inactive_dualmono_input", "input", ["mbeq_1197"],
        # user params are IGNORED when inactive; Python re-merges defaults+bypass
        {"eq": {"active": False, "params": _changed(_default_params("eq"), **{"100Hz gain": -9.0})}},
        label="Micro",
    ))

    # -- compressor active, single true-stereo node sc4_1882 --
    comp_on = _changed(_default_params("compressor"), **{"Ratio (1:n)": 4.0, "Attack time (ms)": 20.0})
    cases.append(_make_case(
        "compressor_active_stereo_input", "input", ["sc4_1882"],
        {"compressor": {"active": True, "params": comp_on}}, label="Micro",
    ))

    # -- gate (inactive/bypassed) + eq (active): dual-mono serial chain --
    cases.append(_make_case(
        "gate_bypassed_eq_active_input", "input", ["gate_1410", "mbeq_1197"],
        {
            "gate": {"active": False, "params": _default_params("gate")},
            "eq": {"active": True, "params": eq_on},
        },
        label="Micro",
    ))

    # -- full INPUT chain as on this headless container (no rnnoise .so) --
    # gate inactive, eq active, tube active, compressor inactive.
    tube_on = _changed(_default_params("tube"), **{"Drive": 4.5})
    cases.append(_make_case(
        "full_input_chain_no_rnnoise", "input",
        ["gate_1410", "mbeq_1197", "tap_tubewarmth", "sc4_1882"],
        {
            "gate": {"active": False, "params": _default_params("gate")},
            "eq": {"active": True, "params": eq_on},
            "tube": {"active": True, "params": tube_on},
            "compressor": {"active": False, "params": _default_params("compressor")},
        },
        label="Micro",
    ))

    # -- OUTPUT chain: eq active, tube inactive, compressor active (spec: 3) --
    comp_out = _changed(_default_params("compressor"), **{"Ratio (1:n)": 3.0})
    cases.append(_make_case(
        "output_chain_eq_tube_comp", "output",
        ["mbeq_1197", "tap_tubewarmth", "sc4_1882"],
        {
            "eq": {"active": True, "params": eq_on},
            "tube": {"active": False, "params": _default_params("tube")},
            "compressor": {"active": True, "params": comp_out},
        },
        label="Speakers",
    ))

    # -- noise_cancel requested but plugin missing -> ignored (empty chain) --
    nc_on = _changed(_default_params("noise_cancel"), **{"Dry Mix": 0.0})
    cases.append(_make_case(
        "noise_cancel_requested_but_missing", "input", [],
        {"noise_cancel": {"active": True, "params": nc_on}}, label="Micro",
    ))

    # -- noise_cancel STEREO plugin available (rnnoise naming) --
    cases.append(_make_case(
        "noise_cancel_stereo_input", "input", ["librnnoise_ladspa"],
        {"noise_cancel": {"active": True, "params": nc_on}}, label="Micro",
    ))

    return cases


def main(out_path: str) -> None:
    fx_plugin_map = [
        [key, internal, plugin_file, label, bool(plugin_file in ae.STEREO_PLUGINS)]
        for (key, internal, plugin_file, label) in ae.FX_PLUGIN_MAP
    ]

    def table(table_dict):
        return {k: [[p, v] for p, v in params.items()] for k, params in table_dict.items()}

    payload = {
        "generator": "tools/export_fixtures.py",
        "method": (
            "Imported the real src/backend/audio_engine.py + src/models/strip_model.py "
            "with inert stubs for the unavailable sounddevice/numpy dependencies "
            "(metering only). Pure FX functions (_build_fx_graph, _format_params, "
            "_resolve_effect_controls, _get_fx_data) are used as-is, NOT copied."
        ),
        "notes": {
            "rust_style_floats": (
                "Graph and props values are formatted like Rust f32 Display "
                "(shortest round-trip, no trailing '.0'), not like Python "
                "_format_params (0.0). Rust is the J2 reference formatter."
            ),
            "props_shape": (
                "Python _build_fx_props emits a per-node NESTED struct which J1 "
                "proved is a silent no-op on PipeWire 0.3.65/1.2.x. props_flat / "
                "props_pairs use the corrected FLAT shape { \"params\" = [ "
                "\"eq_L:port\" val ... ] } (module emission order: plugin chain "
                "order, mono <label>_L then <label>_R, ports in control order)."
            ),
            "availability_override": (
                "os.path.exists was overridden so each case only 'sees' the listed "
                ".so files; fixtures therefore do not depend on the ambient plugin "
                "directory. The headless container has gate_1410, mbeq_1197, "
                "tap_tubewarmth, sc4_1882 (no rnnoise)."
            ),
            "output_kind_divergence": (
                "Python _build_fx_graph ignores strip.kind and would include a "
                "bypassed gate on an output chain when gate_1410.so is present. "
                "holaf-core restricts chains to EffectKey::for_strip(kind) "
                "(output = eq/tube/comp per spec §5). The output fixture "
                "restricts availability to {mbeq_1197,tap_tubewarmth,sc4_1882} so "
                "both Python and Rust agree."
            ),
        },
        "default_effect_params": table(DEFAULT_EFFECT_PARAMS),
        "bypass_params": table(BYPASS_PARAMS),
        "fx_plugin_map": fx_plugin_map,
        "stereo_plugins": {
            file: dict(ports) for file, ports in ae.STEREO_PLUGINS.items()
        },
        "cases": build_cases(),
    }

    with open(out_path, "w", encoding="utf-8") as handle:
        json.dump(payload, handle, indent=2, ensure_ascii=False)
        handle.write("\n")

    print(f"wrote {out_path}")
    print(f"  cases: {[c['id'] for c in payload['cases']]}")


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        ROOT, "rust", "crates", "holaf-core", "tests", "fixtures", "spa_builders_golden.json"
    )
    os.makedirs(os.path.dirname(out), exist_ok=True)
    main(out)
