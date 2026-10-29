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
| `holaf-core/src/graph.rs` | helpers purs de planification : noms de nœuds `Holaf_Strip_*`/`Holaf_FX_*`, `wants_fx_chain` (= `_has_active_fx` + graphe non vide), `is_physical`, `ChainFlavour` |
| `holaf-pw/src/live.rs` | vue pure du registre (`LiveView`) : mapping GlobalObject → nodes/ports/links + helpers FL/FR, testée sans daemon |
| `holaf-pw/src/engine.rs` | moteur réel (PwSession + Engine) : reconcilieur, chaines, liens, volume/mute, metadata, device_list |
| `holaf-pw/src/backend.rs` | `PipeWireBackend` : poignée `Send` qui marshalle vers le thread worker |
| `holaf-pw/tests/backend_live.rs` | test d'intégration live (skip si le daemon headless est injoignable) |
| `holafd/src/main.rs` | `--backend pipewire` (+ test de parse) |

## 2. Architecture / threading

PipeWire (`Context`/`Core`/`MainLoop`) est `!Send` (Rc interne) alors que le
trait exige `Send` (le daemon partage `Mutex<Box<dyn Backend>>` entre les
threads de connexion IPC).  `PipeWireBackend` est donc une **poignée Send**
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

`apply_snapshot` = **sync complet diff** du désiré :

1. teardown des strips qui ont disparu (chaîne déchargée, liens détruits,
   null-sink détruit) ;
2. pour chaque strip : null-sink `Holaf_Strip_<uid>` créé si virtuel (un
   device physique est adressé par `device_name`, nœud absent → warning, pas
   de node créé) ; chaîne `Holaf_FX_<uid>` chargée **iff**
   `graph::wants_fx_chain` (au moins un effet actif *applicable* + plugin
   dispo) ; chaîne obsolète **détruite** ; volume/mute poussés ;
3. routes = union des `.routes` des strips input, **reliées/ré-liées** : on
   délie puis relie toujours, car le nœud effectif d'un strip change quand sa
   chaîne apparaît/disparaît dans le même snapshot (cible = `input.Holaf_FX_*`
   si chaîne, sinon le null-sink/device) ;
4. default sink d'après `is_default` (sinon premier input), via metadata.

Anti-doublon WirePlumber : `link_nodes` vérifie l'existence du lien
(`LiveView::link_exists`) avant de créer via `link-factory` — exactement le
pattern validé au J1 phase 2 (« ne pas dupliquer les liens de WP »).

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

- `set_volume`/`set_mute` : `Props` `channelVolumes=[v,v]` + `mute` sur le
  nœud maître (null-sink pour virtuel, device pour physique).  Chaque
  `set_param` est suivi d'un **round-trip** : sans itération du mainloop, la
  requête reste dans la file du proxy et n'atteint jamais le daemon (bug
  réellement observé puis corrigé pendant la tâche).
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

Résultat sur ce conteneur :
`cargo test --workspace` (avec env headless) : **137 tests verts** (69 core +
13 holaf-pw + 8 main + 21 holafd lib + 6 ipc + 4 holafctl + 6 config_check +
…), incluant le test live (2.5 s).  Sans env headless, le test live passe en
« SKIP » et le workspace reste 100 % vert.

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
