# Spec — Réécriture Rust du cœur de Holaf-Mix (daemon `holafd` + UI Python IPC)

## 1. Arborescence projet Rust
Workspace cargo dans `/projects/Holaf-Mix/rust/` (même dépôt git, pas de repo séparé) : l'UI Python et le daemon évoluent ensemble et sont packagés ensemble (build.sh, packaging/).
```
rust/
  Cargo.toml                 # [workspace] members = crates/*
  crates/holaf-core/         # modèle + constantes FX + migrations + formatage SPA (PUR, 0 dépendance PipeWire)
  crates/holaf-pw/           # couche pipewire-rs (GraphBackend trait + impl PipeWire + Fake pour tests)
  crates/holaf-ipc/          # types serde du protocole IPC (enveloppe req/resp/event)
  crates/holafd/             # binaire daemon : machine à états + thread PipeWire + serveur socket Unix
  crates/holafctl/           # (jalon J3+) client CLI de debug
```
Dépendances prévues : pipewire = "0.8", spa = "0.8", serde/serde_json, indexmap, thiserror, log/env_logger, socket Unix via std, pas de runtime async (threads + channels).

## 2. Modèle de données Rust (source de vérité = daemon)
holaf-core porte : Strip, StripType, StripMode, état FX, tables DEFAULT_EFFECT_PARAMS/BYPASS_PARAMS, migrations from_dict, lecture/écriture config.json. L'UI Python n'envoie que des ordres (intent).
Structs (clés JSON snake_case, compatibles à l'octet avec config.json actuel):
- Strip { uid, label, kind: input|output, mode: physical|virtual, volume: f32 (1.0), mute: bool, is_mono: bool, routes: Vec<String>, device_name: Option<String>, assigned_apps: Vec<String>, is_default: bool, midi mappings, effects: IndexMap<EffectKey, EffectState> }
- EffectKey enum { Gate, NoiseCancel, Eq, Tube, Compressor } serde snake_case
- EffectState { active: bool, params: IndexMap<String, f32> }
IndexMap préserve l'ordre canonique des ports LADSPA (HashMap briserait les golden tests).
Migrations portées à l'identique depuis Strip.from_dict (strip_model.py): EQ 50Hz→"50Hz gain (low shelving)"…; comp "Threshold (dB)"→"Threshold level (dB)", "Attack (ms)"→"Attack time (ms)", "Release (ms)"→"Release time (ms)", "Makeup Gain (dB)"→"Makeup gain (dB)"; gate "Release (ms)"→"Decay (ms)"; tube "Distortion level"→"Drive", "Distortion character"→"Tape--Tube Blend"; RNNoise suppression de "Model". Ordre critique: migrations AVANT remplissage des defaults.
Config IO: reprend settings.py (écriture atomique tmp+os.replace, résolution chemin source tree vs frozen).

## 3. Couche PipeWire (mapping audio_engine.py → pipewire-rs)
- filter-chain LADSPA: charger libpipewire-module-filter-chain dans le daemon via API core (args = chaîne SPA-JSON identique à fx_config_json)
- set-param Props: Node::set_param(ParamType::Props) sur le proxy du node → hot-reload
- strips virtuels: pw_core_create_object factory adapter/support.null-audio-sink (props node.name, media.class, node.passive…)
- volume/mute: set_param Props (channelVolumes, mute) sur node ET son monitor
- default sink: pw_metadata default.audio.sink
- liens: factory link-factory (link.output.node/port, link.input.node/port)
- discovery/cleanup zombies: registry globals, identifier nodes "Holaf*", détruire via proxy
Props anti-WirePlumber à reproduire exactement: node.passive=true, stream.dont-remix=true, audio.channels=2, audio.position=[FL,FR], media.role=comms (output), target.object="<master>" (input: capture; output: playback), media.class=Audio/Source|Sink; node.name=Holaf_FX_<uid>.
Fonctions pures à porter: _has_active_fx, _build_fx_graph (dual-mono _L/_R vs stereo single node via STEREO_PLUGINS), _build_fx_props, cascade hot-reload→reload strip→full restart, _create_link/_destroy_link/_auto_link_ports (routage exclusif, retries 5×0.1s, mono L→L+L, mono-source→stéréo), _link_physical_source_to_strip.
5 incertitudes API pipewire-rs à lever au J1 (spike): (1) chargement module filter-chain dans le serveur exposé par pipewire-rs? sinon shim FFI; (2) Node::set_param avec Pod depuis texte SPA-JSON; (3) création Link via factory link-factory exposée?; (4) pw_metadata wrapper?; (5) compat version crate 0.8.x vs libpipewire système (~1.2).

## 4. Contrat IPC
Socket Unix + NDJSON (un JSON par ligne). Requêtes {"id":1,"method":"…","params":{…}}; réponses {"id":1,"ok":true,"result":{…}} / {"ok":false,"error":{"code":"…","message":"…"}}; événements {"event":"…","params":{…}}.
Méthodes: engine.apply_snapshot {strips:[…]} (=start_engine), engine.stop, engine.status, strip.set_volume {uid,value}, strip.set_mute {uid,muted}, strip.set_mono {uid,mono}, fx.set_active {uid,effect,active}, fx.set_params {uid,effect,params}, routing.set {source_uid,target_uid,active}, routing.list, system.set_default_sink {uid}, device.list, app.list, app.move {app_id,sink_name}, meter.get {uid} (optionnel J5).
Événements: state_changed, device_list_changed, strip_fx_ready {uid}.
Concurrence: thread pw (mainloop) + thread IPC; ordres → mpsc channel consommé par thread pw (sérialisé, équiv. RLock Python); événements → broadcast clients. Pas de runtime async.

## 5. Mapping FX (recopie exacte des constantes Python)
Chaîne par strip: gate → noise_cancel → eq → tube → compressor. INPUT: 5 effets; OUTPUT: eq, tube, compressor. Plugins absents du disque → ignorés.
- gate (gate_1410, dual-mono): LF key filter (Hz) 33.6, HF key filter (Hz) 23520.0, Threshold (dB) −70.0, Attack (ms) 250.008, Hold (ms) 1500.5, Decay (ms) 2001.0, Range (dB) −90.0, "Output select (-1 = key listen, 0 = gate, 1 = bypass)" 0.0 [FILTRÉ, contient =]. BYPASS: Threshold −100.0, Range 0.0.
- noise_cancel (librnnoise_ladspa/noise_suppressor_stereo, STÉRÉO: Input (L)/(R), Output (L)/(R)): VAD Threshold (%) 74.25, VAD Grace Period (ms) 500, Retroactive VAD Grace (ms) 100, Dry Mix 0.0. BYPASS: Dry Mix 1.0.
- eq (mbeq_1197, dual-mono): 15 bandes "50Hz gain (low shelving)", "100Hz gain"…"20000Hz gain" = 0.0. BYPASS: toutes 0.0.
- tube (tap_tubewarmth, dual-mono): Drive 2.575 (0.1-10), Tape--Tube Blend 10.0 (−10..10). BYPASS: Drive 0.1.
- compressor (sc4_1882, STÉRÉO: Left/Right input/output): RMS/peak 0.0, Attack time (ms) 101.125, Release time (ms) 401.0, Threshold level (dB) 0.0, Ratio (1:n) 1.0, Knee radius (dB) 3.25, Makeup gain (dB) 0.0. BYPASS: Ratio (1:n) 1.0.
Règle de fusion quand inactif: params = defaults ⊕ bypass (TOUS les ports reçoivent une valeur explicite).
Formatage SPA-JSON: { "key" = value … }, clés contenant '=' exclues, pas de inf/nan.

## 6. VU meters
Garder metering Python (sounddevice) jusqu'à J4 au moins. Daemon expose les noms effectifs (fx_source_names/monitor/device) dans engine.status/strip_fx_ready. Option native documentée J5+: lire Props audio.peak ou pw_stream passifs sur monitors.

## 7. Jalons (estimations heures agent)
J0 (6-10h): workspace + holaf-core complet (model, enums, tables, migrations, config IO atomique, formateurs SPA) + tests unitaires. 0 dépendance PipeWire. Livrable: cargo test vert.
J1 (10-16h): spike pipewire-rs holaf-spike: connexion, discovery, UNE chaine filter-chain + contrôle, set-param, null-sink, un lien, volume/mute, metadata default sink. Léve les 5 incertitudes §3. Machine réelle requise.
J2 (14-20h): holaf-pw complet derrière trait GraphBackend (+FakeBackend), reconcilieur de graphe, routing exclusif, mono, restart local/full, hot-reload, cleanup zombies. Tests Fake.
J3 (10-16h): holaf-ipc, serveur socket NDJSON, contrat complet, holafctl CLI. Tests IPC.
J4 (12-18h): IpcAudioEngine Python (même surface publique qu'AudioEngine → main_window.py quasi inchangé), launcher daemon, bascule config.
J5 (8-12h): hotplug devices, retries/timeouts, packaging daemon+.desktop, metering natif optionnel, doc.
Total ≈ 60-92 h.

## 8. Stratégie de test sans PipeWire
Dès J0 en pur Rust: formateurs SPA golden-testés (fixtures capturées des fonctions Python pures), tables byte-à-byte, migrations from_dict (vieux formats bool/Model/noms courts), config IO rond-tri, serde round-trip, logique routing en mémoire (heuristiques _auto_link_ports sur noms simulés), décisions hot/reload/restart via FakeBackend, IPC NDJSON parse/encode dispatch.
Mode daemon --check-config: charge config.json, applique migrations, affiche le plan de graphe sans se connecter à PipeWire.
Seul holaf-pw réel non testable sans serveur → J1/J2 sur machine utilisateur.
