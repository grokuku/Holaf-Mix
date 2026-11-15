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
feature échoue, le script **réessaie sans** : on garde `holaf-ui` et on perd
seulement l'icône (WARNING explicite).

| | |
|---|---|
| Backend | **`ksni`** — un `org.kde.StatusNotifierItem` en D-Bus pur (`tray-icon 0.26`, feature `ksni`). **Aucune bibliothèque GTK**, aucun `libappindicator`, aucune `GMainLoop` (migration du 2026-11-15 : `docs/dependency-audit.md`) |
| Paquets à installer | **aucun** au build comme au runtime |
| Hôte StatusNotifier | sinon l'icône ne **s'affiche** pas : `kstatusnotifieritem` (KDE/Plasma), `waybar`, `tray` de `swaybar`, `gnome-shell-extension-appindicator` (GNOME) |
| Détection de l'hôte | l'app interroge D-Bus (`org.kde.StatusNotifierWatcher`) **avant** de revendiquer un tray — l'équivalent de `QSystemTrayIcon::isSystemTrayAvailable()`, que le backend ksni ne fait pas lui-même |
| `libxdo` | **inutile** : `tray-icon` est déclaré `default-features = false` (cette feature ne sert qu'aux items prédéfinis Copy/Paste) |
| Repli | `--features tray-gtk` = l'ancien backend libappindicator (GTK3 `gtk3` + `libayatana-appindicator`) ; jamais activé par défaut |
| Bundle PyInstaller | plus aucune bibliothèque de tray à retirer ; `packaging/holaf-mix.spec` ne filtre plus GTK3 que pour le **fallback Qt** (`libqgtk3.so` de PySide6) |

Sans display, sans bus de session ou sans hôte StatusNotifier au lancement,
`tray::available()` vaut `false` et **fermer la fenêtre quitte** (cacher sans
moyen de revenir laisserait un processus inatteignable) ; le log dit
`tray: unavailable (no StatusNotifier host: org.kde.StatusNotifierWatcher has no
owner) - closing the window will quit, not hide`.  L'icône est **générée en
RGBA dans le code** (`tray::icon_rgba`) : aucun fichier, donc pas de dépendance
aux PNG gitignorés de `dist/icons/`.

**Preuve de bout en bout, sans desktop** :
`cargo test -p holaf-ui --features tray --test tray_live` démarre un
`dbus-daemon --session` privé, sert un faux `org.kde.StatusNotifierWatcher` et
vérifie que l'item **s'enregistre** puis publie le menu attendu
(`Show/Hide`, `Restart audio engine`, séparateur, `Quit`) via
`com.canonical.dbusmenu`.

**Clics** : le backend ksni ne transmet pas les clics de souris à l'app (comme
`libappindicator` avant lui).  C'est le **menu** qui pilote tout
(`with_menu_on_left_click(true)` : clic gauche → menu).  Détails et limites :
§10quater de [`docs/ui-rust-design.md`](../docs/ui-rust-design.md).

Pour l'interface : **Wayland**, pas X11.  La fenêtre (`eframe`/`winit`) est
compilée avec la feature cargo `wayland` **seule** — c'est ce qui supprime
l'icône « X11 » de la barre des tâches et le message « réclame un serveur X » :
sans le backend X11, le binaire ne peut plus être pris pour un client XWayland.
X11 reste disponible **en option** pour une machine X11-only, jamais par défaut :

```bash
cargo build -p holaf-ui --features x11      # session X11 uniquement
```

Plus de détails (paquets, `ldd`, ce qui reste à vérifier sur une vraie session) :
§ « Serveur graphique » ci-dessous.

### Serveur graphique : Wayland (X11 en option)

`rust/crates/holaf-ui/Cargo.toml` :

```toml
eframe = { version = "0.37", default-features = false,
           features = ["glow", "wayland", "default_fonts"] }
# feature cargo `x11` du crate = `eframe/x11`, désactivée par défaut
```

Mesuré ici (`ldd rust/target/release/holaf-ui`) :

* **sans `tray`** : ni `libX11`/`libxcb`, ni `libwayland-client`, ni
  `libxkbcommon`.  `winit` les `dlopen` (`wayland-sys/dlopen`,
  `xkbcommon-dl`) : c'est ce qui rend le binaire Wayland-only et ce qui explique
  qu'aucun outil ne les voie comme dépendances ;
