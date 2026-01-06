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
        self.link_registry.clear()
        
        # 1. Create Nodes (Virtual Strips)
        for strip in strips:
            node_id = None
            node_name = None

            if strip.kind == StripType.OUTPUT and strip.mode == StripMode.PHYSICAL:
                # Direct hardware mapping
                node_id = self._find_physical_node(strip)
                if node_id:
                     node_name = self._get_node_name(node_id)
            else:
                # Virtual Node creation
                node_id = self._create_virtual_node(strip)
                if node_id:
                    node_name = self.name_cache.get(node_id)
            
            if node_id:
                self.node_registry[strip.uid] = node_id
                self.set_volume(strip.uid, strip.volume)
                self.set_mute(strip.uid, strip.mute)
                
                # --- METERING SETUP (Non-Blocking via Threaded Metering) ---
                target_name = self._resolve_metering_target_name(strip, node_name)
                
                if target_name:
                    # This call is now non-blocking (spawns a thread)
                    self.metering.start_monitoring(strip.uid, target_name)
                else:
                    logger.warning(f"Metering: Could not resolve target Name for {strip.label}")
            else:
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
        if node_name:
            vol_pct = f"{int(volume * 100)}%"
            # 1. Apply to Sink (Input)
            subprocess.run(['pactl', 'set-sink-volume', node_name, vol_pct], capture_output=True)
            # 2. Apply to Monitor (Output) - Fixes hardware volume control
            monitor_name = f"{node_name}.monitor"
            subprocess.run(['pactl', 'set-source-volume', monitor_name, vol_pct], capture_output=True)
        else:
            pipewire_utils.set_node_volume(node_id, volume)

    def set_mute(self, strip_uid: str, muted: bool):
        node_id = self.node_registry.get(strip_uid)
        if not node_id: return

        node_name = self.name_cache.get(node_id)
        val = "1" if muted else "0"

        if node_name:
            subprocess.run(['pactl', 'set-sink-mute', node_name, val], capture_output=True)
            monitor_name = f"{node_name}.monitor"
            subprocess.run(['pactl', 'set-source-mute', monitor_name, val], capture_output=True)
        else:
            pipewire_utils.toggle_node_mute(node_id, muted)

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

    def _resolve_metering_target_name(self, strip: Strip, node_name: Optional[str]) -> Optional[str]:
        """
        Returns the PulseAudio SOURCE NAME to record from.
        """
        # Case 1: Physical Input (Mic) -> Use device name directly
        if strip.kind == StripType.INPUT and strip.mode == StripMode.PHYSICAL:
            return strip.device_name
            
        # Case 2: Virtual Strip (Sink) -> We need the .monitor
        if node_name:
            return f"{node_name}.monitor"
            
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
        description = f"Holaf: {strip.label}"
        
        # Strategy A: pw-cli (Preferred)
        props = (
            f"{{ factory.name=support.null-audio-sink, "
            f"node.name=\"{node_name}\", "
            f"node.description=\"{description}\", "
            f"media.class=Audio/Sink, "
            f"object.linger=true, "
            f"audio.position=[FL,FR] }}"
        )
        cmd_pw = ['pw-cli', 'create-node', 'adapter', props]
        
        try:
            subprocess.run(cmd_pw, check=True, capture_output=True)
            time.sleep(0.2)
            
            node_id = self._find_node_id_by_name(node_name)
            if node_id:
                self.created_nodes.append(node_id)
                self.name_cache[node_id] = node_name
                logger.info(f"Created virtual node '{strip.label}' via pw-cli (ID: {node_id})")
                return node_id
        except subprocess.CalledProcessError as e:
            logger.warning(f"pw-cli failed ({e}), trying pactl fallback...")

        # Strategy B: pactl (Fallback)
        cmd_pactl = [
            'pactl', 'load-module', 'module-null-sink',
            f'sink_name={node_name}',
            f'sink_properties=device.description="{description}"' 
        ]
        
        try:
            subprocess.run(cmd_pactl, check=True, capture_output=True)
            time.sleep(0.2)
            node_id = self._find_node_id_by_name(node_name)
            if node_id:
                self.created_nodes.append(node_id)
                self.name_cache[node_id] = node_name
                logger.info(f"Created virtual node '{strip.label}' via pactl (ID: {node_id})")
                return node_id
        except (subprocess.CalledProcessError, FileNotFoundError):
            logger.error("Both pw-cli and pactl strategies failed.")
        
        return None

    def _link_physical_source_to_strip(self, strip: Strip):
        strip_node_id = self.node_registry.get(strip.uid)
        if not strip_node_id or not strip.device_name: return

        source_id = self._find_node_id_by_name(strip.device_name)
        if not source_id:
            logger.warning(f"Could not find physical source: {strip.device_name}")
            return
            
        src_name = self._get_node_name(source_id)
        dst_name = self._get_node_name(strip_node_id)
        
        if src_name and dst_name:
             self._auto_link_ports(src_name, dst_name, is_source_input=False)

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
                    self.name_cache[node['id']] = node['name']
                    return node['id']
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

    def _auto_link_ports(self, src_name: str, dst_name: str, is_source_input: bool = False) -> List[Tuple[str, str]]:
        """
        Helper to find compatible ports and link them. Returns list of linked ports.
        """
        src_ports = self._get_ports_by_name(src_name, is_input=False)
        dst_ports = self._get_ports_by_name(dst_name, is_input=True)

        links_to_make = []
        src_l = next((p for p in src_ports if 'FL' in p or 'left' in p.lower()), None)
        src_r = next((p for p in src_ports if 'FR' in p or 'right' in p.lower()), None)
        dst_l = next((p for p in dst_ports if 'FL' in p or 'left' in p.lower()), None)
        dst_r = next((p for p in dst_ports if 'FR' in p or 'right' in p.lower()), None)

        if src_l and dst_l: links_to_make.append((src_l, dst_l))
        if src_r and dst_r: links_to_make.append((src_r, dst_r))
        
        # Mono handling
        if len(src_ports) == 1 and len(dst_ports) >= 2:
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
        # Check if already tracked, BUT do not return immediately, 
        # because maybe the physical link was deleted externally.
        # We enforce the link.
        
        src_name = self._get_node_name(src_id)
        dst_name = self._get_node_name(dst_id)
        
        if not src_name or not dst_name: return

        created_links = self._auto_link_ports(src_name, dst_name)
        if created_links:
            self.link_registry[(source_uid, target_uid)] = created_links

    def _destroy_link(self, source_uid: str, target_uid: str):
        # 1. Try to get known links
        links = self.link_registry.pop((source_uid, target_uid), [])
        
        # 2. If registry was empty (Desync bug), Try to resolve ports anyway and Force Unlink
        if not links:
            src_id = self.node_registry.get(source_uid)
            dst_id = self.node_registry.get(target_uid)
            if src_id and dst_id:
                src_name = self._get_node_name(src_id)
                dst_name = self._get_node_name(dst_id)
                if src_name and dst_name:
                    # We recalculate what 'would' be linked
                    src_ports = self._get_ports_by_name(src_name, is_input=False)
                    dst_ports = self._get_ports_by_name(dst_name, is_input=True)
                    # We brute-force unlink all combinations to be safe
                    for s in src_ports:
                        for d in dst_ports:
                            subprocess.run(['pw-link', '-d', s, d], capture_output=True)
                    return

        # 3. Standard unlink
        for (p_src, p_dst) in links:
             subprocess.run(['pw-link', '-d', p_src, p_dst], capture_output=True)