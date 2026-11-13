# J2 — Phase 2 : PipeWireBackend réel + reconcilieur (fait)

Date : suite de `docs/j1-spike-phase2.md`. Environnement : même conteneur
headless (PipeWire 0.3.65 + WirePlumber 0.4.13, LADSPA gate/mbeq/sc4/
tap_tubewarmth, pas de rnnoise).

Livrable : `crate holaf-pw` = implémentation réelle du trait `Backend` de
holafd (`PipeWireBackend`) derrière un reconcilieur de graphe, branchée dans
`holafd --backend pipewire`, + tests d'intégration live.

## 1. Structure

### Trait déplacé dans holaf-core (Étape 1)

Le trait `Backend` + `BackendError` + `UnimplementedBackend` vivaient dans
`holafd/src/backend.rs`.  Ils sont déplacés dans
**`holaf_core::backend`** (crate partagé, sans PipeWire) pour que `holafd` et
`holaf-pw` puissent tous deux les utiliser sans cycle de dépendance :

```text
        holafd ──┐
 holaf-core::backend ◄── holaf-pw (PipeWireBackend)
```

`holafd/src/backend.rs` re-exporte les types (`pub use holaf_core::backend::…`)
donc `fake.rs`, `server.rs`, `lib.rs` et les 47 tests J3 compilent inchangés.

Nouveaux fichiers :

