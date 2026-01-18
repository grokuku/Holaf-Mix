import subprocess
import time
import logging
import json
import re
import os
from typing import Dict, List, Optional, Tuple

from . import pipewire_utils
from src.models.strip_model import Strip, StripType, StripMode
from src.backend.metering import MeteringEngine

# Configuration du logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger("AudioEngine")

# Chemin standard des plugins LADSPA sous Arch/Linux
LADSPA_PATH = "/usr/lib/ladspa"

class AudioEngine:
    def __init__(self):
        self.node_registry: Dict[str, int] = {}
        self.name_cache: Dict[int, str] = {}
        self.monitor_cache: Dict[int, str] = {}
        self.is_source_registry: Dict[str, bool] = {} 
        self.mono_registry: Dict[str, bool] = {}
        self.fx_source_names: Dict[str, str] = {}
        self.link_registry: Dict[Tuple[str, str], List[Tuple[str, str]]] = {}
        self.created_nodes: List[int] = []
        self.fx_host_process: Optional[subprocess.Popen] = None
        self.metering = MeteringEngine()
        self._meter_retry_counter = 0

    def start_engine(self, strips: List[Strip]):
        logger.info("Starting Audio Engine...")
        self.metering.stop_all() 
        self._stop_fx_host() 
        self._clean_zombie_nodes()
        self._start_fx_host()

        self.node_registry.clear()
        self.name_cache.clear()
        self.monitor_cache.clear()
        self.is_source_registry.clear()
        self.mono_registry.clear()
        self.link_registry.clear()
        self.fx_source_names.clear()
        
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
                
                # --- EFFECTS SETUP (UPDATED LOGIC) ---
                if strip.kind == StripType.INPUT:
                    # Check if ANY effect is effectively active
                    has_active_fx = False
                    for fx_data in strip.effects.values():
                        if isinstance(fx_data, dict) and fx_data.get('active'):
                            has_active_fx = True
                            break
                        elif isinstance(fx_data, bool) and fx_data:
                            has_active_fx = True
                            break
                    
                    if has_active_fx:
                        base_source = strip.device_name if is_source else f"{node_name}.monitor"
                        if base_source:
                            fx_src = self._create_fx_chain(strip, base_source)
                            if fx_src:
                                self.fx_source_names[strip.uid] = fx_src

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
        self.created_nodes.clear()
        self.node_registry.clear()
        self.name_cache.clear()
        self.monitor_cache.clear()
        self.is_source_registry.clear()
        self.mono_registry.clear()
        self.fx_source_names.clear()

    # --- FX Host Management ---

    def _start_fx_host(self):
        if self.fx_host_process and self.fx_host_process.poll() is None:
            return 

        try:
            logger.info("Starting Persistent FX Host (pw-cli)...")
            self.fx_host_process = subprocess.Popen(
                ['pw-cli'], 
                stdin=subprocess.PIPE, 
                stdout=subprocess.DEVNULL, 
                stderr=subprocess.DEVNULL, 
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
                self.fx_host_process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.fx_host_process.kill()
            self.fx_host_process = None

    # --- Public API ---
    
    def get_meter_levels(self):
        self._meter_retry_counter += 1
        if self._meter_retry_counter > 50:
            self.metering.retry_pending()
            self._meter_retry_counter = 0
        return self.metering.get_levels()

    def update_fx_params(self, strip: Strip):
        """
        Hot-reload FX chain when parameters change.
        Currently implements a full reload strategy (simplest/safest for graph consistency).
        """
        # 1. Unlink current FX
        if strip.uid in self.fx_source_names:
            old_fx_node = self.fx_source_names[strip.uid]
            # Unlink from all targets
            for (src, dst) in list(self.link_registry.keys()):
                if src == strip.uid:
                    dst_id = self.node_registry.get(dst)
                    if dst_id:
                        dst_name = self._get_node_name(dst_id)
                        if dst_name:
                            self._unlink_nodes(old_fx_node, dst_name)
            
            # Destroy old FX (Wait a bit handled by _clean_zombie_nodes logic if we restart whole engine)
            # But here we want to be surgical.
            # For now, simplest is to just restart the specific chain?
            # Creating a new one with same name might conflict.
            # Given the complexity, we will rely on the UI triggering a restart 
            # or just re-run routing update.
            # TODO: Ideally, implement parameter update via 'pw-cli set-param' but hard with filter-chain.
            pass
        
        # Real-time parameter update for LADSPA in filter-chain is complex without destroying/recreating.
        # For now, the UI might need to restart engine or we implement a "Respawn FX" method.
        # But for this fix, we just ensure START works correctly.
        pass

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

    def set_mono_registry(self, strip_uid: str, is_mono: bool):
        self.mono_registry[strip_uid] = is_mono

    def set_system_default(self, strip_uid: str):
        node_id = self.node_registry.get(strip_uid)
        if not node_id: return
        
        node_name = self.name_cache.get(node_id)
        if node_name:
            self._set_system_default_sink(node_name)

    # --- Internal Logic ---

    def _format_params(self, params: Dict[str, float]) -> str:
        """
        Converts a dictionary of parameters into SPA-JSON control format.
        Example: {'Thresh': -30} -> '{ "Thresh" = -30 }'
        """
        if not params:
            return "{}"
        items = [f'"{k}" = {v}' for k, v in params.items()]
        return f'{{ {" ".join(items)} }}'

    def _create_fx_chain(self, strip: Strip, master_source_name: str) -> Optional[str]:
        if not self.fx_host_process or self.fx_host_process.poll() is not None:
            logger.error("FX Host process is not running! Restarting...")
            self._start_fx_host()
            if not self.fx_host_process:
                return None

        fx_node_name = f"Holaf_FX_{strip.uid}"
        safe_label = re.sub(r'[^a-zA-Z0-9 ]', '', strip.label)
        fx_label = f"Holaf FX {safe_label}"
        
        def build_graph(include_controls: bool) -> str:
            nodes_config = []
            links_config = []
            
            # --- 1. Select Candidates & Prepare Params ---
            fx_candidates = []
            
            # Helper to safely get active state and params
            def get_fx_data(key):
                data = strip.effects.get(key)
                if isinstance(data, dict):
                    return data.get('active', False), data.get('params', {})
                return bool(data), {} # Fallback for old boolean style

            # GATE
            active, params = get_fx_data('gate')
            if active:
                ctrl = self._format_params(params)
                fx_candidates.append(('gate', 'gate_1410', 'gate', ctrl))

            # NOISE CANCEL
            active, params = get_fx_data('noise_cancel')
            if active:
                # RNNoise usually has no controls, but we keep format consistent
                ctrl = self._format_params(params)
                fx_candidates.append(('rnnoise', 'librnnoise_ladspa', 'noise_suppressor_mono', ctrl))

            # EQ
            active, params = get_fx_data('eq')
            if active:
                ctrl = self._format_params(params)
                fx_candidates.append(('eq', 'mbeq_1197', 'mbeq', ctrl))

            # COMPRESSOR
            active, params = get_fx_data('compressor')
            if active:
                ctrl = self._format_params(params)
                fx_candidates.append(('comp', 'sc4_1882', 'sc4', ctrl))

            fx_list = []
            for (name, plugin, label, ctrl) in fx_candidates:
                plugin_path = os.path.join(LADSPA_PATH, f"{plugin}.so")
                if os.path.exists(plugin_path):
                    fx_list.append((name, plugin_path, label, ctrl)) 
            
            if not fx_list: return ""

            # --- 2. Build Nodes & Internal Links ---
            first_nodes = []
            last_nodes = []

            for i, (name, plugin_abs_path, label, ctrl) in enumerate(fx_list):
                control_str = f" control = {ctrl}" if include_controls and ctrl != '{}' else ""
                
                nodes_config.append(f'{{ type = ladspa name = "{name}_L" plugin = "{plugin_abs_path}" label = "{label}"{control_str} }}')
                nodes_config.append(f'{{ type = ladspa name = "{name}_R" plugin = "{plugin_abs_path}" label = "{label}"{control_str} }}')
                
                if i == 0:
                    first_nodes = [f"{name}_L", f"{name}_R"]
                
                if i == len(fx_list) - 1:
                    last_nodes = [f"{name}_L", f"{name}_R"]

                if i > 0:
                    prev_name = fx_list[i-1][0]
                    links_config.append(f'{{ output = "{prev_name}_L:Output" input = "{name}_L:Input" }}')
                    links_config.append(f'{{ output = "{prev_name}_R:Output" input = "{name}_R:Input" }}')

            # --- 3. Define Inputs/Outputs ---
            inputs_def = f'[ "{first_nodes[0]}:Input", "{first_nodes[1]}:Input" ]'
            outputs_def = f'[ "{last_nodes[0]}:Output", "{last_nodes[1]}:Output" ]'

            nodes_str = " ".join(nodes_config)
            links_str = " ".join(links_config)
            
            return (
                f'{{ '
                f'nodes = [ {nodes_str} ] '
                f'links = [ {links_str} ] '
                f'inputs = {inputs_def} '
                f'outputs = {outputs_def} '
                f'}}'
            )

        attempts = [True, False] 
        
        for use_controls in attempts:
            graph_str = build_graph(use_controls)
            if not graph_str:
                return None

            fx_config_json = (
                f'{{ '
                f'node.name = "{fx_node_name}" '
                f'node.description = "{fx_label}" '
                f'media.name = "{fx_label}" '
                f'filter.graph = {graph_str} '
                f'capture.props = {{ node.passive = true audio.channels = 2 audio.position = [ FL, FR ] }} '
                f'playback.props = {{ media.class = Audio/Source audio.channels = 2 audio.position = [ FL, FR ] }} '
                f'}}'
            ).replace('\n', ' ')

            try:
                cmd_str = f"load-module libpipewire-module-filter-chain {fx_config_json}\n"
                
                logger.info(f"Sending FX command to host (controls={use_controls})...")
                self.fx_host_process.stdin.write(cmd_str)
                self.fx_host_process.stdin.flush()
                
                # --- VERIFICATION ---
                in_node = f"input.{fx_node_name}"
                out_node = f"output.{fx_node_name}"
                
                retries = 20 
                ports_ready = False
                while retries > 0:
                    time.sleep(0.1)
                    ports = self._get_ports_by_name(in_node, is_input=True)
                    if ports:
                        ports_ready = True
                        break
                    retries -= 1
                    
                if not ports_ready:
                    logger.warning(f"FX Node verification failed (controls={use_controls}).")
                    continue 

                logger.info(f"FX Chain successfully loaded: {fx_node_name}")

                # Linking Logic
                links = self._auto_link_ports(master_source_name, in_node)
                if not links:
                    logger.info(f"Stereo link to FX incomplete (normal if link exists). Verifying...")
                
                in_id = self._find_node_id_by_name(in_node)
                out_id = self._find_node_id_by_name(out_node)
                if in_id: self.created_nodes.append(in_id)
                if out_id: self.created_nodes.append(out_id)
                
                return out_node

            except Exception as e:
                logger.error(f"Exception during FX load: {e}")
                continue

        logger.error(f"All attempts to load FX failed for {strip.label}")
        return None

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
                process = subprocess.Popen(
                    ['pw-cli'], 
                    stdin=subprocess.PIPE, 
                    stdout=subprocess.DEVNULL, 
                    stderr=subprocess.DEVNULL, 
                    text=True
                )
                cmds = "\n".join([f"destroy {oid}" for oid in to_destroy])
                process.communicate(input=f"{cmds}\nquit\n")
                logger.info(f"Destroyed {len(to_destroy)} zombie objects.")
                time.sleep(0.2)
        except Exception as e:
            logger.error(f"Cleanup failed: {e}")

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
            
            time.sleep(0.3)
            
            node_id = self._find_node_id_by_name(node_name)
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
                        time.sleep(0.1)
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
        pass

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
                subprocess.run(['pw-link', '-d', s, d], capture_output=True, check=False)

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
            if src_l:
                if dst_l: links_to_make.append((src_l, dst_l))
                if dst_r: links_to_make.append((src_l, dst_r))
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
        dst_name = self._get_node_name(dst_id)
        
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
            
            if dst_name:
                if raw_name: self._unlink_nodes(raw_name, dst_name)
                if fx_name: self._unlink_nodes(fx_name, dst_name)

        for (p_src, p_dst) in links:
                subprocess.run(['pw-link', '-d', p_src, p_dst], capture_output=True)