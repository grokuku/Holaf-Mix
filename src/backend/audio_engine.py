import subprocess
import time
import logging
from typing import Dict, List, Optional, Tuple
import pipewire_utils
from src.models.strip_model import Strip, StripType, StripMode
from src.backend.metering import MeteringEngine

# Configuration du logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger("AudioEngine")

class AudioEngine:
    def __init__(self):
        self.node_registry: Dict[str, int] = {}
        # Cache pour stocker les noms des noeuds : ID -> Name
        self.name_cache: Dict[int, str] = {}
        # Cache pour stocker le vrai nom du moniteur (Source) : ID -> Monitor Name
        self.monitor_cache: Dict[int, str] = {}
        # Registre pour savoir si un noeud est une Source (Input Physique) ou un Sink
        self.is_source_registry: Dict[str, bool] = {} 
        # Registre pour stocker l'état Mono des strips
        self.mono_registry: Dict[str, bool] = {}

        self.link_registry: Dict[Tuple[str, str], List[Tuple[str, str]]] = {}
        self.created_nodes: List[int] = []
        
        # Metering System
        self.metering = MeteringEngine()
        self._meter_retry_counter = 0

    def start_engine(self, strips: List[Strip]):
        logger.info("Starting Audio Engine...")
        
        # 0. CLEANUP
        self.metering.stop_all() 
        self._clean_zombie_nodes()

        self.node_registry.clear()
        self.name_cache.clear()
        self.monitor_cache.clear()
        self.is_source_registry.clear()
        self.mono_registry.clear()
        self.link_registry.clear()
        
        # 1. Create Nodes (Virtual Strips)
        for strip in strips:
            node_id = None
            node_name = None
            
            # Identify if this strip behaves as a Source (Physical Input)
            # Virtual inputs are technically Null-Sinks, so only PHYSICAL INPUTS are Sources.
            is_source = (strip.kind == StripType.INPUT and strip.mode == StripMode.PHYSICAL)
            self.is_source_registry[strip.uid] = is_source
            self.mono_registry[strip.uid] = strip.is_mono

            if strip.kind == StripType.OUTPUT and strip.mode == StripMode.PHYSICAL:
                # Direct hardware mapping
                node_id = self._find_physical_node(strip)
                if node_id:
                     node_name = self._get_node_name(node_id)
            else:
                # Virtual Node creation (or Physical Input placeholder handled later)
                if is_source:
                     # Physical Inputs are linked, not created. 
                     # But we need their ID in the registry for Mute/Volume to work.
                     if strip.device_name:
                         node_id = self._find_node_id_by_name(strip.device_name)
                         if node_id:
                             self.name_cache[node_id] = strip.device_name
                else:
                    node_id = self._create_virtual_node(strip)
                    if node_id:
                        node_name = self.name_cache.get(node_id)
            
            if node_id:
                self.node_registry[strip.uid] = node_id
                self.set_volume(strip.uid, strip.volume)
                self.set_mute(strip.uid, strip.mute)
                
                # --- METERING SETUP (Non-Blocking via Threaded Metering) ---
                target_name = self._resolve_metering_target_name(strip, node_id)
                
                if target_name:
                    self.metering.start_monitoring(strip.uid, target_name)
                else:
                    logger.warning(f"Metering: Could not resolve target Name for {strip.label}")
            else:
                if not is_source: # Only warn if we expected to create/find a sink
                    logger.warning(f"Could not initialize node for strip: {strip.label}")

        # 2. Input Logic: Link Physical Sources -> Input Strips
        input_strips = [s for s in strips if s.kind == StripType.INPUT]
        for inp in input_strips:
            if inp.mode == StripMode.PHYSICAL and inp.device_name:
                self._link_physical_source_to_strip(inp)

        # 3. Routing Logic: Input Strips -> Output Strips
        for inp in input_strips:
            source_uid = inp.uid
            if source_uid not in self.node_registry:
                continue
            for target_uid in inp.routes:
                if target_uid in self.node_registry:
                    self.update_routing(source_uid, target_uid, active=True)

        # 4. Set Default Sink Strategy
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
        self.metering.stop_all() # Stop meters
        for node_id in self.created_nodes:
            self._destroy_node(node_id)
        self.created_nodes.clear()
        self.node_registry.clear()
        self.name_cache.clear()
        self.monitor_cache.clear()
        self.is_source_registry.clear()
        self.mono_registry.clear()

    # --- Public API ---
    
    def get_meter_levels(self):
        # Retry logic: Every ~2 seconds (50 ticks @ 25Hz)
        self._meter_retry_counter += 1
        if self._meter_retry_counter > 50:
            self.metering.retry_pending()
            self._meter_retry_counter = 0
            
        return self.metering.get_levels()

    def set_volume(self, strip_uid: str, volume: float):
        node_id = self.node_registry.get(strip_uid)
        if not node_id: return

        node_name = self.name_cache.get(node_id)
        if not node_name:
            pipewire_utils.set_node_volume(node_id, volume)
            return

        vol_pct = f"{int(volume * 100)}%"
        
        # Check if it is a Source (Mic) or Sink (Output/Virtual)
        is_source = self.is_source_registry.get(strip_uid, False)
        
        if is_source:
             subprocess.run(['pactl', 'set-source-volume', node_name, vol_pct], capture_output=True)
        else:
            # 1. Apply to Sink
            subprocess.run(['pactl', 'set-sink-volume', node_name, vol_pct], capture_output=True)
            # 2. Apply to Monitor (Output) for metering sync
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
            # IT'S A MIC: Mute the source directly
            subprocess.run(['pactl', 'set-source-mute', node_name, val], capture_output=True)
        else:
            # IT'S A SINK: Mute the sink AND its monitor
            subprocess.run(['pactl', 'set-sink-mute', node_name, val], capture_output=True)
            
            monitor_name = self.monitor_cache.get(node_id)
            if monitor_name:
                subprocess.run(['pactl', 'set-source-mute', monitor_name, val], capture_output=True)

    def set_mono(self, strip_uid: str, enabled: bool):
        """
        Updates the mono state and refreshes routing links if needed.
        """
        if self.mono_registry.get(strip_uid) == enabled:
            return # No change
        
        self.mono_registry[strip_uid] = enabled
        logger.info(f"Setting Mono for {strip_uid}: {enabled}")
        
        # We need to refresh all OUTPUT links originating from this strip.
        # Find all targets linked to this source
        targets_to_refresh = []
        for (src, dst) in self.link_registry.keys():
            if src == strip_uid:
                targets_to_refresh.append(dst)
        
        # Re-apply routing for each target
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

    def _resolve_metering_target_name(self, strip: Strip, node_id: Optional[int]) -> Optional[str]:
        """
        Returns the PulseAudio SOURCE NAME to record from.
        """
        # Case 1: Physical Input (Mic) -> Use device name directly
        if strip.kind == StripType.INPUT and strip.mode == StripMode.PHYSICAL:
            return strip.device_name
            
        # Case 2: Virtual Strip or Physical Output -> Use the MONITOR (Source)
        # We now look it up in the cache instead of guessing
        if node_id and node_id in self.monitor_cache:
            return self.monitor_cache[node_id]
            
        return None

    def _clean_zombie_nodes(self):
        logger.info("Cleaning up zombie nodes...")
        # USE INTERNAL=TRUE to find zombies!
        nodes = pipewire_utils.get_audio_nodes(include_internal=True)
        count = 0
        for node in nodes:
            if "Holaf_Strip_" in node.get('name', ''):
                self._destroy_node(node['id'])
                count += 1
        if count > 0:
            logger.info(f"Cleaned {count} zombie nodes.")
            time.sleep(0.2)

    def _set_system_default_sink(self, node_name: str):
        try:
            subprocess.run(['pactl', 'set-default-sink', node_name], check=True, capture_output=True)
            logger.info(f"System default sink set to: {node_name}")
        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to set default sink: {e}")

    def _create_virtual_node(self, strip: Strip) -> Optional[int]:
        node_name = f"Holaf_Strip_{strip.uid}"
        # SINK Description (what you see in Output list)
        sink_desc = f"Holaf Mix: {strip.label}"
        
        # Strategy: pactl (Primary for compatibility)
        # This registers the device correctly in the PulseAudio DB used by Discord/Teamspeak.
        # Note: The monitor will be auto-named "Monitor of Holaf Mix: ..." by Pulse.
        cmd_pactl = [
            'pactl', 'load-module', 'module-null-sink',
            f'sink_name={node_name}',
            f'sink_properties=device.description="{sink_desc}"'
        ]
        
        try:
            # We don't use check=True immediately to handle errors gracefully
            proc = subprocess.run(cmd_pactl, capture_output=True, text=True)
            if proc.returncode != 0:
                logger.warning(f"pactl failed: {proc.stderr}")
                # Retry logic or fallback could go here, but pactl is usually robust.
            else:
                logger.info(f"Created virtual sink via pactl: {node_name}")
            
            time.sleep(0.3) # Give PipeWire slightly more time to register
            
            node_id = self._find_node_id_by_name(node_name)
            if node_id:
                self.created_nodes.append(node_id)
                self.name_cache[node_id] = node_name
                
                # Retrieve the AUTO-GENERATED monitor name
                # pactl creates it, PipeWire maps it.
                # It is usually just node_name + .monitor, BUT let's fetch it safely if possible
                # or fallback to standard convention.
                
                # Try to get the real monitor name from the node properties if available
                node_info = pipewire_utils.get_node_info(node_id)
                monitor_prop = None
                if node_info and 'info' in node_info:
                    monitor_prop = node_info.get('monitor_source_name') 
                
                if monitor_prop:
                    self.monitor_cache[node_id] = monitor_prop
                else:
                    self.monitor_cache[node_id] = f"{node_name}.monitor"

                return node_id
                
        except (subprocess.CalledProcessError, FileNotFoundError) as e:
            logger.error(f"Failed to create node via pactl: {e}")
        
        return None

    def _link_physical_source_to_strip(self, strip: Strip):
        strip_node_id = self.node_registry.get(strip.uid)
        if not strip_node_id or not strip.device_name: return
        pass

    def _find_node_id_by_name(self, node_name: str) -> Optional[int]:
        # USE INTERNAL=TRUE to verify creation of our own nodes!
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
                    
                    # VITAL: Capture the real monitor name from pipewire_utils
                    if 'monitor_source_name' in node and node['monitor_source_name']:
                        self.monitor_cache[nid] = node['monitor_source_name']
                    else:
                        # Fallback if property missing (rare)
                        self.monitor_cache[nid] = f"{node['name']}.monitor"
                        
                    return nid
        return None

    def _destroy_node(self, node_id: int):
        # If created by pactl load-module, we should strictly use 'pactl unload-module'
        # BUT pipewire handles 'pw-cli destroy' on the node ID gracefully too.
        # To be safe, let's try to find if it has a module ID?
        # For simplicity in this hybrid env, pw-cli destroy works 99% of time.
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
            all_ports = [line.strip() for line in result.stdout.splitlines() if line.strip()]
            prefix = f"{node_name}:"
            matched_ports = [p for p in all_ports if p.startswith(prefix)]
            return matched_ports
        except Exception:
            return []

    def _pw_link(self, port_src: str, port_dst: str) -> bool:
        """
        Tries to link ports. Returns True if linked OR if link already exists.
        """
        try:
            # We don't check=True immediately to handle the "exists" case
            result = subprocess.run(
                ['pw-link', port_src, port_dst], 
                capture_output=True, text=True
            )
            
            if result.returncode == 0:
                return True
            
            # If it failed, check if it's because it exists
            if "exists" in result.stderr.lower():
                # We consider this a success for tracking purposes
                return True
                
            return False
        except Exception:
            return False

    def _auto_link_ports(self, src_name: str, dst_name: str, force_mono: bool = False) -> List[Tuple[str, str]]:
        """
        Helper to find compatible ports and link them. Returns list of linked ports.
        Supports downmixing to Mono if force_mono is True.
        """
        src_ports = self._get_ports_by_name(src_name, is_input=False)
        dst_ports = self._get_ports_by_name(dst_name, is_input=True)

        links_to_make = []
        src_l = next((p for p in src_ports if 'FL' in p or 'left' in p.lower()), None)
        src_r = next((p for p in src_ports if 'FR' in p or 'right' in p.lower()), None)
        dst_l = next((p for p in dst_ports if 'FL' in p or 'left' in p.lower()), None)
        dst_r = next((p for p in dst_ports if 'FR' in p or 'right' in p.lower()), None)

        if force_mono:
            # Mix BOTH source channels to BOTH dest channels
            # L->L, L->R, R->L, R->R
            if src_l:
                if dst_l: links_to_make.append((src_l, dst_l))
                if dst_r: links_to_make.append((src_l, dst_r))
            if src_r:
                if dst_l: links_to_make.append((src_r, dst_l))
                if dst_r: links_to_make.append((src_r, dst_r))
        else:
            # Standard Stereo
            if src_l and dst_l: links_to_make.append((src_l, dst_l))
            if src_r and dst_r: links_to_make.append((src_r, dst_r))
        
        # Fallback / Special Mono Handling for devices that only have 1 port
        if len(src_ports) == 1 and len(dst_ports) >= 2 and not force_mono:
             if src_ports:
                links_to_make.append((src_ports[0], dst_ports[0]))
                links_to_make.append((src_ports[0], dst_ports[1]))

        created_links = []
        for p_src, p_dst in links_to_make:
            if self._pw_link(p_src, p_dst):
                created_links.append((p_src, p_dst))
        
        return created_links

    def _create_link(self, source_uid: str, target_uid: str):
        src_id = self.node_registry.get(source_uid)
        dst_id = self.node_registry.get(target_uid)
        
        if not src_id or not dst_id: return
        
        src_name = self._get_node_name(src_id)
        dst_name = self._get_node_name(dst_id)
        
        if not src_name or not dst_name: return

        is_mono = self.mono_registry.get(source_uid, False)
        created_links = self._auto_link_ports(src_name, dst_name, force_mono=is_mono)
        
        if created_links:
            self.link_registry[(source_uid, target_uid)] = created_links

    def _destroy_link(self, source_uid: str, target_uid: str):
        # 1. Try to get known links
        links = self.link_registry.pop((source_uid, target_uid), [])
        
        # 2. Force Unlink fallback
        if not links:
            src_id = self.node_registry.get(source_uid)
            dst_id = self.node_registry.get(target_uid)
            if src_id and dst_id:
                src_name = self._get_node_name(src_id)
                dst_name = self._get_node_name(dst_id)
                if src_name and dst_name:
                    src_ports = self._get_ports_by_name(src_name, is_input=False)
                    dst_ports = self._get_ports_by_name(dst_name, is_input=True)
                    for s in src_ports:
                        for d in dst_ports:
                            subprocess.run(['pw-link', '-d', s, d], capture_output=True)
                    return

        # 3. Standard unlink
        for (p_src, p_dst) in links:
             subprocess.run(['pw-link', '-d', p_src, p_dst], capture_output=True)