# rust/ — Réécriture Rust du cœur de Holaf-Mix

Workspace cargo du **daemon `holafd`** et de son client CLI **`holafctl`**.  L'UI
Python (PySide6) parle au daemon en NDJSON sur socket Unix au lieu de piloter
PipeWire directement — **c'est le mode par défaut** de Holaf-Mix (repli
`AudioEngine` Python si le daemon ne démarre pas, opt-out
`HOLAF_USE_PYTHON=1`) : voir §9 de la spec et §0 de
[`docs/rust-rewrite-status.md`](../docs/rust-rewrite-status.md).

La spec complète (modèle, contrat IPC, mapping FX, jalons) est dans
[`docs/rust-rewrite-spec.md`](../docs/rust-rewrite-spec.md).  L'état courant des
jalons est suivi dans [`docs/rust-rewrite-status.md`](../docs/rust-rewrite-status.md).

## Arborescence

```
rust/
  Cargo.toml                 # [workspace] members = crates/*
  crates/holaf-core/         # J0 — modèle, enums, tables FX, migrations, SPA (pur)
                            #        + trait Backend/BackendError + helpers graph (J2)
  crates/holaf-ipc/          # J3 — types serde du protocole NDJSON (req/resp/event)
  crates/holafd/             # J3 — binaire daemon (serveur socket Unix + backends)
  crates/holafctl/           # J3 — client CLI de debug (transport pur)
  crates/holaf-pw/           # J1/J2 — couche pipewire-rs : PipeWireBackend réel
                            #        (reconcilieur de graphe) + FFI + spike
  crates/holaf-ui/           # UI Rust/egui (port de l'interface PySide6) :
                            #        thème sérialisable, widgets, vue principale,
                            #        EngineBridge + config.json + VU réels,
                            #        dialogues, capture PNG headless
```

L'UI Rust est **fonctionnelle** : `cargo run -p holaf-ui` charge le vrai
`config.json`, pilote le graphe (`holaf-pw`, feature `engine-pw` **par défaut**)
et persiste chaque modification. Le **MIDI** est là aussi par défaut (`midir`,
feature `midi`) : le contrôleur stocké dans `ui_rust.midi_port` est ouvert au
démarrage (un AKAI MIDI Mix est reconnu par son nom), les mappings déjà écrits
par l'UI Python sont repris tels quels, et le bouton `MIDI` d'un strip ouvre
`Learn Volume` / `Learn Mute` / `Learn Mono` / `Clear Mappings`. Sans
séquenceur ALSA (`libasound2-dev` au build, `/dev/snd/seq` au run) l'UI démarre
quand même et l'explique.  Sans libpipewire :
`cargo build -p holaf-ui --no-default-features` ; sans MIDI :
`cargo build -p holaf-ui --no-default-features --features engine-pw` ; lister les
contrôleurs : `cargo run -p holaf-ui -- --midi-list` ; capture sans serveur X :
`cargo run -p holaf-ui --bin holaf-ui-snapshot -- --config ../config.json --out /tmp/ui.png`.
Architecture, modèle de thème, MIDI (§10ter), tray (§10quater) et plan de
migration (`HOLAF_UI=rust|python`) :
[`docs/ui-rust-design.md`](../docs/ui-rust-design.md) ; valeurs mesurées de
l'UI Qt : [`docs/ui-inventory.md`](../docs/ui-inventory.md).

### Tray (icône de zone de notification) — activée par défaut au build

**Le paquet distribué contient l'icône de tray** : `scripts/build_rust.sh`
passe `--features tray` à `holaf-ui` **par défaut** (donc `build.sh`, le
PKGBUILD, le tarball et la CI aussi).  Décision assumée du 2026-11-14.

```bash
./scripts/build_rust.sh                   # défaut : AVEC l'icône
HOLAF_NO_TRAY=1 ./scripts/build_rust.sh   # sans l'icône (HOLAF_TRAY=0 : idem, historique)

cd rust
cargo build -p holaf-ui --features tray   # idem, à la main
cargo test  -p holaf-ui --features tray   # + le test de repli sans display
```

