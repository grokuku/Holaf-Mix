import subprocess
import time
import logging
from typing import Dict, List, Optional, Tuple
import pipewire_utils
from src.models.strip_model import Strip, StripType, StripMode

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

    def start_engine(self, strips: List[Strip]):
        logger.info("Starting Audio Engine...")
        
        # 0. CLEANUP: Remove leftover nodes from previous crashes/restarts
        self._clean_zombie_nodes()

        self.node_registry.clear()
        self.name_cache.clear()
        self.link_registry.clear()
        
        # 1. Create Nodes (Virtual Strips)
        for strip in strips:
            node_id = None
            if strip.kind == StripType.OUTPUT and strip.mode == StripMode.PHYSICAL:
                # Direct hardware mapping
                node_id = self._find_physical_node(strip)
            else:
                # Virtual Node creation
                node_id = self._create_virtual_node(strip)
            
            if node_id:
                self.node_registry[strip.uid] = node_id
                self.set_volume(strip.uid, strip.volume)
                self.set_mute(strip.uid, strip.mute)
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
        for node_id in self.created_nodes:
            self._destroy_node(node_id)
        self.created_nodes.clear()
        self.node_registry.clear()
        self.name_cache.clear()

    # --- Public API ---

    def set_volume(self, strip_uid: str, volume: float):
        node_id = self.node_registry.get(strip_uid)
        if not node_id: return

        node_name = self.name_cache.get(node_id)
        if node_name:
            vol_pct = f"{int(volume * 100)}%"
            try:
                subprocess.run(['pactl', 'set-sink-volume', node_name, vol_pct], check=True, capture_output=True)
                return
            except subprocess.CalledProcessError:
                pass 
        pipewire_utils.set_node_volume(node_id, volume)

    def set_mute(self, strip_uid: str, muted: bool):
        node_id = self.node_registry.get(strip_uid)
        if not node_id: return

        node_name = self.name_cache.get(node_id)
        if node_name:
            val = "1" if muted else "0"
            try:
                subprocess.run(['pactl', 'set-sink-mute', node_name, val], check=True, capture_output=True)
            except subprocess.CalledProcessError:
                try:
                    subprocess.run(['pactl', 'set-source-mute', node_name, val], check=True, capture_output=True)
                except subprocess.CalledProcessError:
                    pipewire_utils.toggle_node_mute(node_id, muted)
        else:
            pipewire_utils.toggle_node_mute(node_id, muted)

    def update_routing(self, source_uid: str, target_uid: str, active: bool):
        if active:
            self._create_link(source_uid, target_uid)
        else:
            self._destroy_link(source_uid, target_uid)

    # --- Internal Logic ---

    def _clean_zombie_nodes(self):
        """Scans for existing Holaf nodes from previous sessions and destroys them."""
        logger.info("Cleaning up zombie nodes...")
        nodes = pipewire_utils.get_audio_nodes()
        count = 0
        for node in nodes:
            # Check for our signature in the name
            if "Holaf_Strip_" in node.get('name', ''):
                self._destroy_node(node['id'])
                count += 1
        if count > 0:
            logger.info(f"Cleaned {count} zombie nodes.")
            # Give PipeWire a moment to process deletions
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
        
        # --- STRATEGY 1: pw-cli (Native PipeWire) - PRIMARY ---
        # We use this first because it handles strings/descriptions way better than pactl
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

        # --- STRATEGY 2: pactl (PulseAudio Compatibility) - FALLBACK ---
        # Note: Quoting descriptions with spaces is tricky here.
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
             src_ports = self._get_ports_by_name(src_name, is_input=False)
             dst_ports = self._get_ports_by_name(dst_name, is_input=True)
             
             links_to_make = []
             
             src_l = next((p for p in src_ports if 'FL' in p or 'left' in p.lower()), None)
             src_r = next((p for p in src_ports if 'FR' in p or 'right' in p.lower()), None)
             dst_l = next((p for p in dst_ports if 'FL' in p or 'left' in p.lower()), None)
             dst_r = next((p for p in dst_ports if 'FR' in p or 'right' in p.lower()), None)
             
             if src_l and dst_l: links_to_make.append((src_l, dst_l))
             if src_r and dst_r: links_to_make.append((src_r, dst_r))
             
             if len(src_ports) == 1 and len(dst_ports) >= 2:
                 if src_ports:
                    links_to_make.append((src_ports[0], dst_ports[0]))
                    links_to_make.append((src_ports[0], dst_ports[1]))
                 
             for p_src, p_dst in links_to_make:
                 self._pw_link(p_src, p_dst)

    def _find_node_id_by_name(self, node_name: str) -> Optional[int]:
        nodes = pipewire_utils.get_audio_nodes()
        for node in nodes:
            if node.get('name') == node_name:
                return node['id']
        return None

    def _find_physical_node(self, strip: Strip) -> Optional[int]:
        nodes = pipewire_utils.get_audio_nodes()
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
        try:
            subprocess.run(['pw-link', port_src, port_dst], check=True, capture_output=True)
            return True
        except subprocess.CalledProcessError:
            return False

    def _create_link(self, source_uid: str, target_uid: str):
        src_id = self.node_registry.get(source_uid)
        dst_id = self.node_registry.get(target_uid)
        
        if not src_id or not dst_id: return
        if (source_uid, target_uid) in self.link_registry: return

        src_name = self._get_node_name(src_id)
        dst_name = self._get_node_name(dst_id)
        
        if not src_name or not dst_name: return

        src_ports = self._get_ports_by_name(src_name, is_input=False)
        dst_ports = self._get_ports_by_name(dst_name, is_input=True)

        links_to_make = []
        src_l = next((p for p in src_ports if 'FL' in p or 'left' in p.lower()), None)
        src_r = next((p for p in src_ports if 'FR' in p or 'right' in p.lower()), None)
        dst_l = next((p for p in dst_ports if 'FL' in p or 'left' in p.lower()), None)
        dst_r = next((p for p in dst_ports if 'FR' in p or 'right' in p.lower()), None)

        if src_l and dst_l: links_to_make.append((src_l, dst_l))
        if src_r and dst_r: links_to_make.append((src_r, dst_r))
        
        created_links = []
        for p_src, p_dst in links_to_make:
            if self._pw_link(p_src, p_dst):
                created_links.append((p_src, p_dst))
                
        if created_links:
            self.link_registry[(source_uid, target_uid)] = created_links

    def _destroy_link(self, source_uid: str, target_uid: str):
        links = self.link_registry.pop((source_uid, target_uid), [])
        for (p_src, p_dst) in links:
             subprocess.run(['pw-link', '-d', p_src, p_dst], capture_output=True)