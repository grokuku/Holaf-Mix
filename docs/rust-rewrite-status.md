# État de la réécriture Rust du cœur de Holaf-Mix

Document de suivi « productif » du chantier **holafd** (réécriture Rust du
cœur).  La spec technique détaillée vit dans
[`rust-rewrite-spec.md`](./rust-rewrite-spec.md) ; le guide du développeur Rust
est dans [`rust/README.md`](../rust/README.md).

Statut : **le backend Rust est le mode PAR DÉFAUT de Holaf-Mix** (voir §0).
**J5 — partie sans PipeWire terminée.**  **J1 + J2 phase 2 (backend
PipeWire réel) ont été réalisés sur ce conteneur headless** (PipeWire 0.3.65
+ WirePlumber 0.4.13, plugins LADSPA gate/mbeq/sc4/tap_tubewarmth) : voir
[`j2-phase2-pipewire-backend.md`](./j2-phase2-pipewire-backend.md).  Les
seuls points restants dépendent d'une **machine utilisateur avec audio réel**
(validation multi-plugins sur des devices physiques, PipeWire ~1.2, hotplug).

---

## 0. Mode par défaut : Rust (avec repli Python)

Depuis J5, `main.create_audio_engine()` **préfère le daemon Rust** — plus de
variable d'environnement à mettre pour en profiter :

```
  python main.py
        │
        ▼
  HOLAF_USE_PYTHON=1 (ou HOLAF_USE_RUST=0) ? ──── oui ──▶ AudioEngine (Python)
        │ non (= défaut)                                     (aucun daemon)
        ▼
  ensure_daemon_running()  →  holafd --backend pipewire
        │
        ├── daemon OK ──▶ IpcAudioEngine (NDJSON sur socket Unix)
        │
        └── échec ─────▶ WARNING loggé + raison dans la barre de statut (30 s)
                         → repli AudioEngine (Python)
```

* **Opt-out** : `HOLAF_USE_PYTHON=1` force l'`AudioEngine` Python classique (le
daemon n'est ni sondé, ni lancé). `HOLAF_USE_RUST=0` reste accepté (variante
historique de J4). `HOLAF_USE_RUST=1` est désormais redondant (= défaut).
* **Repli, jamais silencieux** : binaire absent, socket inaccessible, daemon
  qui meurt au démarrage, import IPC cassé → `WARNING` dans le log **et**
  message persistant dans la barre de statut de la fenêtre (30 s,
  `FALLBACK_NOTICE_TIMEOUT_MS`), puis `AudioEngine` Python. La raison exacte
  est attachée à l'engine (`_rust_fallback_reason`) et affichée.
* **Teardown** : le drapeau `_rust_daemon_spawned_by_us` n'est posé que sur le
  chemin Rust réussi ; après un repli, `shutdown_all()` ne tente donc **jamais**
  d'arrêter un daemon que l'app n'a pas lancé (§2.1).
* **Backend du daemon** : `pipewire` par défaut (`DEFAULT_BACKEND` dans
  `rust_launcher.py`) ; `HOLAFD_BACKEND=fake` reste l'override de debug/tests.

### 0.1 Résolution du binaire `holafd`

Ordre exact (premier trouvé gagne, chemin loggé au niveau INFO) :