Comme la feature cargo n'est pas dans `default = ["engine-pw", "midi"]`, un
`cargo build -p holaf-ui` **brut** ne l'inclut pas : c'est le *script* de build
(et le PKGBUILD/CI) qui ajoute `--features tray`.  Si la compilation avec la
feature échoue (GTK3 dev absent), le script **réessaie sans** : on garde
`holaf-ui` et on perd seulement l'icône (WARNING explicite).

| | |
|---|---|
| Paquets (Arch, **runtime**) | **`gtk3`** (lié : `libgtk-3.so.0`, `libgdk-3`, `pango`, `cairo`, `atk`…) + **`libayatana-appindicator`** (`dlopen`é par `libappindicator-sys`) |
| Paquets (Debian/Ubuntu) | `libgtk-3-dev` + `libayatana-appindicator3-dev` (build) ; `libgtk-3-0` + `libayatana-appindicator3-1` (runtime) |
| Hôte appindicator | sinon l'icône ne **s'affiche** pas : `gnome-shell-extension-appindicator`, `kstatusnotifieritem` (KDE), `tray` de `swaybar` |
| `libxdo` | **inutile** : `tray-icon` est déclaré `default-features = false` (cette feature ne sert qu'aux items prédéfinis Copy/Paste) |
| Bundle PyInstaller | ces bibliothèques **ne sont PAS embarquées** : `packaging/holaf-mix.spec` les retire de `a.binaries` (même règle que libpipewire) pour que l'app utilise la GTK3 **du système** |

Sans display ou sans hôte appindicator au lancement, `tray::available()` vaut
`false` et **fermer la fenêtre quitte** (cacher sans moyen de revenir
laisserait un processus inatteignable) ; le log dit
`tray: unavailable (…) - closing the window will quit, not hide`.  L'icône est
**générée en RGBA dans le code** (`tray::icon_rgba`) : aucun fichier, donc pas
de dépendance aux PNG gitignorés de `dist/icons/`.

**Wayland** : `tray-icon` ne livre les **clics** que sous X11. Sous
GNOME/Wayland les clics sur l'icône sont donc ignorés — c'est le **menu** qui
pilote l'app (`with_menu_on_left_click(true)` : le clic gauche ouvre le menu,
qui contient `Show/Hide`, `Restart audio engine`, `Quit`).  Détails et
limites : §10quater de [`docs/ui-rust-design.md`](../docs/ui-rust-design.md).

Seuls les jalons J0/J3/J4 sont **sans libpipewire obligatoire** ; `holaf-pw`
lie `libpipewire-0.3` (installée ici).  La couche réelle `holaf-pw` (J1/J2)
est faite et testée contre le daemon headless : voir
[`docs/j2-phase2-pipewire-backend.md`](../docs/j2-phase2-pipewire-backend.md).

## Prérequis

* Rust stable ≥ 1.98 (`rustc`/`cargo`).  `holaf-pw` (et donc `holafd`, qui en
dépend) lie `libpipewire-0.3`/`libspa-0.2` + `pkg-config`/`clang` pour le
build (installés sur ce conteneur).
* Python 3.10+ uniquement pour le côté client (`ipc_audio_engine.py`,
  `rust_launcher.py`) et les tests d'intégration.
* **Pour l'icône de tray** (feature `tray`, **activée par défaut par
  `scripts/build_rust.sh`**) : GTK3 dev + libappindicator — Arch `gtk3` +
  `libayatana-appindicator`, Debian/Ubuntu `libgtk-3-dev` +
  `libayatana-appindicator3-dev`.  Sans elles, le script réessaie sans la
  feature (build sans icône) ; `HOLAF_NO_TRAY=1` rend ce choix explicite.

### Dépendances RUNTIME des binaires livrés

Vérifiées sur le binaire (`ldd rust/target/release/holaf-ui`, plus la liste des
commandes externes que le code lance) — ce sont les `depends` du PKGBUILD :

