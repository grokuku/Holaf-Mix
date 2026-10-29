# rust/ — Réécriture Rust du cœur de Holaf-Mix

Workspace cargo du **daemon `holafd`** et de son client CLI **`holafctl`**.  L'UI
Python (PySide6) reste inchangée : en mode expérimental (`HOLAF_USE_RUST=1`)
elle parle au daemon en NDJSON sur socket Unix au lieu de piloter PipeWire
directement.

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
```

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
* **J3** ✅ `holaf-ipc` + `holafd` + `holafctl`.  46 tests (96 au total workspace).
* **J4** ✅ `IpcAudioEngine` Python + `rust_launcher` + bascule `HOLAF_USE_RUST=1`.  21 tests Python (28 après J5 partiel).
* **J5** ◑ partie sans PipeWire faite : robustesse client (retry borné, timeouts, erreurs `code`/`request_id`), launcher anti-double-spawn + relance si mort, `rust/README.md`, `scripts/build_rust.sh`, doc de bascule.
* **Machine utilisateur** 🔴 validation multi-plugins sur devices audio réels (strips physiques), PipeWire ~1.2, hotplug, metering natif.

## Tester

```bash
cd rust
cargo test                 # workspace complet : 137 tests verts (J0+J1/J2 lib+J3)
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
