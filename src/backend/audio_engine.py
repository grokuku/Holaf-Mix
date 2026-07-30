import subprocess
import time
import logging
import json
import re
import os
import select
from typing import Dict, List, Optional, Tuple

from . import pipewire_utils
from src.models.strip_model import Strip, StripType, StripMode, BYPASS_PARAMS, DEFAULT_EFFECT_PARAMS
from src.backend.metering import MeteringEngine

# NOTE: Logging is configured in main.py, not here. Module-level
# basicConfig() is process-global and only takes effect on the first call;
# doing it here makes the import order of modules matter, which is fragile.
logger = logging.getLogger("AudioEngine")

# Chemin standard des plugins LADSPA sous Arch/Linux
LADSPA_PATH = "/usr/lib/ladspa"

# --- Timing constants (module-level, no magic numbers scattered around) ---
# Virtual node creation: how long to wait for the node to appear in pw-dump
# (used by _create_virtual_node polling loop).
VIRTUAL_NODE_POLL_ATTEMPTS = 20
VIRTUAL_NODE_POLL_INTERVAL_S = 0.05

# FX chain verification: how long to wait for the filter-chain node ports
# to appear in pw-link output after a load-module command.
FX_CHAIN_PORT_VERIFY_ATTEMPTS = 20
FX_CHAIN_PORT_VERIFY_INTERVAL_S = 0.1

# FX host process shutdown grace period before SIGKILL.
FX_HOST_TERMINATE_TIMEOUT_S = 2

# Meter retry: how many get_meter_levels() calls between retry_pending()
# sweeps. At 25Hz meter timer, this is ~2s between sweeps.
METER_RETRY_INTERVAL_CYCLES = 50

# Default polling interval for waiting on async PipeWire operations.
PIPEWAIT_POLL_INTERVAL_S = 0.05

# Safety net: after this many consecutive set-param failures for a given
# strip, hot-reload is disabled and all FX toggles fall back to restart.
FX_HOTRELOAD_MAX_FAILURES = 3


# --- FX graph builders (module-level so they can be tested in isolation) ---

# Mapping: (effect_key, internal_name, plugin_file, ladspa_label)
FX_PLUGIN_MAP = [
    ('gate', 'gate', 'gate_1410', 'gate'),
    ('noise_cancel', 'rnnoise', 'librnnoise_ladspa', 'noise_suppressor_stereo'),
    ('eq', 'eq', 'mbeq_1197', 'mbeq'),
    ('tube', 'tube', 'valve_1209', 'valve'),
    ('compressor', 'comp', 'sc4_1882', 'sc4'),
]

# Plugins with true stereo LADSPA ports (single node instead of dual-mono).
# Each entry maps the plugin_file (3rd element of FX_PLUGIN_MAP) to its
# four stereo port names. Different plugins use different naming conventions:
#   sc4_1882:          "Left input" / "Right input" / "Left output" / "Right output"
#   librnnoise_ladspa: "Input (L)"  / "Input (R)"  / "Output (L)"  / "Output (R)"
STEREO_PLUGINS = {
    'sc4_1882': {'in_l': 'Left input', 'in_r': 'Right input',
                 'out_l': 'Left output', 'out_r': 'Right output'},
    'librnnoise_ladspa': {'in_l': 'Input (L)', 'in_r': 'Input (R)',
                          'out_l': 'Output (L)', 'out_r': 'Output (R)'},
}


def _get_fx_data(effects: Dict, key: str):
    """Returns (active: bool, params: dict) for a given effect, handling both
    legacy boolean and new dict formats."""
    data = effects.get(key)
    if isinstance(data, dict):
        return data.get('active', False), data.get('params', {})
    return bool(data), {}  # Fallback for old boolean style


def _has_active_fx(strip: Strip) -> bool:
    """Return True if the strip has at least one effect toggled active.

    Used by ``start_engine`` to decide whether to create a filter-chain.
    When no effect is active, the filter-chain is NOT created and routing
    falls back to direct monitor-port links (the original behaviour before
    always-on).  This prevents WirePlumber from auto-connecting the virtual
    source created by the filter-chain to physical outputs (which caused
    the microphone to leak into headphones).

    Once a filter-chain *is* created (because at least one effect was active
    at start time), it stays alive for the lifetime of the engine — all
    plugins remain in the graph with neutral bypass values when inactive, so
    subsequent toggles can use ``set-param`` (hot-reload) without a restart.
    """
    for (key, *_rest) in FX_PLUGIN_MAP:
        active, _ = _get_fx_data(strip.effects, key)
        if active:
            return True
    return False


def _format_params(params: Dict[str, float]) -> str:
    """
    Converts a dictionary of parameters into SPA-JSON control format.
    Example: {'Thresh': -30} -> '{ "Thresh" = -30 }'

    IMPORTANT: Keys containing '=' signs are **skipped** because PipeWire's
    SPA-JSON parser uses '=' as its key-value separator and may fail to
    parse the entire control block when a quoted key contains '=' —
    discarding ALL control values for that node.

    The only affected key is the gate_1410 plugin's
    'Output select (-1 = key listen, 0 = gate, 1 = bypass)'.
    Omitting it is safe: the LADSPA default for that port is 0 (gate mode),
    which is correct for active gates.  For inactive (bypassed) gates,
    the safety nets Range=0.0 and Threshold=-100.0 (set in BYPASS_PARAMS)
    make the gate fully transparent regardless of the Output select value.
    """
    if not params:
        return "{}"
    items = [f'"{k}" = {v}' for k, v in params.items() if '=' not in k]
    return f'{{ {" ".join(items)} }}'


def _resolve_effect_controls(strip, key: str) -> Dict[str, float]:
    """Return the full control-parameter dict for a given effect.

    When the effect is **active**, the user's params are used as-is.
    When the effect is **inactive**, BYPASS_PARAMS values are merged on top
    of DEFAULT_EFFECT_PARAMS so that *every* control port receives an
    explicit value.  This is critical for ``set-param`` (Props), which
    **replaces** all control values rather than merging them — sending only
    a partial set would reset unmentioned ports to LADSPA defaults and
    could produce unexpected audio artefacts.
    """
    active, params = _get_fx_data(strip.effects, key)
    if active:
        return params
    merged = dict(DEFAULT_EFFECT_PARAMS.get(key, {}))
    merged.update(BYPASS_PARAMS.get(key, {}))
    return merged