| | Arch | Debian / Ubuntu | Ce que ça fournit |
|---|---|---|---|
| moteur audio | `pipewire` | `pipewire` + `libpipewire-0.3-0` | `libpipewire-0.3.so.0` **liée** par `holaf-ui` et `holafd` ; `pw-cli`, `pw-dump`, `pw-link`, `pw-metadata`, `pw-top` |
| serveur Pulse | `pipewire-pulse` | `pipewire-pulse` | le socket auquel `pactl` parle (remplace `pulseaudio`, qui **entre en conflit** avec lui : même `pulse-native-provider`) |
| `pactl` | `libpulse` | `pulseaudio-utils` | liste / déplace les flux applicatifs (`holaf-pw/src/sink_inputs.rs`) |
| `pw-cat` | `pipewire-audio` | `pipewire-bin` | une capture enfant par source pour les vumètres (`holaf-ui/src/capture.rs`) |
| session | `wireplumber` | `wireplumber` | construit le graphe (nœuds physiques, `.monitor`) que le mixeur pilote |
| MIDI | `alsa-lib` | `libasound2` | `libasound.so.2` **liée** : séquenceur ALSA via `midir` |
| FX | `swh-plugins` | `swh-plugins` | `gate_1410`, `mbeq_1197`, `sc4_1882` dans `/usr/lib/ladspa/` (⚠ `ladspa-plugins` n'existe sur aucune des deux distros ; `rnnoise` ne contient **pas** le plugin LADSPA `librnnoise_ladspa`) |
| tray | `gtk3` + `libayatana-appindicator` | `libgtk-3-0` + `libayatana-appindicator3-1` | GTK3 **liée**, appindicator `dlopen`é |

## Builder

Depuis la racine du workspace :

```bash
cd rust

# Tout le workspace (core + ipc + daemon + CLI)
cargo build --release

# Seulement le daemon (le plus utile pour lancer l'app)
cargo build --release -p holafd

# Seulement le client CLI
cargo build --release -p holafctl
```

Les binaires sortent dans `rust/target/release/` (`debug/` pour `cargo build`
sans `--release`).  C'est l'emplacement que le launcher Python
(`src/backend/rust_launcher.py`) cherche en priorité, et celui que
[`scripts/build_rust.sh`](../scripts/build_rust.sh) publie dans `dist/`.

`rust/target/` est ignoré par git (`.gitignore` racine).

## Lancer le daemon

```bash
cd rust
./target/release/holafd --backend fake
```

* Socket par défaut : `$HOLAF_SOCKET`, sinon `/tmp/holaf-$USER.sock`.
* `--backend fake` : moteur **en mémoire** (aucun audio) qui répond à tout le
  contrat IPC — idéal pour valider la pile sans PipeWire.
* `--backend unimplemented` (défaut) : répond `not_implemented` sur tout ordre
  qui toucherait l'audio.  Le vrai backend PipeWire arrive à J1/J2.
* Logs : `env_logger` sur stderr (`RUST_LOG=debug` pour plus de détails).

### Vérifier une config sans PipeWire : `--check-config`

```bash
cd rust
./target/release/holafd --check-config ../config.json
```

Charge `config.json`, applique les migrations `holaf-core`, imprime le plan de
graphe **sans se connecter à PipeWire** et sort avec le code 0 (2 en cas
d'erreur).  C'est aussi un smoke-test des migrations sur la vraie config.

## Client CLI : holafctl

Chaque commande ouvre une connexion fraîche, envoie une requête (id 1), affiche
le JSON de réponse et sort 0 en cas de succès / 1 en cas d'erreur.

```bash
# Daemon qui tourne (terminal 1)
cd rust && ./target/release/holafd --socket /tmp/demo.sock --backend fake

# Terminal 2
cd rust
export HOLAF_SOCKET=/tmp/demo.sock

./target/release/holafctl status
./target/release/holafctl apply-snapshot --config ../config.json   # 7 strips appliqués
./target/release/holafctl volume <uid> 0.5
./target/release/holafctl mute   <uid> true
./target/release/holafctl fx-active <uid> eq true
./target/release/holafctl fx-params <uid> eq "100Hz gain=-3.0"
```

## Backend fake vs réel

| Backend            | Où / quand            | Ce qu'il fait                                      |
|--------------------|-----------------------|----------------------------------------------------|
| `fake`             | J3, maintenant        | Simule l'état en mémoire (strips, routes, ordres) — aucun son. Parfait pour tester l'IPC, `holafctl`, la couche Python. |
| `unimplemented`    | défaut aujourd'hui    | Répond `not_implemented` : évite tout effet de bord tant que le backend n'est pas choisi. |
| `pipewire`         | J2 (ce dépôt)         | Connexion au serveur PipeWire, null-sinks, graph filter-chain LADSPA, routage, volume/mute, discovery. Voir `docs/j2-phase2-pipewire-backend.md`. |

Le launcher Python choisit le backend via `$HOLAFD_BACKEND` (`fake` par
défaut) ; le vrai backend se lance explicitement avec
`holafd --backend pipewire`.

## État des jalons

Voir [`docs/rust-rewrite-status.md`](../docs/rust-rewrite-status.md) pour le
détail complet (tableau, commandes de test, reste à faire, incertitudes
pipewire-rs).

* **J0** ✅ `holaf-core` : modèle, FX, migrations, SPA-JSON.  69 tests.
* **J1 / J2** ✅ spike + `holaf-pw` réel (reconcilieur, `PipeWireBackend`) + `holafd --backend pipewire` — fait sur le conteneur headless (`docs/j2-phase2-pipewire-backend.md`).
* **J3** ✅ `holaf-ipc` + `holafd` + `holafctl`.  57 tests (146 au total workspace).
* **J4** ✅ `IpcAudioEngine` Python + `rust_launcher`, bascule du backend côté `main.py`.  21 tests Python (62 après J5).
* **J5** ◑ partie sans PipeWire faite : robustesse client (retry borné, timeouts, erreurs `code`/`request_id`), launcher anti-double-spawn + relance si mort, **mode Rust par défaut + repli Python visible**, packaging (`build.sh` → `scripts/build_rust.sh` → spec PyInstaller, PKGBUILD/.desktop), doc.
* **Machine utilisateur** 🔴 validation multi-plugins sur devices audio réels (strips physiques), PipeWire ~1.2, hotplug, metering natif.

## Tester

```bash
cd rust
cargo test                 # workspace complet : 146 tests verts (J0+J1/J2 lib+J3)
                           # le test live (backend_live) se saute si aucun daemon headless
cargo test -p holaf-core   # J0 + graph
cargo test -p holafd       # J3 (serveur IPC + FakeBackend + tests d'intégration)
cargo test -p holaf-pw     # lib pure (sans daemon) + FFI
cargo test -p holaf-pw --test backend_live   # contre le daemon headless (env headless requis)
```

Côté Python (client IPC + launcher) :

```bash
python -m unittest discover -s tests -v     # depuis la racine du dépôt
```

## Intégration future dans build.sh / packaging

`build.sh` (PyInstaller) et `packaging/` n'embarquent **pas encore** le binaire
Rust.  Le script [`scripts/build_rust.sh`](../scripts/build_rust.sh) construit
`holafd` en release et le copie dans `dist/` sans modifier `build.sh` ;
l'étape suivante (quand le backend réel sera stable) sera de l'appeler depuis
`build.sh` et de déclarer `holafd` dans la spec PyInstaller / PKGBUILD.

## Contribuer

1. Ne pas toucher au code Python classique (`audio_engine.py`) — il reste le
   comportement par défaut.
2. Toute fonctionnalité Rust doit venir avec des tests purs (pas de dépendance
   PipeWire) tant que J1/J2 ne sont pas sur machine réelle.
3. Le daemon est la source de vérité du graphe ; l'UI Python n'envoie que des
   ordres (voir spec §2/§4).
4. Ne pas committer les artefacts (`rust/target/`, `dist/`, `build/`).