* **avec `tray` (le paquet livré)** : plus rien de tout ça non plus — le
  backend ksni est du D-Bus pur (zbus, via la socket de session), donc
  `ldd holaf-ui` ne montre **ni GTK3, ni la pile X11 de GDK**
  (`libX11`, `libxcb`, `libXcursor`…), contrairement à l'ancien build
  libappindicator.

| | Arch | Debian / Ubuntu | Rôle |
|---|---|---|---|
| client Wayland | `wayland` | `libwayland-dev` | `libwayland-client.so.0`, **`dlopen`é** au run par `winit` (feature `wayland-sys/dlopen` → aucune édition de liens dans un build sans tray) |
| protocoles | `wayland-protocols` | `wayland-protocols` | XML `xdg-shell`, `xdg-decoration`, `viewporter`… consommés à la compilation par `wayland-protocols` (crate) |
| scanner | `wayland` (fournit `wayland-scanner`) | `libwayland-bin` | génération des bindings à la compilation |
| clavier | `libxkbcommon` | `libxkbcommon-dev` (build) / `libxkbcommon0` (run) | `libxkbcommon.so.0`, **`dlopen`é** (`xkbcommon-dl`) |
| X11 (option) | `libxkbcommon-x11` | `libxkbcommon-x11-dev` | seulement si on compile avec `--features x11` |

Avec `--features x11`, `winit` choisit son backend à l'exécution
(`WAYLAND_DISPLAY` d'abord, puis `DISPLAY`) : le binaire peut tourner sur les
deux, mais le paquet livré n'a que Wayland pour que le support Wayland ne
puisse pas régresser sans qu'on le voie.

**Limites connues** : le conteneur de dev n'a **ni Wayland ni X11**, donc seul le
repli est vérifiable ici (le binaire démarre, `run_native` échoue avec un
message explicite et propose le snapshot PNG) ; le verrouillage de la largeur
passe par `xdg_toplevel.set_min_size`/`set_max_size`, que la plupart des
compositeurs respectent pendant le drag, et `enforce_window_width` corrige le
cas contraire (`docs/ui-rust-design.md` §10quinquies).

Seuls les jalons J0/J3/J4 sont **sans libpipewire obligatoire** ; `holaf-pw`
lie `libpipewire-0.3` (installée ici).  La couche réelle `holaf-pw` (J1/J2)
est faite et testée contre le daemon headless : voir
[`docs/j2-phase2-pipewire-backend.md`](../docs/j2-phase2-pipewire-backend.md).

## Prérequis

* Rust stable ≥ 1.98 (`rustc`/`cargo`).  `holaf-pw` (et donc `holafd`, qui en
dépend) lie `libpipewire-0.3`/`libspa-0.2` + `pkg-config`/`clang` pour le
build (installés sur ce conteneur).
* **Wayland** pour bâtir l'UI: `wayland` + `wayland-protocols` +
  `libxkbcommon` (Arch), `libwayland-dev` + `wayland-protocols` +
  `libxkbcommon-dev` + `libwayland-bin` (Debian).  Le lien est **dynamique au
  run** (`dlopen`), mais `wayland-scanner` et les XML de protocoles sont
  nécessaires à la compilation du backend.  `--features x11` ajoute
  `libxkbcommon-x11` (Debian) — voir § « Serveur graphique ».
* Python 3.10+ uniquement pour le côté client (`ipc_audio_engine.py`,
  `rust_launcher.py`) et les tests d'intégration.
* **Pour l'icône de tray** (feature `tray`, **activée par défaut par
  `scripts/build_rust.sh`**) : **rien**.  Le backend est `ksni` (D-Bus
  StatusNotifierItem), donc plus de `gtk3` + `libayatana-appindicator` au build.
  Le script réessaie de toute façon sans la feature si elle échoue ;
  `HOLAF_NO_TRAY=1` rend ce choix explicite, `HOLAF_TRAY_GTK=1` force l'ancien
  backend libappindicator (qui, lui, exige `gtk3` +
  `libayatana-appindicator`).

### Dépendances RUNTIME des binaires livrés

