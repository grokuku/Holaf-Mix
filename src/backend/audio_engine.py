import subprocess
import time
import logging
from typing import Dict, List, Optional, Tuple
import pipewire_utils  # CORRECTION: Import direct depuis la racine
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
        self.node_registry.clear()
        self.name_cache.clear()
        self.link_registry.clear()
        
        # 1. Create/Find Nodes
        for strip in strips:
            node_id = None
            if strip.mode == StripMode.VIRTUAL:
                node_id = self._create_virtual_node(strip)
            else:
                node_id = self._find_physical_node(strip)
            
            if node_id:
                self.node_registry[strip.uid] = node_id
                # Mise à jour de l'état initial
                self.set_volume(strip.uid, strip.volume)
                self.set_mute(strip.uid, strip.mute)
            else:
                logger.warning(f"Could not initialize node for strip: {strip.label}")

        # 2. Restore Routing
        input_strips = [s for s in strips if s.kind == StripType.INPUT]
        for inp in input_strips:
            source_uid = inp.uid
            if source_uid not in self.node_registry:
                continue
            for target_uid in inp.routes:
                if target_uid in self.node_registry:
                    self.update_routing(source_uid, target_uid, active=True)

        # 3. Set Default Sink (Feature Request)
        # We look for a strip labeled "Desktop" to set as system default
        desktop_strip = next((s for s in strips if s.label.lower() == "desktop" and s.kind == StripType.INPUT), None)
        if desktop_strip and desktop_strip.uid in self.node_registry:
            node_name = self.name_cache.get(self.node_registry[desktop_strip.uid])
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
        if node_id:
            pipewire_utils.set_node_volume(node_id, volume)

    def set_mute(self, strip_uid: str, muted: bool):
        node_id = self.node_registry.get(strip_uid)
        if not node_id: return

        # FIX: Use pactl for named nodes (more reliable for KDE/Pulse clients)
        node_name = self.name_cache.get(node_id)
        if node_name:
            # pactl set-sink-mute works for both sinks and sources usually if named correctly,
            # but let's be specific or fallback.
            # "0" for unmute, "1" for mute
            val = "1" if muted else "0"
            try:
                # Try sink first
                subprocess.run(['pactl', 'set-sink-mute', node_name, val], check=True, capture_output=True)
            except subprocess.CalledProcessError:
                try:
                    # Try source if sink failed
                    subprocess.run(['pactl', 'set-source-mute', node_name, val], check=True, capture_output=True)
                except subprocess.CalledProcessError:
                    # Fallback to low-level pw-cli
                    pipewire_utils.toggle_node_mute(node_id, muted)
        else:
            # Fallback for unnamed nodes
            pipewire_utils.toggle_node_mute(node_id, muted)

    def update_routing(self, source_uid: str, target_uid: str, active: bool):
        if active:
            self._create_link(source_uid, target_uid)
        else:
            self._destroy_link(source_uid, target_uid)

    # --- Internal Logic ---

    def _set_system_default_sink(self, node_name: str):
        """Sets the given node name as the default system audio output."""
        try:
            subprocess.run(['pactl', 'set-default-sink', node_name], check=True, capture_output=True)
            logger.info(f"System default sink set to: {node_name}")
        except subprocess.CalledProcessError as e:
            logger.error(f"Failed to set default sink: {e}")

    def _create_virtual_node(self, strip: Strip) -> Optional[int]:
        node_name = f"Holaf_Strip_{strip.uid}"
        description = f"Holaf: {strip.label}"
        
        # --- STRATEGY 1: pactl (PulseAudio Compatibility) ---
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
            logger.warning("pactl failed or not found. Falling back to pw-cli.")

        # --- STRATEGY 2: pw-cli (Native PipeWire) ---
        props = (
            f"{{ factory.name=support.null-audio-sink, "
            f"node.name={node_name}, "
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
            logger.error(f"Failed to create node for {strip.label}: {e}")
        
        return None

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

        # 1. Exact Match
        if strip.device_name:
            for node in candidates:
                if node['name'] == strip.device_name:
                    self.name_cache[node['id']] = node['name']
                    return node['id']
        
        # 2. Heuristic Match
        for node in candidates:
            if "Holaf_Strip" not in node['name']:
                logger.info(f"Auto-assigned physical device '{node['description']}'")
                strip.device_name = node['name']
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
        except Exception as e:
            logger.error(f"Error listing ports: {e}")
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

        # 1. Récupérer les Noms
        src_name = self._get_node_name(src_id)
        dst_name = self._get_node_name(dst_id)
        
        if not src_name or not dst_name:
            logger.error(f"Cannot link: Could not resolve node names for IDs {src_id}->{dst_id}")
            return

        # 2. Récupérer les Ports par NOM
        src_ports = self._get_ports_by_name(src_name, is_input=False)
        dst_ports = self._get_ports_by_name(dst_name, is_input=True)

        logger.info(f"Linking {src_name} -> {dst_name}")

        # 3. Matching
        src_l = next((p for p in src_ports if 'FL' in p or 'left' in p.lower()), None)
        src_r = next((p for p in src_ports if 'FR' in p or 'right' in p.lower()), None)
        dst_l = next((p for p in dst_ports if 'FL' in p or 'left' in p.lower()), None)
        dst_r = next((p for p in dst_ports if 'FR' in p or 'right' in p.lower()), None)

        links_to_make = []

        if src_l and dst_l: links_to_make.append((src_l, dst_l))
        if src_r and dst_r: links_to_make.append((src_r, dst_r))
        
        # Fallback Index
        if not links_to_make and src_ports and dst_ports:
            min_len = min(len(src_ports), len(dst_ports))
            for i in range(min_len):
                links_to_make.append((src_ports[i], dst_ports[i]))

        created_links = []
        for p_src, p_dst in links_to_make:
            if self._pw_link(p_src, p_dst):
                created_links.append((p_src, p_dst))
                
        if created_links:
            self.link_registry[(source_uid, target_uid)] = created_links
            logger.info(f"Successfully linked: {created_links}")
        else:
            logger.error("Failed to link strips. No matching ports.")

    def _destroy_link(self, source_uid: str, target_uid: str):
        links = self.link_registry.pop((source_uid, target_uid), [])
        for (p_src, p_dst) in links:
             subprocess.run(['pw-link', '-d', p_src, p_dst], capture_output=True)
        if links:
            logger.info(f"Unlinked {source_uid} -> {target_uid}")

if __name__ == "__main__":
    print("Audio Engine Module.")