| Fichier | Rôle |
|---|---|
| `holaf-core/src/backend.rs` | trait `Backend`, `BackendError`, `UnimplementedBackend` (déplacé) |
| `holaf-core/src/graph.rs` | helpers purs de planification : noms de nœuds `Holaf_FX_*` + bus `Holaf_Bus_<Label>` (voir `rust-rewrite-status.md` §2.4 ; à l'époque de J2 : `Holaf_Strip_<uid>`), `wants_fx_chain` (= `_has_active_fx` + graphe non vide), `is_physical`, `ChainFlavour` |
| `holaf-pw/src/live.rs` | vue pure du registre (`LiveView`) : mapping GlobalObject → nodes/ports/links + helpers FL/FR, testée sans daemon |
| `holaf-pw/src/engine.rs` | moteur réel (PwSession + Engine) : reconcilieur, chaines, liens, volume/mute, metadata, device_list |
| `holaf-pw/src/backend.rs` | `PipeWireBackend` : poignée `Send` qui marshalle vers le thread worker |
| `holaf-pw/tests/backend_live.rs` | test d'intégration live (skip si le daemon headless est injoignable) |
| `holafd/src/main.rs` | `--backend pipewire` (+ test de parse) |

## 2. Architecture / threading

PipeWire (`Context`/`Core`/`MainLoop`) est `!Send` (Rc interne) alors que le
trait exige `Send` (le daemon partage `Mutex<Box<dyn Backend>>` entre les
threads de connexion IPC — *depuis, `holafd` dispatche via un thread
propriétaire unique et un `mpsc`, voir `rust-rewrite-status.md` §6.1*).
`PipeWireBackend` est donc une **poignée Send**
qui marshalle chaque opération vers un **thread worker dédié** propriétaire
des objets PipeWire (mpsc + réponse synchrone, timeout 120 s).  Le mainloop
n'est itéré que quand une opération a besoin d'un round-trip / d'un poll
(pas de dispatch d'événements continu en J2).  C'est le modèle « thread pw +
ordres sérialisés » de la spec §4.

Se connecter au daemon = environnement du processus : `XDG_RUNTIME_DIR`,
`PIPEWIRE_RUNTIME_DIR`, `PIPEWIRE_REMOTE` sont honorés par PipeWire.  Le mode
headless se pointe donc uniquement par l'env :

```bash
bash /opt/holaf-headless/start-pipewire-headless.sh
export XDG_RUNTIME_DIR=/tmp/pw-runtime PIPEWIRE_RUNTIME_DIR=/tmp/pw-runtime \
       DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/pw-runtime/bus
holafd --backend pipewire
```

## 3. Reconciliation

`apply_snapshot` = **sync complet diff** du désiré, exécuté en **phases
ordonnées** (rien n'est câblé avant d'exister) :

| phase | contenu |
|---|---|
| P0 | premier apply seulement : décharger les chaînes qu'on héberge, puis `sweep_zombies` (débris `Holaf_*` d'un run précédent) |
| P1 | teardown des strips qui ont disparu (chaîne, null-sink, liens) |
| P2 | **publication de l'état désiré** dans `Engine::strips` |
| P3 | nœuds de strip (null-sinks) — **tous les nœuds avant tout lien** |
| P4 | chaînes FX `Holaf_FX_<uid>` (chargées **iff** `graph::wants_fx_chain`, ancrées à un nœud qui existe vraiment) |
| P5 | matrice de routage (liens entre strips — **les liens en dernier**) |
| P6 | passe de vérification : liens intra-strip de chaîne + volume/mute (idempotents) |
| P7 | default sink (metadata `default`) |

**Aucune phase n'avorte le snapshot.**  Tout ce qui ne peut pas être fait
maintenant (device pas encore énuméré, paire de ports pas publiée, null-sink
qui n'apparaît pas) devient un *pending* (`PendingIssue`) :

- retourné par `engine.apply_snapshot` (`degraded`/`pending`) et exposé par
  `status` (`degraded`, `pending: [{key, message}]`) ;
- **retenté automatiquement** par le worker (`Engine::repair_pending`, backoff
  200 ms → 5 s) : un casque USB qui s'énumère 300 ms après le snapshot est
  recâblé **sans aucune commande client** ;
- un toggle (`fx.set_active`) re-dérive **toujours** la topologie, donc
  l'utilisateur peut aussi réparer à la main.

Deux invariants nés de bugs mesurés (à ne pas casser) :

1. **les strips sont publiés avant tout accès** — le volume, les routes et la
   résolution des chaînes passent par `Engine::strips` ; l'ancien code ne le
   publiait qu'à la fin, donc le **premier** `apply_snapshot` n'appliquait
   aucun volume (nœuds laissés à 1.0 = +14 dB) ;
2. **le lien intra-strip d'une chaîne n'est jamais créé une seule fois** : il
   est vérifié à chaque apply/toggle/repair (`ensure_chain_links`).  Créé
   uniquement dans `create_chain`, il restait absent pour toujours si la cible
   n'existait pas à cet instant (symptôme : « EQ actif au lancement → ça
   hachait, et toggler l'EQ ne changeait rien »).

Règles de câblage :

- routes = union des `.routes` des strips input, **reliées/ré-liées** : on
  délie puis relie toujours, car le nœud effectif d'un strip change quand sa
  chaîne apparaît/disparaît (cible = `input.Holaf_FX_*` si chaîne, sinon le
  null-sink/device).  `unroute` détruit **toutes** les paires candidates
  (ancienne et nouvelle topologie), sinon deux chemins audio coexistent.
- chaîne non créée tant que son **nœud d'ancrage** (`graph::master_node_name`)
  est absent : `module-filter-chain` reçoit un `target.object` irrésoluble et
  WirePlumber relie alors son stream au *sink par défaut* — mesuré : la chaîne
  d'un strip output jouait dans le null-sink d'un **autre** strip.
- anti-doublon WirePlumber : `link_nodes` vérifie l'existence du lien
  (`LiveView::link_exists`) avant de créer via `link-factory`, et renvoie un
  `LinkReport` (créés / déjà présents).  Un rapport vide n'est pas un succès :
  la route est marquée *pending*.
- `LinkReport`/`ensure_route` : une route dont un port manque n'est plus
  silencieusement considérée comme créée.

## 4. FX toggles / hot-reload

- `fx_set_active`/`fx_set_params` : chaîne présente → `set-param Props` plat
  (`holaf_core::spa::control_pairs` + `build_props`, forme plate corrigée J1)
  sur **`input.Holaf_FX_<uid>`** ; première activation → création chaîne +
  liens ; chain disparue (`chain_gone`) → reconstruction.
- **Décision documentée** : une fois créée, la chaîne est **conservée** avec
  les valeurs neutres BYPASS quand le dernier effet est désactivé au runtime
  (comportement Python `update_fx_params`) ; seul un `apply_snapshot` sans
  effet actif la **détruit** (sync du désiré).

## 5. Volume / mute / mono / default sink / devices / apps

- `set_volume`/`set_mute` : `Props` `channelVolumes` + `monitorVolumes` +
  `mute` + `monitorMute` sur le nœud maître (null-sink pour virtuel, device
  pour physique).  Chaque `set_param` est suivi d'un **round-trip** : sans
  itération du mainloop, la requête reste dans la file du proxy et n'atteint
  jamais le daemon.
- **Courbe de volume PulseAudio** (parité Python, mesuré) : le volume d'un
  strip est le *pourcentage* affiché par l'UI, et le moteur Python l'applique
  avec `pactl set-sink-volume <node> <int(v*100)>%`.  La sémantique PulseAudio
  est **cubique** : `channelVolumes = v³` (0.44 → 0.08518, soit −21,4 dB).
  Écrire `0.44` en linéaire = −7,1 dB = **+14,3 dB plus fort** que Python
  (mesuré : `pactl 44%` et le Rust corrigé donnent −21,39 dB sur le même banc ;
  l'ancien code −7,13 dB).  Conversion centralisée :
  `holaf_core::model::pulse_linear_gain`.
- **`monitorVolumes` aussi** : le tap `<node>.monitor` (ce que lit le VU-mètre
  Python) a son propre gain, que le moteur Python règle via
  `pactl set-source-volume <monitor> …%`.  Le laisser à 1.0 faisait lire aux
  VU-mètres le niveau *pré-fader* (« les deux VU collés en butée alors que les
  sliders sont bas »).
- Le volume est **ré-asserté** ~40 ms après le premier `set_param` : WirePlumber
  restaure son état par-nœud (`~/.local/state/wireplumber/restore-stream`,
  clé = `node.name`) de façon asynchrone juste après l'apparition du nœud, et
  peut écraser la valeur qu'on vient d'écrire (mesuré sur le banc).
- `set_default_sink` : bind de la metadata `default` du daemon + écriture de
  `default.audio.sink` (`{"name": "Holaf_Strip_<uid>"}` / device), pattern J1.
- `set_mono` : ré-émission des liens sortants (force mono = FL dupliqué sur
  FL+FR, sans sommation — identique à `_auto_link_ports(force_mono=True)` du
  moteur Python).
- `device_list` : registry → nœuds `media.class` Audio/Sink|Source hors
  Holaf/streams.
- `app_list`/`app_move` : **STUB documenté** (JSON `stub:true`) — pas de
  gestion des sink-inputs en J2.
- `set_default_sink` : bind de la metadata `default` du daemon + écriture de
  `default.audio.sink` (`{"name": "Holaf_Strip_<uid>"}` / device), pattern J1.
- `set_mono` : ré-émission des liens sortants (force mono = FL dupliqué sur
  FL+FR, sans sommation) pour chaque route où le strip est source.
- `device_list` : registry → nœuds `media.class` Audio/Sink|Source hors
  Holaf/streams.
- `app_list`/`app_move` : **STUB documenté** (JSON `stub:true`) — pas de
  gestion des sink-inputs en J2.

## 6. Tests

- **Purs (sans daemon)** : holaf-core `graph` (noms, wants_chain), holaf-pw
  `live` (mapping GlobalObject→LiveView, FL/FR, liens), holaf-pw `backend`
  (helpers d'erreur) — comptés dans `cargo test --workspace`.
- **Live** (`holaf-pw/tests/backend_live.rs`) : skip si le daemon headless est
  injoignable / non configuré (garde stricte : `XDG_RUNTIME_DIR` headless ou
  `HOLAF_PW_LIVE=1`, pour ne jamais toucher le daemon d'une vraie session) :
  apply_snapshot 2 strips (output virtuel + EQ actif, input virtuel routé) →
  null-sink + chaîne + liens vérifiés via `status.holaf_nodes` **et** pw-dump ;
  `fx_set_params` change 100Hz=-3.5, **read-back pw-cli enum-params** ; re-apply
  EQ inactif → chaîne détruite, routage re-pointé direct, null-sink conservé ;
  volume/mute/default-sink acceptés ; `stop()` → 0 débris Holaf.

- **Live, robustesse du démarrage** (`holaf-pw/tests/apply_robustness.rs`) :
  volume appliqué **au premier** apply et avec la courbe cubique (read-back
  `pw-cli enum-params` : `channelVolumes` **et** `monitorVolumes` = 0.44³) ;
  device absent → apply non fatal, `degraded`/`pending` renseignés, aucun lien
  vers un mauvais nœud, puis **auto-réparation** dès que le device apparaît
  (sans aucune commande) ; strip cassé → les autres strips gardent chaînes,
  liens et routes ; toggle → liens intra-strip et routes re-dérivés ;
  câblage stéréo exact FL→FL / FR→FR et mono FL→FL+FR.
  Ces tests redémarrent le daemon headless entre eux (jamais sur une vraie
  session) : PipeWire 0.3.65 cesse de publier ses globals `Port` après
  beaucoup de cycles create/destroy, et le partage du daemon entre tests
  fausserait les mesures.

Résultat sur ce conteneur :
`cargo test --workspace` (avec env headless) : **164 tests verts**, dont
6 live de robustesse et 3 live `backend_live`.  Sans env headless, les tests
live passent en « SKIP » et le workspace reste 100 % vert.  Côté Python :
`python3 -m unittest discover -s tests` → **89 tests verts**.

## 7. Branchement holafd

`holafd --backend fake|unimplemented|pipewire`.  `--backend pipewire`
instancie `PipeWireBackend::connect()` (thread worker) et le sert derrière le
socket NDJSON → pilotable par `holafctl` (vérifié bout en bout : apply → chain
→ fx-params → read-back).

## 8. Écarts / limitations documentées (J2)

1. **Chaînes input** : le graphe complet (gate…comp) est supporté, mais le
   lien « source → chaîne input » est créé explicitement (pas de
   `target.object` « .monitor » type pactl) — le modèle holaf est 100 %
   adapter `support.null-audio-sink`, sans nœud monitor séparé.  Non
   exercé live (aucune source hardware dans le conteneur).
2. **Strips physiques** : si `device_name` est absent du registre (aucun
   hardware ici), warning + pas de nœud créé ; volume/FX ré-appliqués quand le
   nœud apparaît.  Non testable headless.
3. **Auto-link WirePlumber** : géré côté « ne pas dupliquer » (vérification
   avant création).  Pas encore de *nettoyage* des liens WP orphelins à la
   destruction de chaîne (les streams disparaissent avec le module → PipeWire
   retire leurs liens).
4. **`apply_snapshot` = sync complet** (le diff « laisser si identique » est
   respecté par les ops incrémentales : hot-reload au toggle, routing diff).
5. **Cleanup zombies** : premier `apply_snapshot` après connexion fait un
   `sweep_zombies` (objets `Holaf_*` orphelins d'un run tué en SIGKILL).  Un
   `engine.stop` normal déteardown tout (0 débris vérifié).
6. Les `set_param` sont suivis d'un round-trip mainloop (obligatoire pour
   flusher — cf. §5).

## 9. Reste pour la fin de J2 / J4-bis

- Vérifier `--reuse-core`/mono-client sur PipeWire 1.x (machine réelle) avant
  de figer l'architecture de connexion.
- Hotplug (events `device_list_changed`, rescan), retries/timeouts fins.
- Brancher l'UI Python sur le vrai backend : `HOLAFD_BACKEND=pipewire` +
  socket runtime — côté IpcAudioEngine/rust_launcher déjà prêt (le launcher
  choisit le backend via `$HOLAFD_BACKEND`, aujourd'hui `fake` par défaut) ;
  reste à passer la config utilisateur réelle en `apply-snapshot` et à
  confirmer le comportement audio sur machine avec devices.
