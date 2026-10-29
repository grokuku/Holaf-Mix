# J1 — Reconnaissance (phase 1) : environnement PipeWire headless + incertitudes API pipewire-rs

Date: recon effectuée dans le conteneur Debian 12 de /projects/Holaf-Mix (root, sans carte son).
Base: docs/rust-rewrite-spec.md §3. Cette phase valide le postulat du JALON 1 (spike pipewire-rs) :
on peut développer/tester le backend PipeWire réel headless, sans la machine utilisateur.

---

## A. Environnement installé et daemon headless

### Paquets (Debian 12 bookworm)
Tous les paquets visés existent sur bookworm et sont installés :

| Paquet | Version | Rôle |
|---|---|---|
| pipewire, pipewire-bin, libpipewire-0.3-dev | **0.3.65-3+deb12u1** | daemon + headers |
| wireplumber | 0.4.13-1 | session manager |
| ladspa-sdk | 1.17-1 | outils `analyseplugin`/`applyplugin` |
| swh-plugins | 0.4.17-2 | **mbeq_1197, sc4_1882, gate_1410** |
| tap-plugins | 1.0.0-1 | **tap_tubewarmth** |
| lsp-plugins | 1.2.5-1 | présent aussi (lsp-plugins-ladspa) |
| pkg-config | 1.8.1-1 | requis par pipewire-sys (system-deps) |
| clang / libclang-dev | 14 | requis par pipewire-sys (bindgen) |

**Absent de Debian 12** : `librnnoise_ladspa` / `noise_suppressor_stereo` (aucun paquet rnnoise-ladspa
dans les dépôts). Comme dans la spec §5, un plugin absent du disque = effet ignoré : à gérer en
spike (grep `os.path.exists` / `FX_PLUGIN_MAP` — comportement identique côté Rust, skip silencieux).

### Fichiers LADSPA vérifiés (chemins exacts)
```
/usr/lib/ladspa/gate_1410.so        (label LADSPA = "gate")
/usr/lib/ladspa/mbeq_1197.so        (label LADSPA = "mbeq")     <-- PAS "mbeq_1197" !
/usr/lib/ladspa/sc4_1882.so         (label LADSPA = "sc4")      <-- PAS "sc4_1882" !
/usr/lib/ladspa/tap_tubewarmth.so   (label LADSPA = "tap_tubewarmth")
```
Vérifié avec `analyseplugin`. Important : dans les args filter-chain, `plugin` = **chemin absolu du .so**
et `label` = label LADSPA court (4e élément de `FX_PLUGIN_MAP` du code Python). Un mauvais label donne
`descriptor_load(): cannot find label ...` (erreur réelle observée).

### Lancement headless reproductible
```bash
bash /opt/holaf-headless/start-pipewire-headless.sh     # écrit lors de la recon
```
Le script fait (exactement ce qui a été validé) :
```
export XDG_RUNTIME_DIR=/tmp/pw-runtime PIPEWIRE_RUNTIME_DIR=/tmp/pw-runtime   (mkdir+chmod 700)
dbus-daemon --session --address=unix:path=$XDG_RUNTIME_DIR/bus --fork          # requis par wireplumber
setsid nohup pipewire &      → pipewire.log
setsid nohup wireplumber &   → wireplumber.log
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus
```

### Résultat (blocages résolus)
- **Le daemon répond** : `pw-dump` retourne Core (cookie, version 0.3.65) + modules + factories.
  `pw-cli ls Node` montre les nœuds drivers (`Dummy-Driver`, `Freewheel-Driver`) créés par le core.
- **Sans carte son**, pas de nœuds hardware (normal). `wireplumber` tourne et crée les métadonnées
  `settings`, `default`, `route-settings`.