1. `$HOLAFD_BIN` (doit déjà être exécutable — on ne `chmod` jamais un fichier
   nommé par l'utilisateur) ;
2. **app gelée (PyInstaller)** : dossier de `sys.executable` (le tarball y place
   `holafd`), puis `sys._MEIPASS` (extraction onefile = `_internal/` en onedir),
   puis `<app>/_internal` ; un candidat sans bit exécutable est rendu exécutable
   (`chmod +x`) à la volée (l'extraction onefile peut perdre le mode) ;
3. arbre de dev : `rust/target/release/holafd` puis `debug/` ;
4. `$PATH` (`shutil.which`).

Le socket reste `$HOLAF_SOCKET` sinon `/tmp/holaf-$USER.sock` ; le socket
périmé n'est supprimé que si personne n'y répond, et `stop_daemon()` ne retire
que celui créé pour un daemon qu'il possède.

### 0.2 Ce qui reste côté Python

Même en mode Rust, restent dans le processus Python (choix assumé, spec §6) :

* **découverte devices / apps** : `main_window.py` appelle encore
  `src/backend/pipewire_utils.py` (`pw-dump` / `pactl`) directement — indépendant
  du moteur, donc fonctionne à l'identique dans les deux modes ;
* **MIDI** (`midi_engine.py`, `mido` + `python-rtmidi`) ;
* **VU-mètres** : `MeteringEngine` (sounddevice + `PULSE_SOURCE`) alimenté par
  les noms de sources renvoyés par le daemon (`meter.sources`) ;
* **UI Qt** évidemment, et la config (`config.json`, migrations côté daemon).

Conséquence : le mode Rust sert l'**état et le graphe audio** ; migrer la
discovery dans le daemon est le reste du J5 (voir §4).

---

## 1. Tableau d'avancement des jalons

| Jalon | Description | Statut | Preuve |
|---|---|---|---|
| **J0** | Workspace cargo + `holaf-core` (modèle, FX, migrations, SPA-JSON, config IO) — pur, 0 dépendance PipeWire | ✅ **fait** | `cargo test -p holaf-core` : **75 tests** verts |
| **J1** | Spike pipewire-rs + `holaf-pw` (connexion, filter-chain, set-param, null-sink, liens, metadata) | ✅ **fait (conteneur headless)** | `cargo run -p holaf-pw --bin holaf-spike` (docs j1-spike-recon/phase2) |
| **J2** | `holaf-pw` réel derrière trait `Backend` (déplacé dans holaf-core), reconcilieur de graphe, routing, volume/mute, metadata, cleanup, `holafd --backend pipewire` | ✅ **fait (conteneur headless)** | `cargo test --workspace` 146 verts + `cargo test -p holaf-pw --test backend_live` (voir docs/j2-phase2-pipewire-backend.md) |
| **J3** | `holaf-ipc` + `holafd` (serveur socket NDJSON) + `holafctl` | ✅ **fait** | `cargo test -p holaf-ipc -p holafd -p holafctl` : **57 tests** verts |
| **J4** | `IpcAudioEngine` Python (même surface publique qu'`AudioEngine`), `rust_launcher`, bascule `HOLAF_USE_RUST=1` | ✅ **fait** | `python -m unittest discover -s tests -v` : **21 tests** verts |
| **J5** | **Partie sans PipeWire** : robustesse client (retry, timeouts, erreurs structurées), launcher anti-double-spawn/relance, packaging & doc, **bascule en mode par défaut + repli Python** | ✅ **fait** | 62 tests Python verts (dont 21 de sélection backend/résolution), E2E headless : daemon `--backend pipewire` par défaut, nœuds `Holaf_*` créés puis nettoyés |
| **J5 (metering)** | VU-mètres avec le backend Rust : `meter.sources` (uid → nom de source), `IpcAudioEngine.get_meter_levels` sur `MeteringEngine` local, durcissement `metering.py` (staleness, backoff, re-probe PortAudio) | ✅ **fait** | `holafctl meter-sources`, E2E sine → niveaux 1.0/0.0 |
| **J5 (suite)** | hotplug devices, metering natif optionnel, discovery devices/apps migrée dans le daemon | 🔴 reste à faire (dépend en partie de J1/J2) | — |

Total workspace Rust : **151 tests** (`cargo test`), tous verts avec un daemon
PipeWire headless (sans lui, les 2 tests live `backend_live` sont skippés).
Tests Python (client IPC + launcher + metering + sélection backend) : **62
tests** verts.

---

## 2. Ce que J5 « sans PipeWire » a apporté

* `ipc_audio_engine.py` :
  * retry **borné** sur les mutateurs légers (`retries=1`, soit 2 essais max) —
    uniquement sur pannes transport transitoires (`timeout`,
    `connection_error`, `connection_closed`), jamais sur une erreur applicative
    du daemon, et **jamais** pour `engine.apply_snapshot` (ordre lourd) ni
    `engine.stop` ; chaque retry est loggé ;
  * timeouts explicites documentés : tout `recv` tourne sous
    `socket.settimeout` (5 s par défaut, `REQUEST_TIMEOUT_S`) ; une absence de
    réponse devient une `IpcEngineError` de code `timeout` (plus de
    `socket.timeout` brut) ;
  * `IpcEngineError` enrichie : `code`, `message` **et** `request_id` (quand
    connu) sur toutes les erreurs transport et daemon.
* `rust_launcher.py` :
  * verrou thread + flag « tentative en cours » : des appels concurrents de
    `ensure_daemon_running()` (pool de threads UI) ne peuvent **plus**
    double-spawner — second appelant attend le verrou puis voit le daemon
    répondre ;
  * contrôle de survie du daemon lancé (`proc.poll()`, l'équivalent de
    `kill(pid,0)`) et effacement d'un processus mort/zombie pour relance
    propre au prochain appel ;
  * binaire introuvable → `False` + message clair (jamais d'exception
    silencieuse) ;
  * `stop_daemon()` : `terminate()` → `wait(3 s)` → `kill()` → `wait()`,
    sérialisé par le même verrou.  Depuis l'ajout du handler `SIGTERM` côté
    daemon (§6.4), `terminate()` déclenche un arrêt **propre** (teardown du
    graphe, socket supprimé, exit 0) ; `kill()` ne sert plus que de garde-fou.
* `rust/README.md`, `scripts/build_rust.sh`, `docs/rust-rewrite-status.md`,
  paragraphe README racine.

### 2.1 Cycle de vie du daemon (arrêt à la sortie d'app)

Décision de review, implémentée dans `main.py` + `rust_launcher.py` :

* `ensure_daemon_running()` ne retourne plus un `bool` mais un petit objet
  `DaemonHandle` (`ok`, `spawned_by_us`) — `bool(handle)` vaut `ok` donc les
  tests de véracité existants restent valables.
  * `ok` : un `holafd` répond sur le socket ;
  * `spawned_by_us` : **c'est ce processus qui l'a lancé** (spawn effectif).
    `False` quand un daemon tournait **déjà** avant l'app (lancé manuellement
    par l'utilisateur ou une session précédente).  Un daemon préexistant n'est
    donc jamais « possédé » par l'app.
* `main.py` (mode Rust, désormais par défaut) mémorise ce flag sur l'engine
  (`IpcAudioEngine._rust_daemon_spawned_by_us`) et, dans `shutdown_all()`
  (connecté à `QApplication.aboutToQuit`), appelle
  `rust_launcher.stop_daemon()` **uniquement si** `spawned_by_us` — et
  **après** `audio.shutdown()` (l'ordre IPC `engine.stop` part avant de tuer
  le processus).  Un daemon déjà actif avant l'app n'est jamais arrêté.
* `stop_daemon()` reste le garde-fou final : il ne termine qu'un processus
  réellement lancé par le module (tracké dans `_process`) et renvoie `False`
  sinon — la décision est donc exprimée à deux niveaux (retour de
  `ensure_daemon_running` + comportement de `stop_daemon`).

### 2.2 Aucun daemon orphelin, quel que soit le mode de mort de l'app

`aboutToQuit` est un signal **Qt** : il ne part que sur une fermeture propre
(dernière fenêtre fermée, `app.quit()`).  Un `SIGTERM` d'arrêt de session, un
`SIGHUP` de terminal fermé, un `SIGINT` ou un `kill` ne passent pas par Qt :
l'app mourait sans nettoyage et `holafd` survivait.  Trois filets imbriqués
règlent le problème :

1. `main.make_shutdown_all()` reste branché sur `aboutToQuit` (fermeture
   propre) ; la fonction est désormais **idempotente** (verrou pris sans
   blocage) donc un signal survenant pendant un arrêt propre ne déclenche pas
   un second teardown ;
2. `src/backend/signal_shutdown.py` installe des handlers `SIGTERM`,
   `SIGINT`, `SIGHUP` qui exécutent ce même teardown puis demandent à la
   boucle Qt de sortir (`app.quit()`) — l'app sort en code 0.  Un second
   signal sort immédiatement (code 130), comme `holafd`.  Deux garde-fous :
   un **thread watchdog** force la sortie (et `kill_daemon_now()`, sans verrou)
   si le teardown ne finit pas, et un **lecteur de wakeup fd**
   (`signal.set_wakeup_fd`) arme ce watchdog même quand le thread principal est
   bloqué dans du code C (boucle Qt au repos) ;
3. **PR_SET_PDEATHSIG** : `holafd` est lancé avec
   `prctl(PR_SET_PDEATHSIG, SIGTERM)` (`rust_launcher._make_pdeathsig_preexec`),
   donc le **noyau** le termine quand l'app meurt — y compris sur `SIGKILL`, où
   aucun handler ne peut s'exécuter.  Le `fork` est fait depuis un thread
   permanent (`holafd-spawner`) car PDEATHSIG est lié au **thread** qui fork ; le
   fils garde la session/groupe de l'app (donc Ctrl-C et signaux de terminal
   l'atteignent aussi) ; `$HOLAF_NO_PDEATHSIG=1` désactive le filet (debug).

Un daemon **préexistant** (`spawned_by_us=False`) n'est jamais concerné : le
filet noyau n'est armé qu'au moment du spawn, et `kill_daemon_now()` comme
`stop_daemon()` ne touchent que le `_process` du module.

Preuves automatisées : `tests/test_daemon_lifecycle.py` — dont deux tests
**live** (vrai `holafd` + vrai PipeWire) qui tuent l'app en `SIGKILL` puis en
`SIGTERM`/`SIGHUP`/`SIGINT` et vérifient la mort du daemon (< 2 s), le code de
sortie 0 et l'absence de nœud `Holaf_*` résiduel :

```bash
eval "$(./scripts/start-pipewire-headless.sh --export)"
python3 -m unittest tests.test_daemon_lifecycle -v
```

---

## 2.3 Les chaînes FX sont hébergées par un processus dédié (`--fx-host`)

Le symptôme « hachage audio en mode Rust seulement, sur une carte USB, quand une
chaîne FX est active » a une cause structurelle : `module-filter-chain`
s'exécute **dans le processus client**, et c'est la boucle d'événements de ce
processus qui rend ses deux *streams* programmables.  Le moteur Python n'a
jamais haché parce qu'il charge la chaîne dans un `pw-cli` dont le seul métier
est `pw_main_loop_run()` ; `holafd` l'hébergeait sur la même boucle que l'IPC,
la réconciliation, la réparation et le logging.

Depuis ce correctif, `holafd` **délègue** l'hébergement à un processus enfant
`holafd --fx-host` (protocole NDJSON sur stdin/stdout, `stdin` enregistré comme
source d'IO du mainloop, aucun autre travail sur ce thread) et garde tout le
graphe : null-sinks, liens, volume/mute, sink par défaut, métriques, routage.

* mode par défaut : `--fx-host-mode external` (env `HOLAF_FX_HOST`) ;
* repli automatique et **bruyant** vers `inprocess` si l'enfant ne peut pas être
  lancé (le graphe marche quand même) ;
* mort de l'hôte : respawn + rejeu des chaînes désirées (l'état désiré est chez
  le parent) puis re-vérification du lien intra-strip par la passe de
  réparation ; `status.fx_host = {mode, pid, respawns, chains}` ;
* l'hôte meurt avec `holafd` (EOF sur son tube, donc `SIGKILL` inclus).

Détails, protocole, mesures CPU et protocole de vérification sur machine réelle :
[`fx-host.md`](./fx-host.md).

---

## 3. Comment tester chaque brique

### 3.1 Rust pur (J0 + J3) — sans PipeWire

```bash
cd rust
cargo test                # workspace : 146 tests (J0 75 + J3 57 + J1/J2 14)
cargo test -p holaf-core  # J0 (75)
cargo test -p holafd      # J3 serveur + FakeBackend + IPC + arrêt propre (45)
cargo test -p holafctl    # J3 CLI (4)
```

Tests live (backend PipeWire réel) reproductibles : lancer d'abord
`./scripts/start-pipewire-headless.sh` (daemon PipeWire + WirePlumber + session
bus privés, sans session graphique), puis
`eval "$(./scripts/start-pipewire-headless.sh --export)"` dans le shell des
tests ; `./scripts/start-pipewire-headless.sh stop` arrête tout. C'est cette
mise en place qui fait passer `backend_live` au lieu de le skipper.

### 3.2 Daemon fake + holafctl (end-to-end sans PipeWire)

```bash
cd rust
cargo build --release -p holafd -p holafctl

# Terminal 1 : daemon fake sur socket dédiée
./target/release/holafd --socket /tmp/demo.sock --backend fake

# Terminal 2 :
export HOLAF_SOCKET=/tmp/demo.sock
./target/release/holafctl status
./target/release/holafctl apply-snapshot --config ../config.json
./target/release/holafctl volume <uid> 0.5
```

### 3.3 Vérification d'une config sans PipeWire

```bash
cd rust
./target/release/holafd --check-config ../config.json
# charge config.json, migre, imprime le plan de graphe, exit 0
```

### 3.4 Lancer l'UI (mode Rust par défaut)

```bash
cd rust && cargo build --release -p holafd        # binaire dispo pour le launcher
cd ..                                              # racine du dépôt
python main.py                                     # UI branchée sur IpcAudioEngine
HOLAF_USE_PYTHON=1 python main.py                  # opt-out : AudioEngine Python
HOLAFD_BACKEND=fake python main.py                 # daemon en mémoire (debug, sans audio)
```

Le launcher (`src/backend/rust_launcher.py`) localise `holafd` dans l'ordre :
`$HOLAFD_BIN` → bundle PyInstaller (dossier de l'exécutable, `sys._MEIPASS`,
`_internal/`) → `rust/target/release/holafd` → `rust/target/debug/holafd` →
`PATH`.  Backend du daemon : `pipewire` par défaut, `$HOLAFD_BACKEND` en
override.  Le socket est `$HOLAF_SOCKET` sinon `/tmp/holaf-$USER.sock`.
**Sans variable d'environnement, c'est le mode Rust ; en cas d'échec du daemon
l'app se rabat sur l'`AudioEngine` Python avec un WARNING visible (§0).**

### 3.5 Tests Python (J4 + robustesse J5 + sélection backend)

```bash
python -m unittest discover -s tests -v
# 62 tests : conversion strip→schéma Rust, enveloppe NDJSON, erreurs,
# retry borné sur connexion coupée, request_id, launcher anti-double-spawn,
# sélection du backend (défaut Rust / opt-out / repli) et résolution du binaire
# (dev, frozen + _MEIPASS, chmod, $HOLAFD_BIN).
```

---

## 4. Ce qui reste à faire pour le produit fini

1. **J1/J2 — backend PipeWire réel** : ✅ fait sur ce conteneur headless
   (`docs/j2-phase2-pipewire-backend.md`).  Reste à confirmer sur une machine
   utilisateur : validation avec devices audio réels (strips physiques),
   PipeWire ~1.2 / mono-client (`--reuse-core`), hotplug.
2. **J5 — suite** :
   * hotplug devices (événements `device_list_changed`, rescan) ;
   * metering natif optionnel (spec §6) — le metering Python est désormais
     alimenté par le daemon (`meter.sources`), un metering natif reste optionnel ;
   * migrer la discovery devices/apps de la UI (`pipewire_utils`) vers le
     daemon (actuellement l'UI appelle encore `pipewire_utils` directement).
3. **Packaging (fait)** : `build.sh` appelle `scripts/build_rust.sh` avant
   PyInstaller, `packaging/holaf-mix.spec` embarque `holafd`+`holafctl`,
   `PKGBUILD` et `.desktop` les installent à côté du binaire, la CI construit le
   daemon avant les étapes Python.  Reste à valider la **machine utilisateur** :
   un tarball/paquet réel doit trouver son `holafd` embarqué (log INFO
   « holafd resolved from the PyInstaller bundle »).
4. **J5 — polish** : purge des sockets/zombies résiduels au démarrage, tests
   de charge IPC, logging `RUST_LOG` réglable depuis l'app.

---

## 5. Incertitudes API pipewire-rs à lever (spec §3)

À lever **au J1** sur une machine réelle, avant d'écrire `holaf-pw` :

1. Chargement du module `filter-chain` (`libpipewire-module-filter-chain`)
   **dans le serveur exposé par pipewire-rs** ?  Sinon shim FFI.
2. `Node::set_param(ParamType::Props, …)` à partir d'un **Pod construit depuis
   du texte SPA-JSON** (les props anti-WirePlumber doivent être reproduites à
   l'identique) ?
3. Création de liens via la **factory `link-factory`** : exposée par
   pipewire-rs ou par l'API core `create_object` ?
4. Existe-t-il un **wrapper `pw_metadata`** pour piloter
   `default.audio.sink` (ou faut-il passer par les globals) ?
5. **Compatibilité versions** — ✅ **RÉSOLU (diagnostic + correctif)**.
   L'incertitude supposait un écart d'ABI entre le crate `pipewire = "0.8"`
   et `libpipewire` système (~1.2).  **Ce n'était pas ça.**  Le vrai
   problème était un bug de *génération de bindings* : `libspa-sys 0.8.0`
   s'appuie sur **bindgen 0.69.5**, qui échoue à parser certaines structs
   avec un **libclang récent** (Arch/CachyOS) et émet un champ factice
   `_address: u8` à la place des vrais champs — d'où l'erreur de compilation
   *« no field `data` on type `spa_pod_builder` »* (les headers C sont
   identiques entre PipeWire 0.3.65, 1.4.0 et master).  Bug amont :
   **pipewire-rs issue #112** (ouverte).
   Correctif appliqué : passage à `pipewire = "0.10"` → `libspa-sys 0.10.1`,
   qui embarque **bindgen 0.72.1** + system-deps 7 et parse correctement.
   MSRV 0.10 = 1.80 (notre pin `rust-version = "1.98"` reste OK), ABI
   `libpipewire-0.3`/`libspa-0.2` inchangée.  L'API 0.10 a en revanche
   introduit les wrappers *owning* (`MainLoopRc`/`ContextRc`/`CoreRc`,
   `PropertiesBox`) : `Node`/`Link` **possèdent** désormais leur proxy, donc
   les objets créés à distance doivent être conservés puis détruits via
   `core.destroy_object()` (cf. `holaf-pw/src/engine.rs`, `PwSession`).
   Vérifiable en un coup d'œil dans la CI (steps « Diagnose Rust build
   toolchain » et « Verify generated SPA bindings » de `build.yml`).

---

## 6. Écarts documentés (revue d'intégration)

### 6.1 Modèle de concurrence — ✅ RÉSOLU (spec §4 respectée)

L'implémentation antérieure de `holafd/src/server.rs` sérialisait les ordres
sous un `Mutex<Box<dyn Backend>>` tenu par les threads de connexion (écart
avec la spec §4).  C'est **corrigé** :

```text
 client A ─┐  thread lecteur par connexion
 client B ─┼─► mpsc::Sender<Command> ───► thread PROPRIÉTAIRE du backend
 client C ─┘         ▲                    (possède Box<dyn Backend>)
                     │ Command::Request { request, reply }
  thread lecteur ◄──┴──── Response ◄──── dispatch() sous catch_unwind
        └─► thread écrivain par connexion ─► socket

 Server::notify(event) ─► registre des écrivains ─► socket   (voie séparée)
```

* **Un seul thread exécute** les ordres → sérialisation naturelle, équivalent
  exact du `RLock` Python, sans verrou partagé ; l'ordre d'arrivée est l'ordre
  d'exécution (`engine.stop` compris, il passe après les ordres déjà reçus).
* **Événements hors du canal d'ordres** : `notify()` ne prend que le registre
  des écrivains (un `Mutex<HashMap<id, Sender>>`), verrou que le thread
  propriétaire ne tient jamais pendant une opération → un backend peut
  broadcaster depuis l'intérieur d'un ordre sans deadlock.
* **Panique backend** : chaque ordre tourne sous `catch_unwind`; le client
  reçoit `internal_error`, le thread (et le daemon) survivent.
* **Backend mort** : canal fermé / réponse déconnectée → `backend_unavailable`
  immédiat ; backend vivant mais lent → `backend_timeout` côté client borné à
  130 s (au-dessus des 120 s du backend PipeWire, pour que l'erreur précise du
  backend gagne).
* **Arrêt** : `Drop for Server` envoie `Command::Shutdown` au thread
  propriétaire (qui droppe le backend → teardown PipeWire) puis le rejoint
  (attente bornée à 5 s, sinon détachement) et supprime le fichier socket.

Conséquence : `holaf_pw::PipeWireBackend` a désormais **deux** niveaux de
sérialisation empilés (thread propriétaire du serveur + son propre thread
worker `Engine`).  Ce n'est plus nécessaire mais reste correct — voir 6.3.

### 6.2 Format des flottants SPA-JSON — divergence actée

Le formateur `holaf-core::spa` sérialise un `f32` avec le Display le plus court
de Rust (ryu) : `23520.0` → `"23520"`.  Le formateur Python historique
`_format_params` (`audio_engine.py`) passe par le float repr : `"23520.0"`.
**Divergence purement textuelle** : le parser SPA-JSON de PipeWire accepte les
deux formes.

Décision pour J2 (la plus simple) : les **fixtures golden sont générées depuis
le RUST** — le formateur Rust devient la référence (seul producteur de
SPA-JSON à J2).  Tout croisement avec les sorties Python héritées est
**normalisé au moment de la comparaison** (troncature d'un `.0` final ou
comparaison en valeurs flottantes), jamais en modifiant le format de sortie
Rust.  Aucun changement de comportement de formatage aujourd'hui.

### 6.3 Doublon de sérialisation côté `PipeWireBackend` — ⏳ TODO (analyse faite)

Avec le thread propriétaire du serveur (6.1), le **thread worker interne** de
`holaf_pw::PipeWireBackend` (`backend.rs` : worker `Engine` + `mpsc` + timeout
120 s) sérialise ce qui est déjà sérialisé.  C'est correct mais empile deux
sauts de thread et deux délais par ordre.

Suppression possible (non faite — trop large pour un changement sûr à froid) :

1. ajouter au serveur un constructeur **paresseux**
   `Server::bind_lazy(path, factory: impl FnOnce() -> Result<Box<dyn Backend>, E> + Send)`
   qui instancie le backend **sur le thread propriétaire** ;
2. relâcher la borne `Backend: Send` de `holaf_core::backend` (inutile une fois
   le backend créé dans son thread) ;
3. `PipeWireBackend` devient une façade synchrone directe sur `Engine` (plus
   de worker, plus de `REQUEST_TIMEOUT`, `Drop` → `engine.stop()`), et la
   vérification de connexion (aujourd'hui l'accusé `ready` avec timeout 20 s)
   remonte dans la factory (échec = erreur claire de démarrage du daemon).

Coût réel : retirer `Send` ripple dans les docs/tests, réécrire ~300 lignes de
`holaf-pw/src/backend.rs`, revalider `backend_live` (PipeWire requis à chaque
itération).  À faire dans une passe dédiée, avec le daemon headless sous la
main.

### 6.4 Arrêt propre : `SIGTERM` / `SIGINT`

`holafd` installe (crate `signal-hook`, pas de `unsafe` libc) un thread
« signaux » qui arme l'`AtomicBool` de shutdown déjà sondé par la boucle
`accept` et logge `received SIGTERM, shutting down`.  Séquence d'arrêt :
arrêt de l'accept → drop du `Server` → `Command::Shutdown` au thread
propriétaire → teardown backend (graphe PipeWire détruit, 0 débris) →
`remove_file` du socket → **exit 0**.  Un **second** signal force la sortie
(`exit 130`) pour qu'un ordre bloqué ne puisse jamais retenir le processus.

Tests : `crates/holafd/tests/shutdown.rs` lance le vrai binaire
(`--backend fake`) et vérifie, pour `SIGTERM` et `SIGINT`, le code de sortie 0,
la ligne de log et la disparition du fichier socket (toutes les attentes sont
bornées : un daemon bloqué fait échouer le test au lieu de figer la suite).
Vérifié aussi en E2E sur `--backend pipewire` (arrêt → 0 nœud / 0 lien
`Holaf_*` restant).

---

## 7. Raccourcis

* Spec : [`docs/rust-rewrite-spec.md`](./rust-rewrite-spec.md)
* Guide Rust / build / holafctl : [`rust/README.md`](../rust/README.md)
* Script build daemon : [`scripts/build_rust.sh`](../scripts/build_rust.sh)
* Client Python : [`src/backend/ipc_audio_engine.py`](../src/backend/ipc_audio_engine.py),
  [`src/backend/rust_launcher.py`](../src/backend/rust_launcher.py)