Vérifiées sur le binaire (`ldd rust/target/release/holaf-ui`, plus la liste des
commandes externes que le code lance).  C'est **exactement ce qu'il faut
installer sur la machine cible**, que les binaires viennent de l'artefact CI
(`holaf-mix-<version>-<arch>.tar.gz`, voir `BUILD.md` §1.3) ou d'un paquet Arch
(ce sont les `depends` de `packaging/PKGBUILD`) :

| | Arch | Debian / Ubuntu | Ce que ça fournit |
|---|---|---|---|
| moteur audio | `pipewire` | `pipewire` + `libpipewire-0.3-0` | `libpipewire-0.3.so.0` **liée** par `holaf-ui` et `holafd` ; `pw-cli`, `pw-dump`, `pw-link`, `pw-metadata`, `pw-top` |
| serveur Pulse | `pipewire-pulse` | `pipewire-pulse` | le socket auquel `pactl` parle (remplace `pulseaudio`, qui **entre en conflit** avec lui : même `pulse-native-provider`) |
| `pactl` | `libpulse` | `pulseaudio-utils` | liste / déplace les flux applicatifs (`holaf-pw/src/sink_inputs.rs`) |
| `pw-cat` | `pipewire-audio` | `pipewire-bin` | une capture enfant par source pour les vumètres (`holaf-ui/src/capture.rs`) |
| session | `wireplumber` | `wireplumber` | construit le graphe (nœuds physiques, `.monitor`) que le mixeur pilote |
| affichage | `wayland` + `libxkbcommon` | `libwayland-client0` + `libxkbcommon0` | `libwayland-client.so.0` et `libxkbcommon.so.0`, **`dlopen`és** par `winit`/`xkbcommon-dl` (invisibles dans `ldd`) : l'UI est une cliente **Wayland** (X11 seulement si recompilée avec `--features x11`) |
| MIDI | `alsa-lib` | `libasound2` | `libasound.so.2` **liée** : séquenceur ALSA via `midir` |
| FX | `swh-plugins` | `swh-plugins` | `gate_1410`, `mbeq_1197`, `sc4_1882` dans `/usr/lib/ladspa/` (⚠ `ladspa-plugins` n'existe sur aucune des deux distros ; `rnnoise` ne contient **pas** le plugin LADSPA `librnnoise_ladspa`) |
| FX (tube) | `tap-plugins` (**AUR**) | `tap-plugins` | `tap_tubewarmth` — l'étage **TUBE WARMTH** ; absent ⇒ seul cet étage n'est pas proposé |
| tray | **aucun** | **aucun** | `ksni` = D-Bus pur : rien à lier, rien à `dlopen`er (l'hôte StatusNotifier du bureau dessine l'icône) |

## Builder

Réservé au développement : l'utilisateur final n'a **pas** besoin de compiler,
les binaires arrivent dans l'artefact CI (`BUILD.md` §1).

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

## Empaquetage (fait) et livraison

Les trois binaires sont construits par [`scripts/build_rust.sh`](../scripts/build_rust.sh)
(qui publie `dist/{holaf-ui,holafd,holafctl}`), embarqués par
`packaging/holaf-mix.spec` (onefile **et** onedir), inclus dans le tarball de
`build.sh`, installés par `packaging/PKGBUILD` et **construits par la CI**
(`.github/workflows/build.yml`, artefacts `holaf-mix-…` et `holaf-rust-…`).

Conséquence pratique : **rien de tout ça n'a besoin d'un toolchain Rust sur la
machine de destination** — les binaires arrivent précompilés dans l'artefact CI
(cf. `BUILD.md` §1).  Le PKGBUILD reste une option de distribution Arch
maintenue, pas le chemin de livraison.

## Contribuer

1. Ne pas toucher au code Python classique (`audio_engine.py`) — il reste le
   comportement par défaut.
2. Toute fonctionnalité Rust doit venir avec des tests purs (pas de dépendance
   PipeWire) tant que J1/J2 ne sont pas sur machine réelle.
3. Le daemon est la source de vérité du graphe ; l'UI Python n'envoie que des
   ordres (voir spec §2/§4).
4. Ne pas committer les artefacts (`rust/target/`, `dist/`, `build/`).