- **dbus** : pipewire démarre sans dbus (seuls les modules optionnels rtkit/portal échouent, non fatals);
  **wireplumber exige une session bus** → on lance un `dbus-daemon --session` sur un socket dans
  `XDG_RUNTIME_DIR` (pas de systemd, pas de $DISPLAY pour l'autolaunch). C'est la seule astuce nécessaire.
- **memlock/shm** : aucune erreur ; `pw-dump`, création de nœuds, liens et metadata fonctionnent
  (pas besoin de `ulimit -l unlimited` ni de `PIPEWIRE_NO_SHM`). La seule trace `acquire_rt ...
  not implemented` est bénigne (pas de rtkit en conteneur → pas de priorité temps réel, inutile ici).
- **Piège shell** : `pkill -f pipewire` dans une commande `bash -c` se suicide (la ligne de commande
  contient "pipewire"). Toujours utiliser `pkill -x` / `pgrep -x`.

### Preuves de fonctionnement (validées en live)
1. **Null-sink** : `pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=... }'`
   → nœud + ports `playback_FL/FR` + `monitor_FL/FR`. (Attention : un client éphémère détruit ses objets
   à la déconnexion → pour tester, garder la session client ouverte — c'est le cas d'usage réel de holafd.)
   Wireplumber a adopté le sink → `default.audio.sink` mis automatiquement.
2. **Filter-chain LADSPA** (mbeq_1197 dual-mono) via un pw-cli interactif persistant :
   `load-module libpipewire-module-filter-chain { node.name=... filter.graph={...} capture.props={...} playback.props={...} }`
   → nœuds `input.Holaf_FX_test` / `output.Holaf_FX_test` visibles côté daemon.
   **Réalité de l'hébergement** : `pw-cli` charge le module dans SON contexte local
   (`pw_context_load_module`, symble visible dans le binaire), pas dans le daemon. Le graphe LADSPA vit
   dans le processus hôte ; seuls les streams capture/playback sont exportés vers le daemon. Voir §B-a
   pour la traduction en architecture Rust.

---

## B. Réponses aux 5 incertitudes API (spec §3), par lecture du source pipewire-rs 0.8.0

Sources téléchargées (cargo, réseau OK) :
```
~/.cargo/registry/src/index.crates.io-*/pipewire-0.8.0/
~/.cargo/registry/src/index.crates.io-*/pipewire-sys-0.8.0/
~/.cargo/registry/src/index.crates.io-*/libspa-0.8.0/      (dép. = crate "libspa", PAS "spa")
~/.cargo/registry/src/index.crates.io-*/libspa-sys-0.8.0/
```
Note : la spec écrivait `spa = "0.8"` — ce crate n'existe pas à cette version (dernier "spa" = 0.5, sans rapport).
L'API spa est fournie par `libspa 0.8`, ré-exportée par pipewire (`pipewire::spa`, `pipewire::spa_sys`).
Le projet temporaire de probe est `/tmp/api-probe` (deps `pipewire="0.8"`, `once_cell`).

### a. Chargement d'un module (filter-chain) depuis une API client → NON exposé ; FFI minimal requis
- pipewire-rs **n'expose aucune fonction de chargement de module** : `grep -r load_module src/` = 0.
- `core.rs` (`struct pw_core_methods`, header C) n'a que : add_listener, hello, sync, pong,
  error, get_registry, **create_object**, destroy. **Pas de méthode client load_module** au protocole.
- `src/module.rs` (`Module`, `ModuleInfoRef`) est un **proxy de lecture** des globals Module du
  registre distant (id, name, filename, args, props) — pas un loader.
- La fonction C de chargement est serveur/local : `impl-module.h:80`
  `struct pw_impl_module *pw_context_load_module(struct pw_context *context, const char *name, const char *args, struct pw_properties *properties);`
  pipewire-sys la **binde** (`pipewire_sys::pw_context_load_module` présent dans les bindings générés).
- `pipewire::context::Context` enveloppe un vrai `pw_context` local (`pw_context_new`, source context.rs)
  et `context.as_raw_ptr() -> *mut pw_sys::pw_context` (context.rs:25) donne accès direct.
- **Conséquence pratique (observée sur ce conteneur)** : pw-cli charge le filter-chain dans son propre
  contexte local → le graphe est hébergé dans pw-cli, les nœuds `input.X`/`output.X` visibles côté daemon
  sont les streams. Le nœud principal (`node.name=Holaf_FX_uid`) n'apparaît **pas** dans le registre
  distant (0.3.65). Le code Python cherche pourtant ce nœud par nom pour le `set-param` hot-reload
  (`_find_node_id_by_name(fx_node_name)`), avec repli "fall back to restart" s'il est introuvable.
  → **À trancher en spike phase 2 sur la machine réelle** (comportement PipeWire 1.x possiblement différent)
  : soit (1) holafd héberge le filter-chain in-process via un shim `pw_sys::pw_context_load_module` sur
  son propre Context (équivalent direct de pw-cli, un process de moins), soit (2) hot-reload set-param
  ciblé sur le nœud hébergé (impl-node local) → nouveau besoin FFI, soit (3) garder un sous-processus pw-cli.
- **Recommandation** : shim holaf-sys de ~10 lignes pour `pw_context_load_module` (+ helper de parse
  JSON→Pod si besoin, cf. b). Le reste (routage, null-sinks, links, metadata, discovery) est 100% pipewire-rs.

### b. Node::set_param(Props, Pod) — construction d'un Pod SPA
- Signature (src/node.rs:77) :
  `pub fn set_param(&self, id: spa::param::ParamType, flags: u32, param: &Pod)`
  avec `ParamType::Props = SPA_PARAM_Props` (libspa src/param/mod.rs:25).
- **Pas de parser SPA-JSON** dans pipewire-rs ni libspa (`grep from_json|parse|json` dans src/pod = rien).
- **spa_sys expose le parser C brut** : wrapper.h inclut `<spa/utils/json.h>` et les bindings générés
  contiennent toute la famille `spa_json_*` (init, next, get_string/parse_string, parse_int, is_object, …)
  + `spa_pod_builder_*` (push_object, prop, string, float, …). pw-cli utilise exactement ça pour
  transformer `set-param N Props { ... }` en Pod.
- **Voie pure Rust (clés connues)** : libspa fournit un niveau haut :
  `pod::Value` (pod/mod.rs:978), `pod::Object` (l.1065), `pod::Property` (l.1114),
  macros `object!`/`property!` (l.1100/1176) ; sérialisation `serialize::PodSerializer::serialize`
  (serialize.rs:268) → `Vec<u8>` → `Pod::from_bytes` (pod/mod.rs:98).
  Pattern exact de l'exemple `examples/tone.rs` l.76-88 : construire `Value::Object { type_, id, properties }`
  puis sérialiser et envoyer. Adapté aux Props à clés standards (`SPA_TYPE_OBJECT_Props`,
  `SPA_PARAM_Props`, ids `SPA_PROP_*` bindés par spa_sys).
- **Limite à vérifier** : les Props du filter-chain utilisent des clés **dynamiques** (noms de nœuds
  "eq_L"/"eq_R", puis index de ports). Leur encodage en pod passe par le type-map/registre de clés de
  PipeWire (côté C, via spa_json+spa_pod_builder), non exposé proprement par pipewire-rs. Si le spike
  confirme ce besoin → parser JSON→Pod via spa_sys (FFI) ou hébergement pw-cli.
- Exemple bas niveau complet si besoin : `spa::pod::Builder` (builder.rs) add_string/add_float/…
  et `spa::pod::Parser` (parser.rs) en lecture.

### c. Création d'un Link → OUI, exposé et validé
- Type `pw::link::Link` (src/link.rs, `ObjectType::Link`), factory **`"link-factory"`** présente dans le
  daemon (module-link-factory).
- Création via `core.create_object::<Link>("link-factory", properties!)` — doc inline de
  `core.rs` l.105-135 et exemple `examples/create-delete-remote-objects.rs` (props
  `link.output.node/port`, `link.input.node/port`).
- **Validé en live** (probe Rust, ce conteneur) :
  `create_object::<Link>("link-factory", { link.output.node=45, link.output.port=47,
  link.input.node=46, link.input.port=50, object.linger=1 })` → Link id 55 dans le daemon
  (`pw-cli ls Link`). Les valeurs acceptées sont les **ids numériques globaux** (nœud + port) ;
  la résolution id-global↔nom se fait par discovery registry (voir D).

### d. pw_metadata → OUI, wrapper complet et validé
- `pw::metadata::Metadata` (src/metadata.rs) : proxy `ObjectType::Metadata` avec
  `set_property(&self, subject: u32, key: &str, type_: Option<&str>, value: Option<&str>)`
  (metadata.rs:53) et `clear()`, + listener `property` sur changements.
- Obtention de l'objet :
  * binder la metadata existante du daemon ("default") via le registry :
    `registry.bind::<Metadata,_>(&GlobalObject)` (pattern `examples/pw-mon.rs:155`) ; ou
  * la créer : `core.create_object::<Metadata>("metadata", { metadata.name = ..., object.linger=1 })`
    (factory "metadata" présente, module-metadata).
- **Validé en live** : bind de la metadata `default` (id 32) puis
  `set_property(0, "default.audio.sink", Some("Spa:String:JSON"), Some(r#"{ "name": "Holaf_Probe_B" }"#))`
  → vérifié côté daemon (`pw-dump` : `default.audio.sink = { "name": "Holaf_Probe_B" }`).
  Format du value = SPA-JSON encodé en String (même forme que wireplumber).
- Cas d'usage holafd : `system.set_default_sink` = binder "default" et écrire `default.audio.sink`.

### e. Compatibilité de version
- pipewire-sys 0.8.0 : `[package.metadata.system-deps.libpipewire] name="libpipewire-0.3" version="0.3"`
  → exige **libpipewire-0.3 >= 0.3**. `links = "pipewire-0.3"`.
- libspa-sys 0.8.0 : `libspa-0.2 >= 0.2`.
- Système installé : `libpipewire-0.3 = 0.3.65`, `libspa-0.2 = 0.2` → **compatible**, build du crate OK
  (bindgen nécessitait clang/libclang + pkg-config, installés).
- Machine cible (Arch/CachyOS, PipeWire ~1.2) : l'ABI pipewire garde le soname `libpipewire-0.3.so.0`
  entre 0.3.x et 1.x → un binaire compilé contre 0.3.65 se charge sur 1.x. Le crate 0.8 est l'API stable
  utilisée contre PipeWire 1.x. Le comportement exact (notamment load-module/hot-reload, §a) reste à
  confirmer sur la vraie machine en phase 2. Le crate expose des features `v0_3_XX` jusqu'à v0_3_57 ;
  libs >= 0.3.57 couvertes.

### D. (question 3 de la tâche) Nœud null-sink/adapter → pas d'exemple dédié dans examples/ mais API validée
Le crate n'a pas d'exemple null-sink (seuls : audio-capture.rs, create-delete-remote-objects.rs, pw-mon.rs,
roundtrip.rs, streams.rs, tone.rs). La création passe par le même `core.create_object` que le link, avec
type `Node` et factory `"adapter"` :
```rust
let _sink = core.create_object::<pw::node::Node>(
    "adapter",
    &pw::properties::properties! {
        "factory.name" => "support.null-audio-sink",
        "node.name" => "Holaf_Probe_A",
        "media.class" => "Audio/Sink",
        "audio.channels" => "2",
        "audio.position" => "[ FL FR ]",
        "object.linger" => "1",
    },
)?;
```
**Validé en live** : 2 null-sinks stéréo créés (ids 45/46), ports playback_FL/FR + monitor_FL/FR, et
persistance via `object.linger`. (Contrairement aux Nodes, les objets `metadata` créés par un client ne
survivent pas à sa déconnexion — utiliser le bind de la metadata existante.)

**Exemples du crate à citer** :
- `examples/tone.rs` → construction d'un Pod (Value::Object + PodSerializer) puis envoi.
- `examples/create-delete-remote-objects.rs` → discovery de factory + `create_object` + `destroy_object`.
- `examples/pw-mon.rs` → registry listener + `registry.bind::<Node/Port/Link/Metadata,_>(&obj)`.

---

## C. Recommandation : pipewire-rs suffit-il, ou faut-il un shim FFI ?

| Fonctionnalité | API pipewire-rs | FFI nécessaire ? |
|---|---|---|
| Connexion / discovery / registry / destroy | ✔ (Context/Core/Registry/Node/Port) | non |
| Null-sinks (adapter support.null-audio-sink) | ✔ `core.create_object::<Node>` | non |
| Links (link-factory) | ✔ `core.create_object::<Link>` | non |
| Metadata default.audio.sink | ✔ `Metadata::set_property` (+bind registry) | non |
| **Chargement module filter-chain** | ✘ | **oui (minimal)** : `pw_sys::pw_context_load_module` sur `Context::as_raw_ptr()` (hébergement in-process type pw-cli), ou sous-processus pw-cli |
| **set-param Props hot-reload** | ✔ pour Pod construit manuellement (clés connues) | **à décider au spike** : clés dynamiques du filter-chain + éventuel set-param sur nœud local (impl) → parser JSON→Pod via `spa_json_*`/`spa_pod_builder_*` (déjà bindés par spa_sys) si nécessaire |

**Conclusion** : l'API pipewire-rs 0.8.0 couvre ~90 % du périmètre (connexion, discovery, null-sinks,
links, metadata, registry, création/destruction). Les deux points nécessitant un shim `holaf-sys` sont
précis et bornés :
1. `pw_context_load_module` (chargement filter-chain dans le contexte local de holafd, comme pw-cli) —
   1 appel C via `pipewire_sys`.
2. (probablement) un helper JSON-SPA→Pod pour le `set-param` hot-reload, réutilisant les fonctions C
   `spa_json_*` + `spa_pod_builder_*` déjà exposées par `spa_sys` — sauf si le spike prouve qu'une
   construction pure Rust (`Value::Object` + `PodSerializer`) suffit pour les Props du filter-chain.
   Sinon, alternative 0-FFI : garder un hôte pw-cli en sous-processus (architecture Python actuelle).

Probe de validation (sources à réutiliser en phase 2) : `/tmp/api-probe/src/main.rs` — valide live :
connexion, création 2 null-sinks, discovery ports par registry, création d'un Link, bind metadata
"default" + écriture default.audio.sink.