def _build_fx_props(strip, format_params_fn) -> str:
    """Build the Props SPA-JSON string for ``set-param`` on a filter-chain node.

    Unlike ``_build_fx_graph`` which produces the full graph definition
    (nodes, links, inputs, outputs), this function produces **only** the
    control values in the format expected by PipeWire's Props param:

    ::

        { "node_name" = { "control_port" = value } ... }

    Mono plugins get two entries (``_L`` and ``_R``); stereo plugins get
    one entry.  Returns an empty string if no plugins are available.
    """
    props_parts = []

    for (key, internal_name, plugin_file, ladspa_label) in FX_PLUGIN_MAP:
        plugin_abs_path = os.path.join(LADSPA_PATH, f"{plugin_file}.so")
        if not os.path.exists(plugin_abs_path):
            continue

        ctrl = _resolve_effect_controls(strip, key)
        ctrl_str = format_params_fn(ctrl)
        if ctrl_str == '{}':
            continue

        stereo_info = STEREO_PLUGINS.get(plugin_file)
        if stereo_info:
            props_parts.append(f'"{internal_name}" = {ctrl_str}')
        else:
            props_parts.append(f'"{internal_name}_L" = {ctrl_str}')
            props_parts.append(f'"{internal_name}_R" = {ctrl_str}')

    if not props_parts:
        return ""

    return f'{{ {" ".join(props_parts)} }}'


def _build_fx_graph(strip, format_params_fn, include_controls: bool) -> str:
    """
    Build the SPA-JSON filter.graph string for a strip's active effects.
    Extracted from _create_fx_chain to be a pure, testable function.

    Mono plugins (gate, eq, tube) are instantiated as dual-mono:
    two identical nodes (``name_L`` / ``name_R``) each processing one channel,
    with ports ``Input`` / ``Output``.

    Stereo plugins (sc4_1882, librnnoise_ladspa) are instantiated as a SINGLE
    node.  Port names come from the STEREO_PLUGINS dict because different
    plugins use different naming conventions (e.g. ``Left input`` vs
    ``Input (L)``).

    All effects are ALWAYS included in the graph.  When an effect is
    inactive, its control parameters are replaced with neutral BYPASS_PARAMS
    values so the plugin is transparent.  This allows toggling effects via
    ``set-param`` (hot-reload) once the filter-chain has been created.

    Note: the filter-chain is only *created* when at least one effect is
    active (see ``_has_active_fx``), but once it exists, ALL plugins are in
    the graph so future toggles don't need a restart.

    Args:
        strip: The Strip model containing the effects dict.
        format_params_fn: Callable that formats a params dict to SPA-JSON.
        include_controls: If False, omit the `control = ...` part of each node
                          (used as a fallback when the first attempt fails).
    """
    nodes_config = []
    links_config = []
    fx_list = []

    # Build the ordered list of ALL effects (always-on).  Inactive effects
    # use neutral BYPASS_PARAMS so they are transparent in the audio path.
    for (key, internal_name, plugin_file, ladspa_label) in FX_PLUGIN_MAP:
        active, params = _get_fx_data(strip.effects, key)
        plugin_abs_path = os.path.join(LADSPA_PATH, f"{plugin_file}.so")
        if not os.path.exists(plugin_abs_path):
            continue
        if active:
            ctrl = format_params_fn(params)
        else:
            # Merge bypass values over defaults so ALL control ports get
            # explicit values.  set-param replaces (not merges) controls,
            # so partial values would reset unmentioned ports to LADSPA
            # defaults — potentially causing unexpected audio behaviour.
            ctrl = format_params_fn(_resolve_effect_controls(strip, key))
        stereo_info = STEREO_PLUGINS.get(plugin_file)
        fx_list.append((internal_name, plugin_abs_path, ladspa_label, ctrl, stereo_info))

    if not fx_list:
        return ""

    # For each FX entry, track the four port-name strings used for linking.
    # mono:    (name_L:Input,  name_R:Input,  name_L:Output,  name_R:Output)
    # stereo:  port names from STEREO_PLUGINS dict
    fx_ports = []

    first_input_ports = None
    last_output_ports = None

    for i, (name, plugin_abs_path, label, ctrl, stereo_info) in enumerate(fx_list):
        control_str = f" control = {ctrl}" if include_controls and ctrl != '{}' else ""

        if stereo_info:
            # --- Single stereo node (e.g. sc4_1882, librnnoise_ladspa) ---
            nodes_config.append(
                f'{{ type = ladspa name = "{name}" plugin = "{plugin_abs_path}" '
                f'label = "{label}"{control_str} }}'
            )
            in_l = f"{name}:{stereo_info['in_l']}"
            in_r = f"{name}:{stereo_info['in_r']}"
            out_l = f"{name}:{stereo_info['out_l']}"
            out_r = f"{name}:{stereo_info['out_r']}"
        else:
            # --- Dual-mono nodes (gate, eq, tube) ---
            nodes_config.append(
                f'{{ type = ladspa name = "{name}_L" plugin = "{plugin_abs_path}" '
                f'label = "{label}"{control_str} }}'
            )
            nodes_config.append(
                f'{{ type = ladspa name = "{name}_R" plugin = "{plugin_abs_path}" '
                f'label = "{label}"{control_str} }}'
            )
            in_l = f"{name}_L:Input"
            in_r = f"{name}_R:Input"
            out_l = f"{name}_L:Output"
            out_r = f"{name}_R:Output"

        fx_ports.append((in_l, in_r, out_l, out_r))

        if i == 0:
            first_input_ports = [in_l, in_r]
        if i == len(fx_list) - 1:
            last_output_ports = [out_l, out_r]

        if i > 0:
            prev_out_l = fx_ports[i - 1][2]
            prev_out_r = fx_ports[i - 1][3]
            links_config.append(f'{{ output = "{prev_out_l}" input = "{in_l}" }}')
            links_config.append(f'{{ output = "{prev_out_r}" input = "{in_r}" }}')

    inputs_def = f'[ "{first_input_ports[0]}", "{first_input_ports[1]}" ]'
    outputs_def = f'[ "{last_output_ports[0]}", "{last_output_ports[1]}" ]'

    return (
        f'{{ '
        f'nodes = [ {" ".join(nodes_config)} ] '
        f'links = [ {" ".join(links_config)} ] '
        f'inputs = {inputs_def} '
        f'outputs = {outputs_def} '
        f'}}'
    )


class AudioEngine:
    def __init__(self):
        self.node_registry: Dict[str, int] = {}
        self.name_cache: Dict[int, str] = {}
        self.monitor_cache: Dict[int, str] = {}
        self.is_source_registry: Dict[str, bool] = {} 
        self.mono_registry: Dict[str, bool] = {}
        self.fx_source_names: Dict[str, str] = {}
        self.fx_sink_names: Dict[str, str] = {}
        self.link_registry: Dict[Tuple[str, str], List[Tuple[str, str]]] = {}
        self.created_nodes: List[int] = []
        self.fx_node_ids: Dict[str, int] = {}  # strip.uid → node_id of the filter-chain node
        self.fx_host_process: Optional[subprocess.Popen] = None
        self.metering = MeteringEngine()
        self._meter_retry_counter = 0
        # Safety net: track consecutive hot-reload (set-param) failures per
        # strip.  After FX_HOTRELOAD_MAX_FAILURES, hot-reload is disabled for
        # that strip and all toggles fall back to full restart.
        self._fx_hotreload_failures: Dict[str, int] = {}

    def start_engine(self, strips: List[Strip]):
        logger.info("Starting Audio Engine...")
        self.metering.stop_all()
        self._stop_fx_host()
        self._clean_zombie_nodes()
        # Reset the meter retry counter so the first retry after a restart
        # happens at a predictable offset (not somewhere in the middle of a
        # 50-cycle window left over from a previous run).
        self._meter_retry_counter = 0
        self._start_fx_host()

        self.node_registry.clear()
        self.name_cache.clear()
        self.monitor_cache.clear()
        self.is_source_registry.clear()
        self.mono_registry.clear()
        self.link_registry.clear()
        self.fx_source_names.clear()
        self.fx_sink_names.clear()
        self.fx_node_ids.clear()
        # Reset hot-reload failure counters on fresh start.
        self._fx_hotreload_failures.clear()
        
        # 2. Create Nodes
        for strip in strips:
            node_id = None
            node_name = None
            
            is_source = (strip.kind == StripType.INPUT and strip.mode == StripMode.PHYSICAL)
            self.is_source_registry[strip.uid] = is_source
            self.mono_registry[strip.uid] = strip.is_mono

            if strip.kind == StripType.OUTPUT and strip.mode == StripMode.PHYSICAL:
                node_id = self._find_physical_node(strip)
                if node_id:
                        node_name = self._get_node_name(node_id)
            else:
                if is_source:
                        if strip.device_name:
                            node_id = self._find_node_id_by_name(strip.device_name)
                            if node_id:
                                self.name_cache[node_id] = strip.device_name
                                # Physical sources expose the listenable signal on their
                                # monitor port (e.g. alsa_input.xxx.monitor). Populate
                                # monitor_cache so _link_physical_source_to_strip can
                                # route the signal to outputs when no FX chain is active.
                                self.monitor_cache[node_id] = f"{strip.device_name}.monitor"
                                node_name = strip.device_name
                else:
                    node_id = self._create_virtual_node(strip)
                    if node_id:
                        node_name = self.name_cache.get(node_id)
            
            if node_id or (is_source and strip.device_name):
                if node_id:
                    self.node_registry[strip.uid] = node_id
                    self.set_volume(strip.uid, strip.volume)
                    self.set_mute(strip.uid, strip.mute)
                
                # --- EFFECTS SETUP ---
                # The filter-chain is created ONLY when at least one effect is
                # active.  This avoids creating a virtual Audio/Source node for
                # every strip, which WirePlumber would auto-connect to physical
                # outputs (causing microphone leakage into headphones) and
                # which could interfere with the manual pw-link routing for
                # virtual inputs (causing Firefox/Desktop to be inaudible).
                #
                # When no effect is active, routing falls back to direct
                # monitor-port links via _link_physical_source_to_strip (for
                # physical sources) or the raw null-sink node (for virtual
                # inputs).
                #
                # Once a filter-chain is created, ALL plugins are included in
                # the graph (inactive ones use neutral BYPASS_PARAMS).  This
                # means subsequent effect toggles can use set-param (hot-reload)
                # without recreating the chain.  If no chain exists yet,
                # update_fx_params returns False and the UI triggers a restart
                # to create it.
                if _has_active_fx(strip):
                    if strip.kind == StripType.INPUT:
                        # Input FX: capture from source, playback as virtual source
                        base_source = strip.device_name if is_source else f"{node_name}.monitor"
                        if base_source:
                            fx_src = self._create_fx_chain(strip, base_source)
                            if fx_src:
                                self.fx_source_names[strip.uid] = fx_src
                    else:  # OUTPUT
                        # Output FX: capture as virtual sink (receives audio
                        # from routed inputs), playback to the physical/virtual
                        # device.
                        if node_name:
                            fx_sink = self._create_fx_chain(strip, node_name, is_output=True)
                            if fx_sink:
                                self.fx_sink_names[strip.uid] = fx_sink

                # --- METERING SETUP ---
                target_name = self.fx_source_names.get(strip.uid) or self._resolve_metering_target_name(strip, node_id)
                if target_name:
                    self.metering.start_monitoring(strip.uid, target_name)
                else:
                    logger.warning(f"Metering: Could not resolve target Name for {strip.label}")
            else:
                if not is_source:
                    logger.warning(f"Could not initialize node for strip: {strip.label}")

        # 3. Input Logic
        input_strips = [s for s in strips if s.kind == StripType.INPUT]
        for inp in input_strips:
            if inp.mode == StripMode.PHYSICAL and inp.device_name:
                self._link_physical_source_to_strip(inp)

        # 4. Routing Logic
        for inp in input_strips:
            source_uid = inp.uid
            if source_uid not in self.node_registry and source_uid not in self.fx_source_names:
                continue
            for target_uid in inp.routes:
                if target_uid in self.node_registry:
                    self.update_routing(source_uid, target_uid, active=True)

        # 5. Set Default Sink
        target_strip = next((s for s in strips if s.is_default and s.kind == StripType.INPUT), None)
        if not target_strip:
                target_strip = next((s for s in strips if s.label.lower() == "desktop" and s.kind == StripType.INPUT), None)
        if not target_strip:
            target_strip = next((s for s in strips if s.label.lower() == "default" and s.kind == StripType.INPUT), None)
        if not target_strip:
            target_strip = next((s for s in strips if s.kind == StripType.INPUT), None)

        if target_strip and target_strip.uid in self.node_registry:
            node_name = self.name_cache.get(self.node_registry[target_strip.uid])
            if node_name:
                self._set_system_default_sink(node_name)

        logger.info("Audio Engine Started.")

    def shutdown(self):
        logger.info("Shutting down Audio Engine...")
        self.metering.stop_all()
        self._stop_fx_host()
        self._clean_zombie_nodes()
        # Invalidate pw-dump cache so next start_engine() sees fresh state.
        pipewire_utils.invalidate_pw_dump_cache()
        self.created_nodes.clear()
        self.node_registry.clear()
        self.name_cache.clear()
        self.monitor_cache.clear()
        self.is_source_registry.clear()
        self.mono_registry.clear()
        self.fx_source_names.clear()
        self.fx_sink_names.clear()
        self.fx_node_ids.clear()

    # --- FX Host Management ---

    def _start_fx_host(self):
        if self.fx_host_process and self.fx_host_process.poll() is None:
            return 

        try:
            logger.info("Starting Persistent FX Host (pw-cli)...")
            # Capture stdout AND stderr via PIPE so we can detect errors
            # (especially set-param failures).  Previously these were
            # DEVNULL, which swallowed all error messages and made
            # debugging impossible.
            self.fx_host_process = subprocess.Popen(
                ['pw-cli'], 
                stdin=subprocess.PIPE, 
                stdout=subprocess.PIPE, 
                stderr=subprocess.PIPE, 
                text=True,
                bufsize=1
            )
            logger.info(f"FX Host started with PID: {self.fx_host_process.pid}")
        except Exception as e:
            logger.error(f"Failed to start FX Host: {e}")

    def _stop_fx_host(self):
        if self.fx_host_process:
            logger.info("Stopping FX Host...")
            self.fx_host_process.terminate()
            try:
                self.fx_host_process.wait(timeout=FX_HOST_TERMINATE_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                self.fx_host_process.kill()
            self.fx_host_process = None

    # --- pw-cli output helpers ---

    def _drain_pw_cli_output(self):
        """Non-blocking drain of any pending pw-cli stdout/stderr.

        Prevents the OS pipe buffer (~64 KB) from filling up and blocking
        pw-cli.  Should be called periodically and before sending commands.
        """
        if not self.fx_host_process or self.fx_host_process.poll() is not None:
            return
        try:
            while True:
                ready, _, _ = select.select(
                    [self.fx_host_process.stdout, self.fx_host_process.stderr],
                    [], [], 0.0
                )
                if not ready:
                    break
                for fd in ready:
                    fd.readline()
        except Exception:
            pass

    def _read_pw_cli_output(self, timeout: float = 0.3) -> Tuple[str, str]:
        """Read available output from pw-cli stdout and stderr.

        Returns (stdout_text, stderr_text).  Blocks up to *timeout* seconds
        for the first line, then drains remaining lines non-blocking.
        """
        stdout_data: List[str] = []
        stderr_data: List[str] = []
        if not self.fx_host_process or self.fx_host_process.poll() is not None:
            return "", "process dead"
        try:
            poll_timeout = timeout
            while True:
                ready, _, _ = select.select(
                    [self.fx_host_process.stdout, self.fx_host_process.stderr],
                    [], [], poll_timeout
                )
                if not ready:
                    break
                for fd in ready:
                    line = fd.readline()
                    if not line:
                        continue
                    if fd == self.fx_host_process.stdout:
                        stdout_data.append(line.rstrip())
                    else:
                        stderr_data.append(line.rstrip())
                poll_timeout = 0.05  # short timeout for subsequent reads
        except Exception as e:
            logger.error(f"Error reading pw-cli output: {e}")
        return "\n".join(stdout_data), "\n".join(stderr_data)

    # --- Public API ---
    
    def get_meter_levels(self):
        self._meter_retry_counter += 1
        if self._meter_retry_counter > METER_RETRY_INTERVAL_CYCLES:
            self.metering.retry_pending()
            self._meter_retry_counter = 0
            # Proactive FX-host health check: if pw-cli died (OOM, manual
            # kill, etc.) and no FX chain is currently loaded, restart it
            # silently so the next FX toggle is not delayed.
            if self.fx_host_process is None or self.fx_host_process.poll() is not None:
                logger.warning("FX host (pw-cli) is not running; restarting.")
                self._start_fx_host()
            else:
                # Drain any stale pw-cli output to prevent pipe buffer overflow.
                self._drain_pw_cli_output()
        return self.metering.get_levels()

    def set_volume(self, strip_uid: str, volume: float):
        node_id = self.node_registry.get(strip_uid)
        if not node_id: return

        node_name = self.name_cache.get(node_id)
        if not node_name:
            pipewire_utils.set_node_volume(node_id, volume)
            return

        vol_pct = f"{int(volume * 100)}%"
        is_source = self.is_source_registry.get(strip_uid, False)
        
        if is_source:
                subprocess.run(['pactl', 'set-source-volume', node_name, vol_pct], capture_output=True)
        else:
            subprocess.run(['pactl', 'set-sink-volume', node_name, vol_pct], capture_output=True)
            monitor_name = self.monitor_cache.get(node_id)
            if monitor_name:
                subprocess.run(['pactl', 'set-source-volume', monitor_name, vol_pct], capture_output=True)

    def set_mute(self, strip_uid: str, muted: bool):
        node_id = self.node_registry.get(strip_uid)
        if not node_id: return

        node_name = self.name_cache.get(node_id)
        val = "1" if muted else "0"

        if not node_name:
            pipewire_utils.toggle_node_mute(node_id, muted)
            return

        is_source = self.is_source_registry.get(strip_uid, False)

        if is_source:
            subprocess.run(['pactl', 'set-source-mute', node_name, val], capture_output=True)
        else:
            subprocess.run(['pactl', 'set-sink-mute', node_name, val], capture_output=True)
            monitor_name = self.monitor_cache.get(node_id)
            if monitor_name:
                subprocess.run(['pactl', 'set-source-mute', monitor_name, val], capture_output=True)

    def set_mono(self, strip_uid: str, enabled: bool):
        if self.mono_registry.get(strip_uid) == enabled:
            return 
        
        self.mono_registry[strip_uid] = enabled
        logger.info(f"Setting Mono for {strip_uid}: {enabled}")
        
        targets_to_refresh = []
        for (src, dst) in self.link_registry.keys():
            if src == strip_uid:
                targets_to_refresh.append(dst)
        
        for dst_uid in targets_to_refresh:
            self._destroy_link(strip_uid, dst_uid)
            self._create_link(strip_uid, dst_uid)

    def update_routing(self, source_uid: str, target_uid: str, active: bool):
        if active:
            self._create_link(source_uid, target_uid)
        else:
            self._destroy_link(source_uid, target_uid)

    def set_system_default(self, strip_uid: str):
        node_id = self.node_registry.get(strip_uid)
        if not node_id: return
        
        node_name = self.name_cache.get(node_id)
        if node_name:
            self._set_system_default_sink(node_name)

    # --- Internal Logic ---

    # Note: _format_params is now a module-level function (see top of file).
    # Kept as a method alias for backward compatibility with existing callers.
    def _format_params(self, params: Dict[str, float]) -> str:
        return _format_params(params)

    def _create_fx_chain(self, strip: Strip, master_node_name: str,
                         is_output: bool = False) -> Optional[str]:
        """Create a PipeWire filter-chain for the strip's active effects.

        For **input** strips (``is_output=False``):
            * ``capture.props`` is passive — it follows an external source.
            * ``playback.props`` has ``media.class = Audio/Source`` so the
              chain appears as a virtual source other nodes can listen to.
            * We link *source → filter input* and return the *filter output*
              node name (the virtual source).

        For **output** strips (``is_output=True``):
            * ``capture.props`` has ``media.class = Audio/Sink`` so the
              chain appears as a virtual sink that input strips route to.
            * ``playback.props`` is passive — it plays to the real device.
            * We link *filter output → device* and return the *filter input*
              node name (the virtual sink).
        """
        if not self.fx_host_process or self.fx_host_process.poll() is not None:
            logger.error("FX Host process is not running! Restarting...")
            self._start_fx_host()
            if not self.fx_host_process:
                return None

        fx_node_name = f"Holaf_FX_{strip.uid}"
        safe_label = re.sub(r'[^a-zA-Z0-9 ]', '', strip.label)
        fx_label = f"Holaf FX {safe_label}"

        if is_output:
            # Output: capture acts as a sink (receives from input strips),
            # playback sends processed audio to the physical/virtual device.
            # node.passive on capture.props tells WirePlumber NOT to auto-connect
            # this virtual sink to any physical source — only manual pw-link
            # routing should drive it.  stream.dont-remix prevents channel
            # remixing that could alter the signal.
            # media.role = comms prevents WirePlumber from listing this as a
            # regular media sink and auto-routing other streams to it (BUG 2:
            # VU meters going crazy on other outputs because WirePlumber was
            # auto-connecting audio to the FX chain's virtual sink).
            capture_props = 'media.class = Audio/Sink node.passive = true stream.dont-remix = true audio.channels = 2 audio.position = [ FL, FR ] media.role = comms'
            playback_props = 'node.passive = true stream.dont-remix = true audio.channels = 2 audio.position = [ FL, FR ]'
        else:
            # Input: capture receives from the source (passive follower),
            # playback acts as a virtual source.
            # node.passive on playback.props tells WirePlumber NOT to auto-connect
            # this virtual source to physical outputs (which would cause mic
            # leakage into headphones).  stream.dont-remix prevents channel
            # remixing.
            capture_props = 'node.passive = true stream.dont-remix = true audio.channels = 2 audio.position = [ FL, FR ]'
            playback_props = 'media.class = Audio/Source node.passive = true stream.dont-remix = true audio.channels = 2 audio.position = [ FL, FR ]'

        # NOTE: We no longer fall back to loading WITHOUT controls.  The
        # previous fallback (attempts = [True, False]) loaded the filter-chain
        # with LADSPA default values when the first attempt failed.  For the
        # gate, the LADSPA default is Output select = 0 (gate mode) with
        # Range = -90 dB → SILENCE.  If the first attempt fails, return None
        # so the caller can retry or fall back to a full engine restart.
        attempts = [True]

        for use_controls in attempts:
            graph_str = _build_fx_graph(strip, _format_params, use_controls)
            if not graph_str:
                return None

            fx_config_json = (
                f'{{ '
                f'node.name = "{fx_node_name}" '
                f'node.description = "{fx_label}" '
                f'media.name = "{fx_label}" '
                f'filter.graph = {graph_str} '
                f'capture.props = {{ {capture_props} }} '
                f'playback.props = {{ {playback_props} }} '
                f'}}'
            ).replace('\n', ' ')

            try:
                cmd_str = f"load-module libpipewire-module-filter-chain {fx_config_json}\n"
                
                logger.info(f"Sending FX command to host (controls={use_controls}, output={is_output})...")
                self.fx_host_process.stdin.write(cmd_str)
                self.fx_host_process.stdin.flush()
                
                # Drain any immediate pw-cli output (e.g. "loaded module: N")
                # so it doesn't accumulate in the pipe buffer.
                self._drain_pw_cli_output()
                
                # --- VERIFICATION ---
                in_node = f"input.{fx_node_name}"
                out_node = f"output.{fx_node_name}"

                ports_ready = False
                for _ in range(FX_CHAIN_PORT_VERIFY_ATTEMPTS):
                    time.sleep(FX_CHAIN_PORT_VERIFY_INTERVAL_S)
                    ports = self._get_ports_by_name(in_node, is_input=True)
                    if ports:
                        ports_ready = True
                        break
                    
                if not ports_ready:
                    logger.warning(f"FX Node verification failed (controls={use_controls}).")
                    # Clean up partially-created nodes before next attempt
                    self._destroy_nodes_by_name_substring(fx_node_name)
                    continue 

                logger.info(f"FX Chain successfully loaded: {fx_node_name}")

                # Linking Logic
                if is_output:
                    # Link filter output → physical/virtual device
                    links = self._auto_link_ports(out_node, master_node_name)
                else:
                    # Link source → filter input
                    links = self._auto_link_ports(master_node_name, in_node)
                if not links:
                    logger.info(f"Stereo link to FX incomplete (normal if link exists). Verifying...")
                
                in_id = self._find_node_id_by_name(in_node)
                out_id = self._find_node_id_by_name(out_node)
                if in_id: self.created_nodes.append(in_id)
                if out_id: self.created_nodes.append(out_id)
                
                # Store the main filter-chain node ID (not the input/output
                # stream nodes) so update_fx_params can send set-param to it.
                fc_node_id = self._find_node_id_by_name(fx_node_name)
                if fc_node_id:
                    self.fx_node_ids[strip.uid] = fc_node_id
                else:
                    logger.warning(f"Could not find filter-chain node ID for {fx_node_name}; hot-reload will fall back to restart.")
                
                # Return the node that other strips should connect to.
                if is_output:
                    return in_node   # virtual sink that input strips route to
                else:
                    return out_node  # virtual source that output strips listen to

            except Exception as e:
                logger.error(f"Exception during FX load: {e}")
                continue

        logger.error(f"All attempts to load FX failed for {strip.label}")
        return None

    def update_fx_params(self, strip: Strip) -> bool:
        """Update a strip's FX parameters in-place via pw-cli set-param, without
        recreating the filter-chain node.

        Returns True if the command was sent and pw-cli acknowledged it.
        Returns False if a full engine restart is needed as fallback.
        """
        # Safety net: after too many consecutive failures, disable hot-reload
        # for this strip and always fall back to restart.
        fail_count = self._fx_hotreload_failures.get(strip.uid, 0)
        if fail_count >= FX_HOTRELOAD_MAX_FAILURES:
            logger.warning(
                f"Hot-reload disabled for {strip.uid} ({fail_count} consecutive "
                f"failures); falling back to restart."
            )
            return False

        node_id = self.fx_node_ids.get(strip.uid)
        if not node_id or not self.fx_host_process:
            logger.warning(f"update_fx_params: no node_id for {strip.uid} or FX host down; falling back to restart.")
            return False

        # Check if pw-cli process is still alive before sending.
        if self.fx_host_process.poll() is not None:
            logger.error(
                f"pw-cli process is dead (exit code {self.fx_host_process.returncode}); "
                f"falling back to restart."
            )
            self._start_fx_host()
            return False

        # Build the Props string with control values ONLY (not the full graph).
        # The previous implementation sent the entire filter.graph definition
        # via set-param, which is invalid — set-param expects control values,
        # not a graph reconfiguration.  This was the primary cause of crashes.
        props_str = _build_fx_props(strip, _format_params)
        if not props_str:
            logger.warning(f"update_fx_params: props_str is empty for {strip.uid}; falling back to restart.")
            return False

        cmd = f'set-param {node_id} Props {props_str}\n'
        try:
            # Drain any stale output before sending the command so we don't
            # misinterpret old messages as errors.
            self._drain_pw_cli_output()

            logger.info(f"Hot-reload: sending set-param to node {node_id} for strip {strip.uid}.")
            logger.debug(f"Hot-reload command: {cmd.strip()}")
            self.fx_host_process.stdin.write(cmd)
            self.fx_host_process.stdin.flush()

            # Give pw-cli a moment to process the command.
            time.sleep(0.1)

            # Check if the process died immediately after the command.
            if self.fx_host_process.poll() is not None:
                logger.error(
                    f"pw-cli died after set-param (exit code "
                    f"{self.fx_host_process.returncode}); falling back to restart."
                )
                self._fx_hotreload_failures[strip.uid] = fail_count + 1
                self._start_fx_host()
                return False

            # Read any output — errors on stderr indicate failure.
            stdout_data, stderr_data = self._read_pw_cli_output(timeout=0.3)
            if stderr_data:
                logger.error(
                    f"pw-cli stderr after set-param: {stderr_data} "
                    f"(fail #{fail_count + 1} for {strip.uid})"
                )
                self._fx_hotreload_failures[strip.uid] = fail_count + 1
                return False
            if stdout_data:
                logger.debug(f"pw-cli stdout after set-param: {stdout_data}")

            # Success — reset the failure counter.
            self._fx_hotreload_failures[strip.uid] = 0
            logger.info(f"Hot-reload FX params sent successfully for strip {strip.uid}.")
            return True

        except BrokenPipeError as e:
            logger.error(f"update_fx_params: pw-cli pipe broken: {e}; falling back to restart.")
            self._fx_hotreload_failures[strip.uid] = fail_count + 1
            self._start_fx_host()
            return False
        except Exception as e:
            logger.error(f"update_fx_params: exception during set-param: {e}")
            self._fx_hotreload_failures[strip.uid] = fail_count + 1
            return False

    def reload_strip(self, strip: Strip) -> bool:
        """Destroy and recreate ONLY the FX chain for a single strip.

        Unlike ``shutdown()`` + ``start_engine()`` which tears down the
        entire engine, this method touches **only** the nodes and links
        belonging to *strip*.  Other strips are completely unaffected,
        which means WirePlumber does not see a device change for them and
        applications (YouTube, Discord, …) are not notified.

        Workflow:
            1.  Destroy the FX nodes (``Holaf_FX_{uid}``, ``input.…``,
                ``output.…``).
            2.  Destroy links that involve this strip (as source or as
                target) so stale pw-link connections don't linger.
            3.  Recreate the filter-chain via ``_create_fx_chain``.
            4.  Re-link routes (input→output) for this strip.
            5.  Restart metering for this strip.

        Returns ``True`` on success, ``False`` if the FX chain could not
        be recreated (caller should then fall back to full restart).
        """
        uid = strip.uid
        fx_node_name = f"Holaf_FX_{uid}"
        is_output = (strip.kind == StripType.OUTPUT)

        logger.info(f"reload_strip: reloading FX chain for '{strip.label}' (uid={uid}, output={is_output})")

        # --- 1. Destroy links involving this strip ---
        # Collect route pairs where this strip is the source or the target.
        routes_to_restore: List[Tuple[str, str]] = []
        keys_to_remove = []
        for (src_uid, dst_uid) in list(self.link_registry.keys()):
            if src_uid == uid or dst_uid == uid:
                routes_to_restore.append((src_uid, dst_uid))
                keys_to_remove.append((src_uid, dst_uid))

        for key in keys_to_remove:
            self._destroy_link(key[0], key[1])

        # --- 2. Destroy FX nodes for this strip ---
        self._destroy_nodes_by_name_substring(fx_node_name)

        # Clean up registries for this strip
        self.fx_source_names.pop(uid, None)
        self.fx_sink_names.pop(uid, None)
        self.fx_node_ids.pop(uid, None)
        # Reset hot-reload failure counter — fresh chain.
        self._fx_hotreload_failures.pop(uid, None)

        # Stop metering for this strip (will restart after chain creation)
        self.metering.stop_monitoring(uid)

        # --- 3. Recreate the FX chain (if any effect is active) ---
        node_id = self.node_registry.get(uid)
        node_name = self.name_cache.get(node_id) if node_id else None
        is_source = self.is_source_registry.get(uid, False)

        if not _has_active_fx(strip):
            # No active FX — no chain to create.  Fall back to direct
            # monitor-port routing for physical sources.
            if strip.kind == StripType.INPUT and is_source:
                self._link_physical_source_to_strip(strip)

            # Re-link routes
            for (src_uid, dst_uid) in routes_to_restore:
                self.update_routing(src_uid, dst_uid, active=True)

            # Restart metering
            target_name = self._resolve_metering_target_name(strip, node_id)
            if target_name:
                self.metering.start_monitoring(uid, target_name)
            return True

        if is_output:
            if not node_name:
                logger.error(f"reload_strip: no node name for output strip {strip.label}")
                return False
            fx_sink = self._create_fx_chain(strip, node_name, is_output=True)
            if not fx_sink:
                logger.error(f"reload_strip: failed to recreate FX chain for {strip.label}")
                return False
            self.fx_sink_names[uid] = fx_sink
        else:
            base_source = strip.device_name if is_source else (f"{node_name}.monitor" if node_name else None)
            if not base_source:
                logger.error(f"reload_strip: no base source for input strip {strip.label}")
                return False
            fx_src = self._create_fx_chain(strip, base_source, is_output=False)
            if not fx_src:
                logger.error(f"reload_strip: failed to recreate FX chain for {strip.label}")
                return False
            self.fx_source_names[uid] = fx_src

        # --- 4. Re-link routes involving this strip ---
        for (src_uid, dst_uid) in routes_to_restore:
            self.update_routing(src_uid, dst_uid, active=True)

        # --- 5. Restart metering for this strip ---
        target_name = self.fx_source_names.get(uid) or self._resolve_metering_target_name(strip, node_id)
        if target_name:
            self.metering.start_monitoring(uid, target_name)
        else:
            logger.warning(f"reload_strip: could not resolve metering target for {strip.label}")

        logger.info(f"reload_strip: successfully reloaded FX chain for '{strip.label}'")
        return True

    def _resolve_metering_target_name(self, strip: Strip, node_id: Optional[int]) -> Optional[str]:
        if strip.kind == StripType.INPUT and strip.mode == StripMode.PHYSICAL:
            return strip.device_name
        if node_id and node_id in self.monitor_cache:
            return self.monitor_cache[node_id]
        return None

    def _clean_zombie_nodes(self):
        logger.info("Cleaning up zombie nodes (Global Cleanup)...")
        try:
            res = subprocess.run(['pw-dump'], capture_output=True, text=True)
            data = json.loads(res.stdout)
            to_destroy = []
            for obj in data:
                props = obj.get('info', {}).get('props', {})
                name = props.get('node.name', '') or props.get('module.name', '') or ''
                desc = props.get('node.description', '')
                if "Holaf" in name or "Holaf" in desc:
                    to_destroy.append(obj['id'])
            
            if to_destroy:
                for oid in to_destroy:
                    subprocess.run(['pw-cli', 'destroy', str(oid)], capture_output=True)
                logger.info(f"Destroyed {len(to_destroy)} zombie objects.")
                time.sleep(0.2)
        except Exception as e:
            logger.error(f"Cleanup failed: {e}")

    def _destroy_nodes_by_name_substring(self, name_substring: str):
        """Destroy all PipeWire nodes whose node.name contains the given substring."""
        try:
            res = subprocess.run(['pw-dump'], capture_output=True, text=True)
            data = json.loads(res.stdout)
            for obj in data:
                props = obj.get('info', {}).get('props', {})
                name = props.get('node.name', '')
                if name_substring in name:
                    subprocess.run(['pw-cli', 'destroy', str(obj['id'])], capture_output=True)
            time.sleep(0.2)
        except Exception as e:
            logger.error(f"Targeted node destruction failed for '{name_substring}': {e}")

    def _set_system_default_sink(self, node_name: str):
        try:
            subprocess.run(['pactl', 'set-default-sink', node_name], check=True, capture_output=True)
            logger.info(f"System default sink set to: {node_name}")
        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to set default sink: {e}")

    def _create_virtual_node(self, strip: Strip) -> Optional[int]:
        node_name = f"Holaf_Strip_{strip.uid}"
        sink_desc = f"Holaf Mix: {strip.label}"
        
        cmd_pactl = [
            'pactl', 'load-module', 'module-null-sink',
            f'sink_name={node_name}',
            f'sink_properties=device.description="{sink_desc}"'
        ]
        
        try:
            proc = subprocess.run(cmd_pactl, capture_output=True, text=True)
            if proc.returncode != 0:
                logger.warning(f"pactl failed: {proc.stderr}")
                return None
            else:
                logger.info(f"Created virtual sink via pactl: {node_name}")

            # Poll for the node to appear in pw-dump. 0.3s is enough on a
            # fast system but unreliable under load; polling exits as soon
            # as the node is visible (typically <50ms) and tolerates slower
            # PipeWire init up to ~1s total.
            node_id = None
            for _attempt in range(VIRTUAL_NODE_POLL_ATTEMPTS):
                node_id = self._find_node_id_by_name(node_name)
                if node_id:
                    break
                time.sleep(VIRTUAL_NODE_POLL_INTERVAL_S)

            if node_id:
                self.created_nodes.append(node_id)
                self.name_cache[node_id] = node_name
                self.monitor_cache[node_id] = f"{node_name}.monitor"
                
                if strip.kind == StripType.OUTPUT and strip.mode == StripMode.VIRTUAL:
                    remap_name = f"{node_name}_remap"
                    remap_desc = f"Holaf Output ({strip.label})"
                    
                    cmd_remap = [
                        'pactl', 'load-module', 'module-remap-source',
                        f'master={node_name}.monitor',
                        f'source_name={remap_name}',
                        f'source_properties=device.description="{remap_desc}"'
                    ]
                    
                    remap_proc = subprocess.run(cmd_remap, capture_output=True, text=True)
                    if remap_proc.returncode == 0:
                        logger.info(f"Created remapped source: {remap_desc}")
                        time.sleep(PIPEWAIT_POLL_INTERVAL_S * 2)
                        remap_id = self._find_node_id_by_name(remap_name)
                        if remap_id:
                            self.created_nodes.append(remap_id)
                    else:
                        logger.warning(f"Failed to create remapped source: {remap_proc.stderr}")

                return node_id
                
        except (subprocess.CalledProcessError, FileNotFoundError) as e:
            logger.error(f"Failed to create node via pactl: {e}")
        
        return None

    def _link_physical_source_to_strip(self, strip: Strip):
        """
        Prepares an input strip backed by a physical source (e.g. USB mic).

        For a physical source in PipeWire, the *listenable* signal is exposed
        on the source's monitor port (e.g. `alsa_input.usb-...:monitor_FL`),
        not on the source node itself. We register the monitor name in
        `fx_source_names` so that the routing logic in `_create_link` picks
        the correct source, mirroring what an FX chain would do.

        Called only when no FX chain is active (the FX branch sets
        `fx_source_names[uid]` itself in `start_engine`).
        """
        if strip.uid not in self.node_registry:
            return
        node_id = self.node_registry[strip.uid]
        monitor_name = self.monitor_cache.get(node_id)
        if not monitor_name:
            logger.warning(f"Physical source '{strip.label}' has no monitor port; cannot route.")
            return
        # Store as the 'effective source' so _create_link uses it for routing.
        # FX chain (if any) would have already populated this with output.Holaf_FX_<uid>,
        # in which case we leave it untouched to preserve the active FX path.
        if strip.uid not in self.fx_source_names:
            self.fx_source_names[strip.uid] = monitor_name
            logger.info(f"Physical source '{strip.label}' linked via monitor: {monitor_name}")

    def _find_node_id_by_name(self, node_name: str) -> Optional[int]:
        nodes = pipewire_utils.get_audio_nodes(include_internal=True)
        for node in nodes:
            if node.get('name') == node_name:
                return node['id']
        return None

    def _find_physical_node(self, strip: Strip) -> Optional[int]:
        nodes = pipewire_utils.get_audio_nodes(include_internal=True)
        target_class = "Audio/Sink" if strip.kind == StripType.OUTPUT else "Audio/Source"
        candidates = [n for n in nodes if n['media_class'] == target_class]

        if strip.device_name:
            for node in candidates:
                if node['name'] == strip.device_name:
                    nid = node['id']
                    self.name_cache[nid] = node['name']
                    
                    if 'monitor_source_name' in node and node['monitor_source_name']:
                        self.monitor_cache[nid] = node['monitor_source_name']
                    else:
                        self.monitor_cache[nid] = f"{node['name']}.monitor"
                        
                    return nid
        return None

    def _destroy_node(self, node_id: int):
        subprocess.run(['pw-cli', 'destroy', str(node_id)], capture_output=True)

    def _get_node_name(self, node_id: int) -> Optional[str]:
        if node_id in self.name_cache:
            return self.name_cache[node_id]
        
        info = pipewire_utils.get_node_info(node_id)
        if info and 'info' in info and 'props' in info['info']:
            name = info['info']['props'].get('node.name')
            if name:
                self.name_cache[node_id] = name
                return name
        return None

    def _get_ports_by_name(self, node_name: str, is_input: bool) -> List[str]:
        flag = '-i' if is_input else '-o'
        try:
            result = subprocess.run(['pw-link', flag, '-l'], capture_output=True, text=True)
            ports = []
            for line in result.stdout.splitlines():
                clean_line = line.strip()
                pattern = r"(?:[\d]+:\s*)?(?:[\|\-><\s]+)?(" + re.escape(node_name) + r":\S+)"
                match = re.search(pattern, clean_line)
                if match:
                    ports.append(match.group(1))
            return ports
        except Exception:
            return []

    def _pw_link(self, port_src: str, port_dst: str) -> bool:
        try:
            result = subprocess.run(
                ['pw-link', port_src, port_dst], 
                capture_output=True, text=True
            )
            
            if result.returncode == 0:
                return True
            
            err = result.stderr.lower()
            if "exists" in err or "existe" in err:
                return True
            
            logger.warning(f"Failed to link {port_src} -> {port_dst}: {result.stderr.strip()}")
            return False
        except Exception as e:
            logger.error(f"Error executing pw-link: {e}")
            return False

    def _unlink_nodes(self, node_src: str, node_dst: str):
        src_ports = self._get_ports_by_name(node_src, is_input=False)
        dst_ports = self._get_ports_by_name(node_dst, is_input=True)

        if not src_ports or not dst_ports:
            return

        for s in src_ports:
            for d in dst_ports:
                logger.info(f"Ensure Unlink: {s} -X- {d}")
                result = subprocess.run(['pw-link', '-d', s, d],
                                        capture_output=True, text=True)
                if result.returncode != 0:
                    # Log instead of silently swallowing. Genuine errors
                    # (port gone, daemon down, perms) were previously hidden.
                    logger.warning(f"Failed to unlink {s} -X- {d}: {result.stderr.strip()}")

    def _auto_link_ports(self, src_name: str, dst_name: str, force_mono: bool = False) -> List[Tuple[str, str]]:
        src_ports = self._get_ports_by_name(src_name, is_input=False)
        dst_ports = self._get_ports_by_name(dst_name, is_input=True)

        if not src_ports or not dst_ports:
            logger.warning(f"Auto-Link failed: Missing ports for {src_name} or {dst_name}")
            return []

        links_to_make = []
        
        def is_left(p): return 'FL' in p or 'left' in p.lower() or 'MONO' in p or ':capture_0' in p or ':output_0' in p
        def is_right(p): return 'FR' in p or 'right' in p.lower() or ':capture_1' in p or ':output_1' in p

        src_l = next((p for p in src_ports if is_left(p)), None)
        src_r = next((p for p in src_ports if is_right(p)), None)
        
        if not src_l and len(src_ports) > 0: src_l = src_ports[0]
        if not src_r and len(src_ports) > 1: src_r = src_ports[1]

        dst_l = next((p for p in dst_ports if is_left(p)), None)
        dst_r = next((p for p in dst_ports if is_right(p)), None)
        
        if not dst_l and len(dst_ports) > 0: dst_l = dst_ports[0]
        if not dst_r and len(dst_ports) > 1: dst_r = dst_ports[1]

        if force_mono:
            # Mono downmix: take left channel, copy to both outputs.
            # Do NOT link src_r to dst_l/dst_r, as PipeWire SUMS linked inputs,
            # which would add +6dB for correlated (mono) signals causing saturation.
            if src_l:
                if dst_l: links_to_make.append((src_l, dst_l))
                if dst_r: links_to_make.append((src_l, dst_r))
            else:
                # Fallback if no left channel found: use right for both
                if src_r:
                    if dst_l: links_to_make.append((src_r, dst_l))
                    if dst_r: links_to_make.append((src_r, dst_r))
        else:
            if src_l and dst_l: links_to_make.append((src_l, dst_l))
            if src_r and dst_r: links_to_make.append((src_r, dst_r))
        
        # Special case: Mono Source to Stereo Dest
        if len(src_ports) == 1 and len(dst_ports) >= 2 and not force_mono:
            if src_ports and dst_l: links_to_make.append((src_ports[0], dst_l))
            if src_ports and dst_r: links_to_make.append((src_ports[0], dst_r))

        created_links = []
        for p_src, p_dst in links_to_make:
            if self._pw_link(p_src, p_dst):
                created_links.append((p_src, p_dst))
        
        return created_links

    def _create_link(self, source_uid: str, target_uid: str):
        src_id = self.node_registry.get(source_uid)
        dst_id = self.node_registry.get(target_uid)
        
        if not src_id or not dst_id: return
        
        active_src_name = self.fx_source_names.get(source_uid)
        raw_src_name = self._get_node_name(src_id)
        # If the output (target) has an FX chain, route to its virtual sink
        # instead of the raw physical/virtual device node.
        dst_name = self.fx_sink_names.get(target_uid) or self._get_node_name(dst_id)
        
        if not dst_name: return

        # ANTI-GATE FIX: EXCLUSIVE ROUTING
        if active_src_name:
            if raw_src_name:
                self._unlink_nodes(raw_src_name, dst_name)
            src_name_to_use = active_src_name
        else:
            fx_name_potential = f"output.Holaf_FX_{source_uid}"
            self._unlink_nodes(fx_name_potential, dst_name)
            src_name_to_use = raw_src_name
        
        if not src_name_to_use: return

        is_mono = self.mono_registry.get(source_uid, False)
        created_links = self._auto_link_ports(src_name_to_use, dst_name, force_mono=is_mono)
        
        if created_links:
            self.link_registry[(source_uid, target_uid)] = created_links

    def _destroy_link(self, source_uid: str, target_uid: str):
        links = self.link_registry.pop((source_uid, target_uid), [])
        
        src_id = self.node_registry.get(source_uid)
        dst_id = self.node_registry.get(target_uid)
        if src_id and dst_id:
            raw_name = self._get_node_name(src_id)
            fx_name = self.fx_source_names.get(source_uid)
            dst_name = self._get_node_name(dst_id)
            fx_sink_name = self.fx_sink_names.get(target_uid)
            
            # Unlink from both the raw device node and the FX sink (if any).
            for dn in [dst_name, fx_sink_name]:
                if dn:
                    if raw_name: self._unlink_nodes(raw_name, dn)
                    if fx_name: self._unlink_nodes(fx_name, dn)

        for (p_src, p_dst) in links:
                result = subprocess.run(['pw-link', '-d', p_src, p_dst],
                                        capture_output=True, text=True)
                if result.returncode != 0:
                    logger.warning(f"Failed to unlink {p_src} -X- {p_dst}: {result.stderr.strip()}")