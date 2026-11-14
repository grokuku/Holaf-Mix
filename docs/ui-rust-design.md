# Portage de l'UI vers Rust / egui — conception

État du portage de l'interface PySide6 vers `egui` 0.37 / `eframe` 0.37, crate
[`rust/crates/holaf-ui`](../rust/crates/holaf-ui).

Les valeurs chiffrées (tailles, couleurs, ordre des rangées, signaux) sont dans
[`docs/ui-inventory.md`](./ui-inventory.md) : ce document-ci décrit
l'**architecture** et le **plan de migration**, pas les constantes.

> **Ce qui existe aujourd'hui** : l'UI **fonctionnelle** — elle charge le vrai
> `config.json` (migrations et clés inconnues préservées), pilote le graphe
> PipeWire par `holaf-pw` (`apply_snapshot`, volume, MUTE/MONO, FX et leurs
> paramètres, routage, device, default sink, ajout/suppression/renommage),
> affiche des **VU réels** (capture `pw-cat` → RMS × 5 → `Arc<[AtomicU32]>`,
> enveloppe de décroissance), persiste chaque modification (écriture atomique
> débouncée) et affiche un **bandeau** quand le démon manque (mode déconnecté).
> Dialogues présents : paramètres FX, ajout de strip, confirmation de
> suppression, choix de device, **sélection d'applications**, **diagnostic** et
> **choix du contrôleur MIDI**
> (`--dialog` du binaire de capture), menus contextuels fenêtre / strip / FX,
> always-on-top, MIDI (contrôleur, learn, LED, reconnexion).  L'**icône de tray**
> est **activée par défaut au build** (feature `tray`, GTK3 + libappindicator ;
> `HOLAF_NO_TRAY=1` pour l'enlever) — voir §10quater.

![Capture de l'UI Rust — fenêtre courte 800 × 600](ui-rust-snapshot.png)

![Capture de l'UI Rust — fenêtre haute 800 × 900](ui-rust-snapshot-tall.png)

![Capture de la fenêtre de diagnostic — onglet Engine](ui-rust-dialog.png)

*Trois captures du **même** code, générées par
`cargo run -p holaf-ui --bin holaf-ui-snapshot -- --config ../config.json
--height 600` (haute : `--height 900`) — les **7 strips du vrai `config.json`**
(4 entrées, 3 sorties, volumes/faders et MUTE tels que l'utilisateur les a
laissés), thème `holaf-dark`, VU de démonstration déterministes.  Aucune fenêtre
n'a été ouverte : le rendu passe par le rasteriseur logiciel de `raster.rs`
(voir §7).

Elles prouvent les deux règles de dimensionnement (§2bis) : la **largeur est
identique** (800 px, calculée pour 4 entrées + 3 sorties), et seule la **hauteur
change**.  Entre les deux, les rangées fixes d'une strip (nom, caption, combo,
FX, routage, MONO/MUTE, MIDI) sont aux mêmes positions relatives et gardent
exactement la même hauteur ; le **fader** d'entrée passe de 267 px à 567 px
(290 → 590 pour la strip *Default*, qui n'affiche pas `SELECT APPS`) et celui de
sortie de 357 px à 657 px, tandis que MONO/MUTE et MIDI restent collés au bas de
la strip (le fader grandit vers le bas : son haut ne bouge pas).  La strip
elle-même mesure 532 px de haut dans la capture courte et 832 px dans la haute
(fenêtre − 40 px de header − 10 px de marge − 18 px de barre d'état).

---

## 1. Pourquoi Rust pour l'UI

| Problème (PySide6) | Conséquence | Réponse du port |
|---|---|---|
| ~40 couleurs en dur dans des `setStyleSheet` | impossible de thématiser, impossible de tester | modèle de thème **données** + résolution pure (§3) |
| signaux Qt + `QThreadPool` | logique UI/moteur intriquée, tests difficiles | `UiAction` (données) + `EngineBridge` (canaux) (§4) |
| `QWidget` sans capture headless | aucune revue visuelle automatisee en CI | `render()` unique + rasteriseur logiciel (§7) |
| widget Qt = 600 lignes de Python + CSS | duplication Qt/Rust pour la même UI | un seul code de dessin, testable sans GPU |

Le point décisif est le dernier : **une seule fonction de rendu**
(`holaf_ui::render`) sert à la fois la fenêtre eframe, la capture PNG et les
tests.  Il n'y a pas de « mode capture » qui pourrait diverger du mode réel.

---

## 2. Architecture de la crate

```
rust/crates/holaf-ui/
├── Cargo.toml            eframe/egui 0.37 (glow, x11, default_fonts), image 0.25, holaf-core
│                         [[bin]] holaf-ui (eframe) + holaf-ui-snapshot (headless)
├── src/
│   ├── lib.rs            vue d'ensemble + helpers de test (`test_support`)
│   ├── theme/
│   │   ├── mod.rs        HexColor, Fill, ImageSpec/Fit, Theme, ThemeFile, surcharges
│   │   ├── resolve.rs    PUR : descripteur de strip + thème → ResolvedStripStyle
│   │   ├── paint.rs      helpers : dégradé à coins arrondis, mapping d'image (Fit)
│   │   └── images.rs     ImageCache (décodage `image` → TextureHandle, warn-once)
│   ├── widgets/
│   │   ├── fader.rs      fader vertical + VU intégrés (dégradé multi-arrêts)
│   │   └── buttons.rs    FX, routage, MONO/MUTE, MIDI, « + », combo
│   ├── views/
│   │   ├── main_window.rs  2 sections, header, colonnes, barre d'état
│   │   └── strip.rs        une strip (rangées fixes + fader)
│   ├── app.rs            AppState (strips, thème, engine, config) + UiAction + render()
│   ├── layout.rs         géométrie : largeur Qt verrouillée, strips qui remplissent la hauteur
│   ├── bridge.rs         EngineBridge : thread moteur ↔ UI (mpsc) + EngineCommand/Event
│   ├── config.rs         config.json : lecture + migrations, écriture atomique débouncée,
│   │                     préservation des clés inconnues, géométrie de fenêtre
│   ├── meters.rs         RMS/peak, enveloppe de décroissance, MeterBoard (Arc<[AtomicU32]>)
│   ├── capture.rs        captures `pw-cat` (une par source) → MeterBoard   [engine-pw]
│   ├── coalesce.rs       throttle 25 Hz du fader
│   ├── dialogs.rs        paramètres FX, ajout, confirmation, device, apps, diagnostic
│   ├── devices.rs        identité d'un device : match description/structure, (Not Found)
│   ├── apps.rs           app_list/app_move, dialogue SELECT APPS, enforcement 5 s
│   ├── menus.rs          menus contextuels (fenêtre/strip/FX) + « Open project page »
│   ├── diagnostics.rs    rapport depuis status() + constats automatiques + texte
│   ├── midi.rs           mapping CC/note → action, learn, feedback LED, port/reconnexion
│   ├── tray.rs           politique du tray + always-on-top + icône générée   [tray]
│   ├── fx_ui.rs          plages/ordre des ports de contrôle (table Qt)
│   ├── engine_json.rs    parsing `meter_sources` / `device_list` / `app_list`
│   ├── raster.rs         rasteriseur logiciel (meshes epaint → pixels → PNG)
│   ├── main.rs           binaire eframe (+ `--config`, `--midi`, `--midi-list`, `MidiDevice`)
│   └── bin/holaf-ui-snapshot.rs   capture (+ `--dialog`, `--dialog-tab`)
├── tests/engine_live.rs  harnais live (graphe réel, gaté sur le démon headless)
├── examples/config_probe.rs  ce que l'UI lit/écrit vraiment dans un config.json
└── (tests dans chaque module : 286 tests, 489 dans le workspace)
```

### Sens des dépendances

```
views ──► widgets ──► theme/paint ──► theme (données)
  │                                     ▲
  └──────► app (UiAction) ──► bridge ───┘ (theme/resolve est appelé par views)
```

Règles tenues :

* `theme/*` ne connaît **ni** `egui::Context` **ni** le modèle de strips : il
  reçoit des descripteurs (`StripDescriptor`) et rend des styles ;
* `widgets/*` ne mute jamais le modèle : ils rendent un `egui::Response` ;
* `views/*` ne mute jamais le modèle non plus : ils collectent des
  `UiAction` et les rendent à l'appelant ;
* `app.rs` est le seul endroit qui applique une action (`AppState::apply`) ;
* `bridge.rs` est le seul endroit qui parle au moteur.

Cette séparation vient directement des signaux Qt : chaque `Signal` de
`StripWidget` est devenu une **variante de données**, ce qui rend testable ce
que Qt ne permettait pas (un test vérifie que `ToggleFx` sur `out-hp` ne touche
pas `in-mic`, qu'un `DefaultSink` désélectionne les autres strips, qu'un uid
inconnu est ignoré, etc.).

### La crate ne dépend de PipeWire que si on le demande

```toml
[features]
default     = ["engine-pw"]      # l'UI est un mixer : le moteur réel par défaut
engine-pw   = ["dep:holaf-pw"]
```

`--no-default-features` compile **sans** `holaf-pw` (donc sans libpipewire) : la
capture PNG, les tests et un poste sans démon s'en servent, le mock est alors le
seul moteur.

`holaf-core` (modèle, tables FX, migrations, `Backend`) est une dépendance dure :
les strips de démonstration sont de **vrais** `holaf_core::model::Strip`, donc
l'UI ne peut pas diverger du modèle du démon.

---

## 2bis. Dimensionnement de la fenêtre

Décision utilisateur (2026-11-14), implémentée dans `layout.rs` :

1. **Vertical — libre, et les strips remplissent.**  La fenêtre reste
   redimensionnable ; les strips prennent **toute** la hauteur disponible et le
   **fader est le seul élément élastique** (les rangées de boutons gardent leur
   hauteur de thème, voir §« hauteur » ci-dessous).  C'est ce qui remplace
   l'ancien comportement « hauteur naturelle ~394 px, vide en dessous » : la
   fenêtre est une surface de faders, pas une liste de widgets.
2. **Horizontal — verrouillé, calculé.**  La largeur est
   `Σ section_width(count)` (formule Qt exacte, voir
   [`ui-inventory.md`](./ui-inventory.md) §1) et se recalcule toute seule quand
   une strip est ajoutée ou supprimée : `setFixedWidth` côté Qt,
   `ViewportCommand::InnerSize` côté Rust.

### Hauteur : qui prend quoi

| rangée | hauteur | élasticité |
|---|---|---|
| header de section (titre + « + ») | `add_button_size + section_padding` = 40 | fixe |
| strip : nom, caption, combo, FX, `Default Sink`, `SELECT APPS`, plaque de routage / `BUS MASTER` | thème | fixe |
| strip : **fader + VU** | `hauteur_dispo − rangées fixes` | **élastique** (plancher 40 px) |
| strip : MONO/MUTE, MIDI | thème | fixe |
| barre d'état | 18 | fixe (réservée, jamais recouverte) |

`layout::fader_height_for(theme, kind, targets, strip_height)` est la règle
entière en une ligne, et `layout::available_strip_height(theme, hauteur_fenêtre)`
donne la hauteur de strip pour une fenêtre.  Les deux sections reçoivent la
**même** hauteur, donc INPUTS et OUTPUTS restent alignés ; dans une section,
toutes les strips ont la même hauteur.

Sous le plancher (fenêtre plus courte que `layout::minimum_window_height`), le
fader s'arrête à 40 px et les rangées du bas sont **rognées** (`clip_rect` de la
strip) plutôt que dessinées à 2 px.  Ce régime est en principe inatteignable :
`main.rs` envoie `ViewportCommand::MinInnerSize` avec cette hauteur minimale.
Il n'y a **pas** de scroll : Qt en a un (`QScrollArea`), mais un scroll vertical
dans une colonne de faders casse le geste de mixage pour un gain nul (la fenêtre
ne descend pas si bas).

### Comment la largeur est verrouillée (et pourquoi pas `min == max`)

`layout::enforce_window_width(ctx, strips)` est appelé à chaque frame par
`render()` (donc l'app eframe **et** la capture headless passent par le même
code) : il lit `ctx.content_rect().width()`, calcule `required_width_for(strips)`
et n'envoie `ViewportCommand::InnerSize(vec2(requis, hauteur_courante))` **que si
l'écart dépasse 2 px** (`layout::width_correction`, fonction pure).

* `with_min_inner_size == with_max_inner_size` serait une ligne, mais c'est une
  *indication* que le WM applique **après** le drag : la fenêtre s'étire
  visiblement puis revient, et certains WM carrelants l'ignorent.  La commande
  de viewport est déterministe sur tous les backends et se corrige toute seule
  si le WM rate une demande.
* Le seuil de 2 px (arrondi du WM, `pixels_per_point`) est l'anti-boucle : une
  frame où la largeur est déjà bonne **n'émet aucune commande** (testé : deux
  tests headless inspectent `FullOutput.viewport_output[ROOT].commands`).
* La hauteur courante est **conservée** dans la commande : l'axe vertical
  n'appartient qu'à l'utilisateur.

---

## 3. Modèle de thème

### Les fonds de section sont **neutres** ; la couleur vient des strips

C'est la règle de couleur la plus facile à casser en lisant une capture
d'écran, et elle a été cassée une fois : le premier thème `holaf-dark`
peignait les colonnes en bleu/rouge désaturés (`#4a6b80` / `#5a3a44`).  Le code
PySide dit l'inverse — `_create_section("INPUTS", "#2b2b2b", …)` et
`_create_section("OUTPUTS", "#1e1e1e", …)` :

| Élément | Qt (`setStyleSheet`) | thème `holaf-dark` |
|---|---|---|
| fond INPUTS | `#2b2b2b` | `#2b2b2b` |
| fond OUTPUTS | `#1e1e1e` | `#1e1e1e` |
| fond fenêtre | — (les sections couvrent tout) | `#232323` (valeur du thème) |
| strip entrée | `#444444 → #3daee9` | identique |
| strip sortie physique | `#444444 → #e93d3d` | identique |
| strip sortie virtuelle | `#444444 → #9b59b6` | identique |
| strip en learn | `#444444 → #f39c12`, bordure `#f39c12` | identique |
| opacité du corps | 1.0 (opaque) | 1.0 |

Trois conséquences dans le code :

1. `SectionStyles` ne porte **que des gris** ; deux tests le verrouillent —
   `section_backgrounds_are_neutral` (étalement des canaux ≤ 2) et
   `the_two_sections_are_two_grey_levels` (INPUTS plus clair, OUTPUTS plus
   sombre, les deux sombres).
2. le sens du dégradé est `#444` **en haut** → couleur **en bas**
   (`qlineargradient(x1:0, y1:0, x2:0, y2:1, stop:0 #444, stop:1 <accent>)`),
   vérifié par capture de pixels et par un rendu Qt hors écran ; le test
   `strip_bodies_go_from_grey_to_the_kind_accent` l'épingle ;
3. `body_opacity` vaut `1.0` : un strip Qt est **opaque**, la section ne
traverse pas son dégradé.  Mettre 0.82 (la valeur du premier squelette)
délavait les couleurs des strips.

### Hiérarchie

```
ThemeFile                     ← un fichier JSON (~/.config/holaf-mix/theme.json)
├── name, version
├── theme: Theme
│   ├── colors: ThemeColors      (~30 rôles : accent, route_active, mute, texte…)
│   ├── backgrounds              fond de fenêtre, body_opacity, image globale
│   ├── dimensions               strip_width, strip_gap, corner_radius, boutons…
│   ├── typography               tiny/small/body/section_title/heading (+ facteur gras)
│   ├── sections                 { input: SectionStyle, output: SectionStyle }
│   └── strips                   { input/output/bus: StripKindStyle }
└── strips: IndexMap<uid, StripOverride>   surcharges par strip
```

`Fill` est une somme étiquetée (`solid` / `vertical_gradient`), `Fit` décrit le
mapping d'image (`cover`/`contain`/`tile`/`stretch`), `HexColor` est un newtype
`[u8;4]` qui se sérialise en `"#rrggbb"` / `"#rrggbbaa"` et **refuse** une
couleur invalide (une faute de frappe dans un thème doit être signalée, pas
remplacée par du noir).

### Tout est optionnel côté fichier

Chaque struct porte `#[serde(default)]` : un fichier de thème peut ne contenir
que ce qu'il change.

```json
{
  "name": "mon-theme",
  "theme": { "colors": { "accent": "#ff0000" } },
  "strips": {
    "in-mic": { "body": { "type": "solid", "color": "#1b2b1f" }, "opacity_milli": 1000 }
  }
}
```

Les champs inconnus sont ignorés (compatibilité ascendante), une `version`
supérieure à `THEME_FORMAT_VERSION` est **rejetée** (sinon le thème
« marche à moitié »).

### Résolution : `resolve()`

```rust
pub fn resolve(file: &ThemeFile, strip: &StripDescriptor) -> ResolvedStripStyle
```

Fonction **pure** (pas d'I/O, pas d'allocation, pas d'egui), appelée une fois
par strip et par frame :

```
kind      = Input | (Output avec device) | Bus (Output sans device)
body      = (surcharge.body ?? (learning ? style_learn : kind_style).body) × opacité
border    = surcharge.border ?? (learning ? style_learn : kind_style).border
accent    = surcharge.accent ?? colors.accent
opacity   = clamp(surcharge.opacity ?? backgrounds.body_opacity)
image     = surcharge.image ?? kind_style.image
```

`learning` est un drapeau du descripteur (`with_learning(true)`), pas un type de
strip : `set_learning(true)` repeint la frame en orange **quel que soit le type**
et laisse le type intact, exactement comme `_update_base_style`.

Précédence : **surcharge par strip > style par type > défaut intégré**.  Le cas
« bus » est dérivé du modèle (`device_name` absent **ou vide**), exactement
comme `StripWidget._update_base_style` — mais sans if/else dans le code de
dessin.  C'est ce qui permet de tester « une sortie sans device est violette »
sans ouvrir de fenêtre.

---

## 4. Threading : `EngineBridge`

```
thread UI (egui, 60 fps)                    thread moteur
──────────────────────────                  ─────────────
AppState::apply(UiAction)
   ├─ mute le modèle localement
   ├─ EngineCommand ──────mpsc──────────►   recv()
   └─ empile l'action (config/undo)                │
                                                   ├─ Backend::set_volume / set_mute / …
                                                   │  (ou mock en mémoire)
AppState::poll_engine()   ◄────mpsc───── EngineEvent::{Ready, Applied, Failed, Levels, Devices, Error}
   │  statut + rollback des commandes refusées
   └── MeterBoard::read(uid)  ◄──────────  Arc<[AtomicU32]> (f32 bits, 25 Hz)
```

Points de conception :

1. **Aucun blocage.** `send` est non bloquant ; un moteur mort devient
   `BridgeError::Disconnected` (puis le bandeau « mode déconnecté »), jamais un
   gel.
2. **Le backend appartient au thread moteur.** Avec `engine-pw`,
   `PipeWireBackend::connect()` est appelé **dans** le thread (le mainloop
   pipewire doit vivre là où il est itéré) — la closure
   `EngineBridge::spawn_with` reçoit la commande *et* le `HashMap` de niveaux.
3. **Un seul canal de commandes** (pas de pool de threads) : `PipeWireBackend`
   est déjà un handle `Send` qui marshale vers son worker.
4. **Changements structurels = snapshot.** Ajouter/supprimer/renommer une strip —
   et **changer de device** (debounce 200 ms, comme
   `_schedule_engine_restart`) — reconstruit le plan de graphe : on envoie la
   **liste complète** (`SyncSnapshot(Vec<Strip>)`), pas une commande ponctuelle.
   Renommer suffit à renommer le bus côté démon (`Holaf_Bus_<label>`).
5. **Métrage réel, hors du démon** (spec §6). `Backend::meter_sources()` ne fait
   que *nommer* les sources ; le thread moteur ouvre une capture `pw-cat
   --record --target <source>.monitor --format f32 -` par source
   ([`capture`](../rust/crates/holaf-ui/src/capture.rs)), calcule RMS et peak par
   bloc de 10 ms ([`meters`](../rust/crates/holaf-ui/src/meters.rs)) et publie
   `min(1, rms·5)` dans un `Arc<[AtomicU32]>` (la règle du moteur Python).  L'UI
   lit ces atomes à 25 Hz et applique `level = max(new, prev·0.85)` (par 40 ms,
   indépendant du framerate) pour que les barres redescendent.
6. **UI optimiste avec rollback.** Chaque changement réversible est enregistré
   (`Undo`) avant d'être envoyé ; une commande refusée revient comme
   `EngineEvent::Failed { command, reason }`, l'UI **rétablit** la valeur
   précédente du modèle et l'affiche dans la barre d'état.

Le mock (`EngineBridge::spawn_mock`) n'a **aucune dépendance temporelle** : il
applique la commande et renvoie `Applied` + `Levels`, ce qui rend les tests
déterministes ; il n'a **pas** de `MeterBoard` (testé), donc les niveaux passent
par les événements.  L'UI anime ses VU par une fonction du temps
(`AppState::animate`) quand aucun bridge n'est branché (capture PNG, tests).

### 4bis. Mode déconnecté

`PipeWireBackend::connect()` peut échouer (pas de démon, pas de permission, pas
de `libpipewire`).  Le thread moteur **reste vivant** et répond
`Failed(command, reason)` à chaque ordre : l'UI affiche un bandeau rouge
(`views::main_window::ENGINE_BANNER_HEIGHT`, testé au pixel près), garde toutes
ses commandes de modèle (faders, dialogues, config) et **journalise/ignore** les
actions audio.  Aucun panic, aucun gel : c'est un état de première classe.

### 4ter. Persistance

`AppState::from_config(path)` charge le `config.json` réel via
[`config::UiConfig`](../rust/crates/holaf-ui/src/config.rs) — `load_config` +
`migration::strip_from_dict` (les mêmes migrations que le démon) — plus la
géométrie (`window.height`) et le thème (`theme`, un nom ou un chemin).

Chaque action persistante programme une écriture **atomique débouncée** (400 ms,
une écriture par geste).  L'écriture part du **document chargé** et ne patche que
les clés modélisées, strip par strip (appariées par `uid`) : `mode`,
`assigned_apps`, `midi_volume`/`midi_mute`/`midi_mono`, `window_geometry` (le blob
Qt) et toute clé inconnue **survivent** — les perdre au premier mouvement de
fader serait une perte de données, pas une simplification.  Un config absent
laisse l'UI démarrer vide (largeur minimale 160+160) avec un avertissement
visible ; un config corrompu est signalé et **jamais réécrit**.

Quatre règles de préservation, chacune avec son test :

| clé | règle |
|---|---|
| `effects` | patché **clé par clé** depuis le bloc chargé : un effet que le modèle ne connaît pas (`reverb`, `limiter`, …) garde sa place et ses valeurs ; les cinq connus sont fusionnés dedans |
| `mode` | gardé du fichier tant que `device_name` n'a pas changé dans l'UI (`physical` + `device_name: null` reste `physical`) ; re-dérivé seulement sur un vrai changement de device |
| `uid` absent | réparé **avant** le mapping des extras (`in-legacy-N`, dédoublonné) : un strip sans `uid` garde son `mode`, ses `assigned_apps` et ses `midi_*` |
| `window.width` | réécrit seulement s'il existait déjà ; jamais **ajouté** à un fichier qui n'en a pas |

### 4quater. Géométrie : ce que l'UI s'autorise à réécrire

Seule la **hauteur** est persistée (`window.height`), et seulement après un
geste de redimensionnement :

* la hauteur chargée du fichier est la valeur à défendre ; la taille que le WM
  (ou `MinInnerSize`) impose au lancement n'est **pas** une décision de
  l'utilisateur, donc elle n'est jamais écrite — un simple lancement ne touche
  pas `config.json` ;
* la comparaison est *observé vs observé* (jamais observé vs persisté) : une
  taille de démarrage constante, quelle qu'elle soit, ne programme aucune
  écriture ;
* les premières frames (`app::RESIZE_ARM_FRAMES`) sont ignorées, le temps que la
  fenêtre se stabilise, et un écart < 2 px (`app::RESIZE_JITTER`) est du bruit.

La largeur, elle, est **verrouillée** sur `Σ section_width` : ce n'est pas un
choix de l'utilisateur, elle n'est donc écrite que si le fichier la portait déjà.
Le blob Qt `window_geometry` est conservé octet pour octet, et `window_state`
(état Qt hérité, `h` = hauteur du **cadre**) reste une clé opaque : `h` ne sert de
repli que si aucune hauteur n'est disponible, `window.height` primant toujours
(les deux ne mesurent pas la même chose).

---

## 5. Une seule fonction de rendu

```rust
pub fn render(state: &mut AppState, ui: &mut egui::Ui)
```

Trois appelants, un seul chemin de code :

| appelant | contexte | ce qu'il prouve |
|---|---|---|
| `src/main.rs` (`eframe::App::ui`) | fenêtre réelle, GPU | l'app |
| `src/bin/holaf-ui-snapshot.rs` | `Context::run_ui` + raster logiciel | la capture PNG |
| tests (`views::strip::strip_paints_headless_in_every_state`) | `Context` sans renderer | pas de panique de layout |

`eframe` 0.37 ne fournit plus `App::update` mais `App::ui(&mut Ui, &mut Frame)`
— vérifié dans la doc de la crate, la coquille `main.rs` ne fait que déléguer.

---

## 6. Modèle d'interaction (le remplacement des signaux)

`UiAction` couvre exactement les signaux de `strip_widget.py` (table complète
dans `docs/ui-inventory.md` §7).  Deux conséquences pratiques :

* un changement de modèle est **une valeur**, donc testable :

  ```rust
  state.apply(UiAction::ToggleFx { uid: "out-hp".into(),
                                   effect: EffectKey::Tube, active: true });
  assert!(hp.effects[&EffectKey::Tube].active);
  assert!(!mic.effects[&EffectKey::Tube].active);
  ```

* `apply` est aussi le point d'où partent les commandes moteur
  (`engine_commands(&action, &strips)`), l'écriture de config débouncée et le
  log — un seul endroit à instrumenter.

Les actions purement UI (ouvrir un dialogue, armer un learn MIDI, demander une
confirmation) ne produisent **aucune** commande moteur : c'est explicite dans
`engine_commands()`.

### Trois règles que Qt appliquait implicitement

| règle | Qt | Rust |
|---|---|---|
| le fader ne spamme pas le moteur | `QTimer` 50 ms (`_check_and_send_volume`, « 20 Hz limit ») | `coalesce::Throttled<f32>`, 40 ms (25 Hz), dernière valeur gagnante — testé sans dormir |
| un changement de device = **un** redémarrage | `_schedule_engine_restart` + `DEBOUNCE_ENGINE_RESTART_MS = 200` | `SaveDebounce` 200 ms → **un** `SyncSnapshot` pour toute la rafale |
| une modification = une écriture | `save_config` à chaque signal | `SaveDebounce` 400 ms + `save_config_atomic`, une écriture par geste |

Les dialogues vivent dans [`dialogs`](../rust/crates/holaf-ui/src/dialogs.rs) :
paramètres FX (sliders par port, plages `fx_ui::range_for_param`, ordre
`fx_ui::sorted_param_keys`, ports `=` masqués), ajout de strip (nom + type),
confirmation de suppression (`QMessageBox` « default No »), choix de device
(`device_list` filtré par `media_class`, plus « Virtual Bus »).  Le renommage
garde son dialogue dans `app.rs`.

Une commande refusée revient en `EngineEvent::Failed` : `AppState::rollback`
rétablit la valeur précédente (volume, mute, mono, FX, paramètres, routes,
default sink, device) et l'annonce dans la barre d'état.

---

## 7. Capture sans serveur X

Ce conteneur n'a ni X11, ni Wayland, ni Xvfb : `eframe` ne peut pas ouvrir de
fenêtre, donc la capture d'écran d'eframe est hors d'atteinte.  La chaîne
retenue :

```
Context::run_ui(RawInput{screen_rect, time, native_pixels_per_point = scale})
        │  (le MÊME render() que l'app)
        ▼
Context::tessellate(shapes, ppp)         → Vec<ClippedPrimitive> (meshes epaint)
        ▼
raster::render(framebuffer)               → triangles en CPU, blending prémultiplié
        ▼   (clip rects respectés, atlas de police échantillonné en bilinéaire)
Framebuffer::downsample(scale)            → anti-aliasing par sur-échantillonnage
        ▼
PNG (crate `image`)
```

Détails qui comptent :

* le rasteriseur reproduit les **mêmes maths que le shader epaint** : couleurs
  prémultipliées en sRGB, multiplication par le texel en gamma, blending
  `dst = src + dst·(1−a)` ;
* l'atlas de police est récupéré **après** la passe (`ctx.fonts(|f| f.image())`),
  sinon les glyphes utilisés n'y sont pas encore ;
* le sur-échantillonnage (`--scale 2`) compense l'absence de *feathering* sur
  les meshes construits à la main (le tessellateur ne feather que ses propres
  formes) ;
* `FullOutput.textures_delta` **panique** au drop s'il reste des deltas : ils
  sont systématiquement vidés (`clear()`).

```bash
cd rust
cargo run -p holaf-ui --bin holaf-ui-snapshot -- --out /tmp/holaf-ui.png
cargo run -p holaf-ui --bin holaf-ui-snapshot -- --scale 3 --time 2.0
cargo run -p holaf-ui --bin holaf-ui-snapshot -- --theme /tmp/mon-theme.json
cargo run -p holaf-ui --bin holaf-ui-snapshot -- --config ../config.json
```

`--config` rend les strips **réelles** de ce fichier ; la capture est en lecture
seule (le store de config est détaché, aucune écriture débouncée ne peut toucher
le fichier rendu).

Pièges vérifiés sur egui 0.37 (ils coûtent une heure chacun) :

| piège | symptôme | correctif |
|---|---|---|
| `ui.max_rect()` | vaut la zone *disponible* (5000 × 5000), pas la fenêtre | `ui.ctx().content_rect()` |
| `ctx.set_pixels_per_point()` | écrase le `screen_rect` demandé (zoom factor × ratio) | `RawInput.viewports[ROOT].native_pixels_per_point` |
| `Ui::id()` / `Id::new()` | dépréciés en 0.37 | `Ui::scope_id()`, `Id::unique()`, `IdSalt` |
| `egui::Rounding` | n'existe plus | `CornerRadius` (u8) |

---

## 8. Stratégie de migration : `HOLAF_UI=rust|python`

> **Étape 5 FAITE (bascule B1).**  `HOLAF_UI` vaut `rust` par défaut :
> `main.py` est devenu un **lanceur** (`src/launcher.py`) qui exécute le binaire
> `holaf-ui`, et l'UI Qt n'est plus qu'une échappatoire (`HOLAF_UI=python` ou
> `HOLAF_USE_PYTHON=1`).  Le plan de suppression du Python est dans
> [`migration-to-pure-rust.md`](migration-to-pure-rust.md).

Le daemon `holafd` et son client Python sont déjà en place ; l'UI Rust vient
**après**, et doit pouvoir être essayée sans renoncer à l'UI Qt.

### 8.1 Le commutateur (implémenté)

`main.py` reste le point d'entrée unique :

```python
# src/launcher.py — la règle réelle, défaut : rust
if launcher.python_ui_forced():        # HOLAF_UI=python / HOLAF_USE_PYTHON=1
    run_python_ui()
else:
    result = launcher.launch(sys.argv[1:])   # résout holaf-ui et l'exécute
    if result.launched:
        sys.exit(result.exit_code)           # code de sortie propagé
    logger.warning("Rust UI unavailable (%s) — starting the classic Python UI", result.reason)
    run_python_ui(result.reason)
```

Ordre de résolution de `holaf-ui` (calqué sur celui de `holafd`) :
`$HOLAF_UI_BIN` → bundle PyInstaller (à côté de l'exécutable, puis
`sys._MEIPASS`, puis `_internal/`, avec restauration du bit exécutable) →
`rust/target/{release,debug}` → `$PATH`.

Règles, calquées sur celles déjà en place pour le moteur
(`HOLAF_USE_PYTHON=1`, daemon Rust par défaut) :

1. ~~défaut = python~~ → **défaut = rust** depuis B1 (l'échappatoire reste) ;
2. **repli automatique** : binaire `holaf-ui` manquant ou non exécutable ⇒ UI
   Qt, avec un WARNING qui porte la raison *et* un message dans la barre de
   statut (un bundle packagé n'a pas de stderr).  Un binaire qui **démarre**
   n'est jamais doublé : son code de sortie est propagé, donc « pas d'écran »
   affiche le message d'eframe au lieu d'ouvrir Qt ;
3. **jamais deux UI en même temps** : l'UI Rust pilote PipeWire **dans son
   processus** (pas de démon, pas de socket) ; l'UI Qt est le seul cas où
   `holafd` est lancé, et elle ne tourne que si `holaf-ui` n'a pas démarré ;
4. **config.json reste la source de vérité** : l'UI Rust lit/écrit le même
   fichier via `holaf_core::config` + `migration::strip_from_dict`.  Comme
   `default_config_path()` côté Rust part du **répertoire courant** (alors que
   l'UI Qt part de son propre chemin), le lanceur épingle `--config` sur le
   fichier de l'UI Qt : c'est ce qui rend la bascule réversible.

### 8.2 Ce que l'UI Rust ne refait pas

| Domaine | Reste où c'est | Pourquoi |
|---|---|---|
| Graphe PipeWire, FX, routage | `holafd` (déjà Rust) | l'UI ne parle jamais à PipeWire directement |
| Métrage | capture Python/`holaf-pw` selon le jalon | `meter_sources()` ne nomme que les sources |
| Diagnostic | Python (`src/diagnostics/`) | collecte pure, déjà testée |
| Packaging | PyInstaller existant | l'UI Rust sera un binaire séparé embarqué dans le bundle |

Le MIDI n'est plus dans cette liste : le port est fait (§10ter), et l'**icône de
tray** l'est aussi (§10quater) — **activée par défaut au build** (elle ajoute
GTK3 + libappindicator, que le script de build sait contourner en réessayant
sans la feature).

### 8.3 Plan par étapes

| Étape | Livrable | Critère de sortie |
|---|---|---|
| **0 (fait)** | squelette compilable : thème + résolution + widgets + vue principale + capture PNG + tests | `cargo test -p holaf-ui` vert, capture produite |
| **1 (fait)** | parité de **lecture** : `AppState::from_config` charge `config.json` (migrations, clés inconnues), la géométrie et le thème ; `Backend::device_list` alimente le choix de device | `holaf-ui-snapshot --config ../config.json` rend les 7 vraies strips |
| **2 (fait)** | parité d'**écriture** : `holaf-ui` pilote `PipeWireBackend` en direct (`default = ["engine-pw"]`), VU réels par capture `pw-cat`, rollback sur refus, mode déconnecté | volume/MUTE/MONO/FX/routage/device agissent sur le graphe ; `tests/engine_live.rs` vert |
| **3 (fait)** | dialogues : renommer, ajout/suppression, paramètres FX, choix de device, **sélection d'applications**, **diagnostic** ; menus contextuels fenêtre/strip/FX ; always-on-top | aucune fonction Qt perdue (checklist de `ui-inventory.md` §9) |
| **4 (fait)** | MIDI complet : mapping CC/note, **learn par propriété** (menu du bouton MIDI), feedback LED, **choix du port** + reconnexion automatique, `midi` dans les features par défaut | un AKAI MIDI Mix pilote le mixeur sans re-apprentissage (§10ter) |
| **5 (fait, B1)** | bascule du défaut à `HOLAF_UI=rust`, `HOLAF_UI=python` en échappatoire ; `holaf-ui` packagé (spec, tarball, PKGBUILD, `.desktop`, CI) | une release complète sans régression signalée (en attente de validation utilisateur) |
| **6 (fait, B2)** | **icône de tray** (feature `tray`, **activée par défaut au build** depuis le 2026-11-14) : menu Qt, « fermer = cacher » seulement si un tray existe, icône **générée** en RGBA (aucune dépendance fichier), repli sans display testé, pile GTK3/GLib **retirée du bundle PyInstaller** (le système fournit la sienne) | `cargo test -p holaf-ui --features tray` vert + `cargo build --features tray` en CI (§10quater) |

Chaque étape ajoute des tests **sans** toucher aux 207 tests Python
(`python -m unittest discover -s tests` reste la référence) ; le workspace Rust
est passé de 349 à **569** tests, dont **342** pour `holaf-ui` (322 unitaires +
19 d'intégration — `midi_config` pour la compatibilité `config.json`, `midi_live`
pour le port, `engine_live` pour le moteur — + 1 doctest), et **343** avec
`--features tray` (le test de repli sans display ne se compile que là).

---

## 9. Écarts connus (état actuel)

| Manque | Effet visible | Étape |
|---|---|---|
| **tray icon** | **fait** (2026-11-14), **activé par défaut au build** (`scripts/build_rust.sh`, donc PKGBUILD/tarball/CI ; `HOLAF_NO_TRAY=1` pour l'enlever) : sans hôte appindicator, ou sans display, il n'y a pas d'icône et fermer la fenêtre **quitte** — choix sûr, cacher rendrait l'app inatteignable. Avec un tray réel, `close_action()` cache la fenêtre (préférence `ui_rust.tray_hide_on_close`, défaut `true`). Sous GNOME/Wayland les **clics** ne sont pas livrés : le **menu** reste la voie d'interaction. Voir §10quater | fait |
| `pw-dump` dans le diagnostic | le port Rust ne lit pas `pw-dump` : le **démon** publie déjà son propre graphe (`status().holaf_nodes`, `strips[].node_name`, `pending`), ce qui couvre les constats utiles. En revanche le **quantum** est publié (`status.timing`, depuis `pw-metadata -n settings` : `rate`, `quantum`, `force-quantum`, min/max) et `small_quantum` (< 256) est actif ; les compteurs **ERR** ne sont disponibles que si `pw-top -b` existe (PipeWire ≥ 0.3.66) — en 0.3.65 le binaire n'a pas de mode batch, donc `pwt_errors` reste muet et `pw_top_unavailable` (info) affiche la raison exacte. Rien n'est inventé | assumé |
| routage applicatif côté démon | **fait** : `Backend::app_list` liste les flux de sortie audio (`pactl -f json list sink-inputs` + `list sinks` pour résoudre le `node.name` du bus) en excluant les flux du démon (son pid, ses enfants — les `pw-cat` de métrage —, ses nœuds `Holaf_*` et l'app Qt), et `app_move` déplace le flux via le gestionnaire de session (`pactl move-sink-input`) **puis relit** la liste pour vérifier que le flux est bien sur le bus ; `stub: true` n'est plus renvoyé | fait |
| apprentissage MIDI par propriété | **fait** : le bouton MIDI d'un strip ouvre le menu Qt (`Learn Volume` / `Learn Mute` / `Learn Mono`, `Clear Mappings`, plus `Cancel learn` quand un learn est armé) ; voir §10ter | fait |
| police « gras » simulée | deux tracés décalés au lieu d'une vraie graisse | 3 |
| boutons FX à 16 px sur les entrées | Qt les fixe à 22 (118 px de boutons dans 90 px : les derniers sont rognés) — le port les rétrécit pour tous les voir | assumé |
| plage `HF key filter` élargie à 24 kHz | Qt clampe à 20 kHz, ce qui réécrivait le défaut du plugin (23520 Hz) à l'ouverture du dialogue | assumé |
| géométrie de fenêtre | seule la **hauteur** est relue/écrite (`window.height`), et seulement après un resize réel ; le blob Qt `window_geometry` et l'état Qt `window_state` sont préservés tels quels ; `window.width` n'est jamais ajouté | assumé |

Ces écarts sont **choisis et documentés** : aucun n'est un oubli silencieux.

---

## 10. Vérification

```bash
cd rust
cargo build -p holaf-ui                          # défaut : moteur PipeWire + MIDI
cargo build -p holaf-ui --no-default-features    # UI seule (ni libpipewire ni libasound)
cargo build -p holaf-ui --features midi --no-default-features   # MIDI sans moteur
cargo build -p holaf-ui --features tray          # l'icône de tray (ce que le script de build ajoute)
cargo test  -p holaf-ui                          # 342 (322 unitaires + 19 d'intégration + 1 doctest)
cargo test  -p holaf-ui --features tray          # idem + le repli sans display
cargo test  --workspace                          # 569 tests
cargo clippy -p holaf-ui --all-targets           # 0 warning (idem toutes les combinaisons)
cargo run   -p holaf-ui -- --midi-list           # les noms de ports que --midi accepte
cargo run   -p holaf-ui --bin holaf-ui-snapshot -- --out /tmp/holaf-ui.png
cargo run   -p holaf-ui --bin holaf-ui-snapshot -- --config ../config.json --height 900
```

### Captures de dialogues

Le même binaire sait ouvrir un dialogue avant de rendre, avec un **moteur mock**
déterministe (`status`, `device_list`, `app_list` canned) : le rendu ne dépend
donc pas d'un démon.

```bash
cargo run -p holaf-ui --bin holaf-ui-snapshot -- \
    --config ../config.json --dialog diagnostics --dialog-tab engine \
    --out ../docs/ui-rust-dialog.png --height 700
# --dialog : diagnostics | apps | fx | device | add | delete | menu
#            midi-port | midi-port-none | midi-learn | midi-menu
# --dialog-tab : summary | engine | graph | system
```

Un dialogue est une `egui::Area` : il flotte **au-dessus** de la fenêtre et ne
l'élargit jamais. La capture verrouille donc la zone de la fenêtre à
`layout::required_width` (794 px pour les sept strips) même quand la toile est
plus large — sans ce verrou, rendre la mise en page calculée pour 794 px dans
une toile de 900 px étirait les deux colonnes de `900/794` et laissait ~72 px de
fond à droite du dernier strip (au lieu des 12 px de la formule Qt : 10 px de
marge + 2 px de jeu). `--width 900` agrandit désormais la **toile** seulement :
la fenêtre garde sa largeur verrouillée et les colonnes en trop sont peintes en
`#141414` (« hors fenêtre »), avec une note sur la sortie d'erreur.

### Test live (harnais `tests/engine_live.rs`)

Le seul test qui prouve que le câblage est réel ; il est **sauté** sans démon
headless (même garde que `holaf-pw/tests/backend_live.rs`) :

```bash
eval "$(./scripts/start-pipewire-headless.sh --export)"
cd rust && cargo test -p holaf-ui --test engine_live -- --nocapture
```

Il charge un config à 2 strips, attache le vrai `EngineBridge`, puis vérifie :
le bus `Holaf_Bus_UI_Out` existe ; un drag de fader simulé laisse
`channelVolumes = 0.44³` (courbe PulseAudio, pas 0.44 brut) côté démon ; un
toggle FX charge `Holaf_FX_out-bus` ; un sinus 1 kHz joué dans le bus fait monter
le VU puis l'enveloppe le ramène à zéro ; `shutdown()` ne laisse **aucun** nœud
`Holaf_*` ni enfant `pw-cat`.

Il couvre aussi les deux fonctions « quotidiennes » : `app_list`/`app_move`
contre le démon réel (l'answer doit être **capable** — `stub: false`,
`source: pactl` — même quand rien ne joue dans le bac à sable) et la fenêtre de
diagnostic, dont le rapport doit décrire le **vrai** graphe (`Holaf_Bus_UI_In`),
sans constat `daemon_unreachable`.

Le déplacement d'un flux est prouvé séparément, en live, par
`holaf-pw/tests/app_routing_live.rs` : un `pw-cat --playback` détaché est listé,
déplacé sur le bus d'un strip (`app_move`), puis **relu** sur ce bus ; trois flux
au profil « à nous » (notre pid, un pid enfant, un `node.name` `Holaf_*`) sont
exclus avec leur raison. Sur PipeWire 0.3.65 `pw-top` n'a pas de mode batch, donc
les compteurs ERR restent indisponibles — le rapport l'affiche comme tel
(`pw_top_unavailable`), il n'invente pas de zéro.

Les captures de référence sont versionnées ici :
[`docs/ui-rust-snapshot.png`](./ui-rust-snapshot.png) (600 px de haut),
[`docs/ui-rust-snapshot-tall.png`](./ui-rust-snapshot-tall.png) (900 px de haut)
et [`docs/ui-rust-dialog.png`](./ui-rust-dialog.png) (fenêtre de diagnostic,
onglet *Engine*).

---

## 10bis. Lot « usage quotidien » (implémenté après le squelette)

Sept briques, chacune avec sa politique **pure** (donc testée sans démon, sans
fenêtre et sans matériel) et son câblage dans `AppState` :

| module | rôle | politique testable |
|---|---|---|
| `devices.rs` | identité d'un device : le `node.name` meurt au reboot/re-plug, la **description** et la structure du nom survivent | `resolve()` (nom exact → casse → description → score vendor/profil/identité, refus si deux candidats), `reconcile()` (réparation + liste des manquants), `presence()` (`(Not Found)` **seulement** si l'engine a répondu) |
| `config.rs` | la hauteur de fenêtre vit dans `window.height` (clé partagée avec Qt) ; `ui_rust.always_on_top` est la seule clé propre au port Rust ; `assigned_apps` et `midi_*` restent des clés UI-only préservées | round-trip + « le blob `window_geometry` de Qt est intact » |
| `menus.rs` | menus contextuels fenêtre / strip / bouton FX, mêmes entrées et même ordre que `build_app_context_menu`, + « Open project page » (`xdg-open` → `gio open` → `sensible-browser`, sans dépendance) | `entries(kind)` / `labels(kind)` / `action_for()` |
| `diagnostics.rs` | rapport structuré depuis `status()` + constats automatiques (`daemon_unreachable`, `engine_not_running`, `graph_degraded`, `graph_pending`, `fx_host_down`, `fx_host_respawns`, `fx_host_inprocess`, `mainloop_pumps_stalled/zero/external`, `legacy_bus_names`, `strip_bus_unnamed`, `strip_bus_missing`, `device_missing`, `device_not_listed`, `metering_no_source`, `multiple_daemons`) | `Report::collect(&Inputs)` + `detect()` ; texte copiable, constats en tête. `strip_bus_missing` ne juge que les strips **virtuels** et `mainloop_pumps_*` que le mode FX **inprocess** (cf. `src/diagnostics/collector.py`, mêmes règles) |
| `apps.rs` | « SELECT APPS » + enforcement : `running ∪ assigned`, `enforces()` (entrée virtuelle non-default), `plan()` (idempotent, une app assignée deux fois n'est déplacée qu'une fois), bus = `status.strips[].node_name` sinon `Holaf_Bus_<Label>` | `AppList::parse`, `plan()`, `AppSelection` |
| `midi.rs` | CC → volume (`value/127`), note → bascule mute/mono sur **press** seulement, learn par propriété, feedback LED, choix/reconnexion du port (`midir`, feature `midi` **par défaut**) — voir §10ter | `MidiMessage::parse`, `decode()`, `feedback()`, `MidiMap`, `choose_port()`, `MidiRuntime` |
| `tray.rs` | politique du tray (entrées, « fermer = cacher seulement si un tray existe »), always-on-top, **icône générée en RGBA** — feature `tray` **activée par défaut au build** (`HOLAF_NO_TRAY=1` pour l'enlever) — voir §10quater | `close_action()`, `menu_action()`, `icon_rgba(size)` (pixels déterministes, testés sans GTK) |

Côté `AppState` : `SelectAppsDone` (persiste `assigned_apps` puis déplace),
`tick_app_enforcement()` (une question `app_list` toutes les 5 s, seulement si un
strip prend des apps), `RestartEngine` (anti-réentrance, `Connecting` →
`Restarted`), `OpenDiagnostics`/`refresh_diagnostics()`, `handle_midi_message()`,
`SetAlwaysOnTop` (un `ViewportCommand::WindowLevel` par changement).

---

## 10ter. MIDI : le AKAI MIDI Mix comme contrôleur principal

Le MIDI n'est pas un extra : c'est **la** façon de mixer de l'utilisateur (huit
faders + vingt-quatre boutons). Trois conséquences pour l'implémentation : le port
doit être là **par défaut**, les mappings déjà écrits par l'UI Python doivent être
**repris tels quels**, et l'assignation d'un contrôle doit se faire **en le
jouant** (aucun CC codé en dur — le MIDI Mix n'est qu'une convention).

### 10ter.1 Où vit quoi

| Couche | Rôle | Testé sans matériel |
|---|---|---|
| `midi.rs` (toujours compilé) | `MidiMessage::parse` (note_on/off, CC ; clock/sysex ignorés), `Binding` ↔ JSON, `MidiMap` (clé `type × canal × id`), `decode()` (CC → `value/127`, note → bascule sur **press**, throttle 10 ms/strip), `feedback()` / `button_feedback()` (LED), `choose_port()`, `MidiRuntime` (machine à états de connexion) | oui — 15 tests unitaires (`cargo test -p holaf-ui --lib midi::tests`) |
| `midi.rs` (feature `midi`) | `MidiPort` (midir : un port in, un port out, un thread), `Ports::ports()` (in + out), `MidiVirtualSource` | `tests/midi_live.rs` (auto-sauté sans séquenceur ALSA) |
| `AppState` | `handle_midi_message()` → `UiAction` (donc persisté/rollback/engine comme un clic), `midi_learn()`, `clear_midi_bindings()`, `midi_summary()` | oui — via `handle_midi_message` directement, sans port |
| `main.rs` (`MidiDevice`) | ouverture, sondage du port toutes les 2 s, reconnexion, LEDs | non (I/O) |

La règle de découpage est celle du reste de la crate : la **politique** est pure et
testée partout, l'**I/O** est dans le binaire.

### 10ter.2 Format de persistance (compatible Python, vérifié)

`config.json` porte trois clés par strip, écrites par `Strip.to_dict` et relues
par `Strip.from_dict` (`src/models/strip_model.py`) :

```json
"midi_volume": {"channel": 0, "control": 61, "type": "control_change"},
"midi_mute":   {"channel": 0, "note": 24,    "type": "note_on"},
"midi_mono":   {"channel": 0, "note": 22,    "type": "note_on"},
"midi_volume": null                                    // non mappé
```

Canal et identifiants sont **0-based** (l'UI affiche `ch.1` pour le canal 0).
`midi_engine.py` (`send_feedback`) écrit des `note_on` 127/0 pour une LED de
bouton, ou un CC 127/0 pour un contrôleur qui pilote ses LEDs en CC.

Côté Rust, `Binding::to_json`/`from_json` produisent exactement ces objets, et
`UiConfig` les traite comme des clés UI-only (elles sont **recopiées** au save,
jamais reconstruites depuis `Strip`). Preuve par test
(`tests/midi_config.rs`) :

* `the_python_config_is_loaded_with_its_mappings` — le **vrai** `config.json` du
dépôt, chargé par `AppState::from_config` : les 18 mappings des 6 strips mappés,
les `null` du bus virtuel, et la résolution `CC 19 → Mic`, `CC 23 → Default`, …
* `a_save_keeps_the_python_shape` — un save réécrit les trois clés de **chaque**
  strip à l'identique (objet par objet, `null` compris) ; le blob Qt et `mode`
  sont intacts ;
* `the_mappings_survive_a_volume_change` — le save routinier (déplacement de
  fader) ne les perd pas ;
* `a_bare_number_mapping_is_understood_and_repaired` — la seule autre forme
  imaginable (`"midi_volume": 61`, sur quoi l'UI **Python plante** :
  `TypeError: 'int' object is not subscriptable` dans `_rebuild_midi_lookup`) est
  lue (canal 0 + forme naturelle de la cible) et **réparée** au premier save en
  objet.

Le **port** n'a pas de clé côté Python : `MainWindow._init_midi` reprenait à
chaque démarrage le premier port dont le nom contient `mix`, sinon le premier
tout court. Le port Rust ajoute donc sa propre préférence —
`ui_rust.midi_port` — que Python recopie sans le lire (`load_config`/
`save_config` ne réécrivent que `strips[]` et `window_geometry`).

### 10ter.3 Sélection du port, démarrage, reconnexion

1. **Démarrage** : `ui_rust.midi_port` présent et dans la liste → ouvert. Absent
   (**jamais choisi**) → `choose_port()` reconnaît le MIDI Mix (`midi mix` puis
   `mix`), sinon l'unique port s'il n'y en a qu'un, sinon rien ; le choix est
   **mémorisé** (premier démarrage « plug and play »). Absent de la liste →
   avertissement + liste des ports dans le log, et le nom reste attendu.
   `null` (choix explicite « No controller ») n'est **jamais** écrasé.
2. **Log de démarrage** : chaque port in/out est listé avec son nom exact
   (`midi:   in  MIDI Mix:MIDI Mix MIDI 1 24:0`), ce qui est précisément ce que
   `--midi PORT` et le sélecteur attendent.
3. **Menu fenêtre → `MIDI port...`** : la liste des entrées + « No controller » ;
   la sélection est écrite dans `ui_rust.midi_port` et appliquée à la frame
   suivante (aucun redémarrage).
4. **Reconnexion** : `MidiRuntime` (pur) impose un essai immédiat puis **toutes
   les 2 s** (`RETRY_INTERVAL`) tant que le port est absent ou a disparu. La
   présence est vérifiée en re-listant les ports (midir ne signale pas un
   débranchement) ; un échec de *listage* (pas de séquenceur ALSA du tout) est une
   condition statique : backoff à 10 s pour ne pas inonder la console. L'état
   courant est dans la barre d'état (`MIDI: <port> unavailable (...) - retrying
   every 2 s`).
5. **`--midi PORT`** force un port pour la session (non persisté) ;
   **`--midi-list`** imprime les entrées et les sorties (LED) avec leur nom exact
   et échoue proprement s'il n'y a pas de séquenceur.

### 10ter.4 Learn : le parcours, identique à Qt

| Étape | Qt (`strip_widget._show_midi_menu` + `midi_engine`) | Rust |
|---|---|---|
| Ouvrir le menu | clic sur le bouton `MIDI` | clic (ou clic droit) sur le même bouton → même menu |
| Entrées | `Learn Volume`, `Learn Mute`, `Learn Mono`, — , `Clear Mappings` | identiques, **+ `Cancel learn`** en tête quand un learn est armé (Qt n'avait aucun retour en arrière) |
| Armer | `start_learning(uid, prop)` + bouton orange `LEARNING...` | `MidiLearnTarget` → `midi_learning = Some((uid, target))`, même bouton orange (le **cadre du strip** passe en orange aussi, comme `set_learning`) |
| Capturer | premier `control_change` ou `note_on/off` → `learning_mode = False` | `decode()` renvoie `Learned` ; **le learn s'arrête** (un geste = une propriété) |
| Stocker | `strip.midi_volume|mute|mono = mapping` + `_save_state()` | `set_midi_binding` + `request_save` (même forme JSON) |
| Feedback | LED du bouton synchronisée | idem, **+ le bouton affiche `CC 19` / `N 24` pendant 1,5 s** (vert) — Qt revenait à `MIDI` sans confirmation |
| Voir les mappings | rien (il fallait lire `config.json`) | **survol du bouton** : `CC 19 ch.1 -> volume`, `N 24 ch.1 -> mute`, … + le contrôleur connecté |
| Effacer | `Clear Mappings` (les trois) | identique (`null` dans le fichier, comme Python) |

Différences **assumées** : `Cancel learn`, le flash du CC, l'infobulle et la
mémorisation du port. Aucune ne change le fichier ni le comportement observable
côté contrôleur.

### 10ter.5 Retour LED

Oui, le Python le fait (`MidiEngine.send_feedback`, appelé par
`_sync_initial_midi_leds`, `on_midi_message_received` et les toggles souris) :

* un mapping `note_on` s'allume en `note_on` vélocité **127**, s'éteint à **0** ;
  un mapping CC en `control_change` valeur 127/0 — c'est la forme du binding,
  donc la LED suit son propre bouton ;
* les boutons `mute`/`mono` de tous les strips sont resynchronisés à l'ouverture
  du port, puis **une fois par seconde** (un MUTE cliqué à la souris, un strip
  ajouté, un undo : la lampe doit suivre le mixeur, pas le dernier événement
  MIDI) ;
* un mapping `volume` n'a pas de LED (un fader n'en a pas).

Les octets partent par un **thread d'écriture** dédié : un contrôleur qui cesse
de drainer ne bloque jamais l'UI.

### 10ter.6 Ce qui est testé, et ce qui ne peut pas l'être ici

Tests **sans matériel** (tout le reste) : parsing, mapping, learn par propriété,
persistance JSON, throttle, LED, choix de port, machine de reconnexion (temps
injecté), résumé d'infobulle, `config.json` réel chargé et réécrit à l'identique.

Non testable dans ce conteneur : **l'I/O MIDI réelle**. `/dev/snd/seq` est absent
et il n'y a ni `modprobe` ni `CAP_SYS_MODULE` ; donc ni `snd-virmidi`, ni
`snd-aloop`, ni `midir::create_virtual` (qui exige le séquenceur ALSA) : aucun port
virtuel n'est créable. `tests/midi_live.rs` contient les trois tests qui
l'exerceraient (listage exact des noms, erreur propre sur un port absent, un
contrôleur virtuel → `CC 19 / 100` reçu par `MidiPort`) ; sans séquenceur ils
**sautent** en imprimant la raison, et ils tournent réellement sur un poste :

```bash
cd rust && cargo test -p holaf-ui --test midi_live -- --nocapture
```

### 10ter.7 Brancher un AKAI MIDI Mix (chez l'utilisateur)

```bash
# 1. les noms de ports vus par ALSA ("MIDI Mix:MIDI Mix MIDI 1 24:0")
cargo run -p holaf-ui -- --midi-list
#    (équivalent système : aconnect -l  /  aseqdump -l)

# 2. lancer l'UI : le MIDI Mix est reconnu et mémorisé au premier démarrage
cargo run -p holaf-ui -- --config ../config.json
#    log attendu : "midi: selected \"MIDI Mix:...\" automatically"
#                   "midi: listening on \"MIDI Mix:...\""

# 3. forcer un port pour une session (sans le mémoriser)
cargo run -p holaf-ui -- --midi "MIDI Mix:MIDI Mix MIDI 1 24:0"

# 4. vérifier le learn : clic sur le bouton MIDI d'un strip (ou clic droit)
#    → "Learn Volume" → bouger le fader voulu → le bouton affiche "CC 19"
#    → le fader du mixeur suit ; survoler le bouton liste les mappings

# 5. changer de contrôleur : menu fenêtre (clic droit sur le fond) → "MIDI port..."

# 6. débrancher/rebrancher : reconnexion automatique en 2 s, état dans la barre
#    (`MIDI: <port> unavailable (...) - retrying every 2 s`)
```

Les mappings déjà présents dans `config.json` sont utilisés **sans
re-apprentissage** : c'est ce que verrouille `tests/midi_config.rs`.

---

## 10quater. Tray : icône de zone de notification (feature `tray`)

Le tray est la dernière brique Qt portée.  Il vit dans `src/tray.rs` et se
découpe en deux : une **politique** toujours compilée (pure, testée en toutes
configurations) et une **I/O GTK** derrière la feature `tray`.

### 10quater.1 Activation et dépendances

**Activée par défaut au build** (décision du 2026-11-14) :
`scripts/build_rust.sh` ajoute `--features tray` à `holaf-ui` sauf
`HOLAF_NO_TRAY=1` (ou l'ancienne `HOLAF_TRAY=0`).  `build.sh`, le PKGBUILD, le
tarball et le workflow CI passent donc tous par ce script : ce qui est livré
contient l'icône.  Si la compilation avec la feature échoue (GTK3 dev absent),
le script **réessaie sans** : on garde `holaf-ui`, on perd l'icône, et un
WARNING le dit.

```bash
./scripts/build_rust.sh                   # défaut : AVEC l'icône
HOLAF_NO_TRAY=1 ./scripts/build_rust.sh   # sans l'icône

cargo build -p holaf-ui --features tray     # équivalent manuel
cargo build -p holaf-ui                      # défaut : pas d'icône, tout le reste identique
```

La feature n'est **pas** dans les `default` de la crate (`default = ["engine-pw",
"midi"]`) : un `cargo build` brut ne l'inclut pas, c'est le script de build qui
l'ajoute.  Ça garde `cargo test --workspace` (et les 569 tests) hermitique sur
une machine sans GTK3, sans renoncer à livrer l'icône.

| Dépendance | Rôle | Particularité |
|---|---|---|
| `tray-icon 0.19` (`default-features = false`) | icône + menu (`muda` ré-exporté) | `default-features = false` **retire `libxdo`** : cette feature ne sert qu'aux items prédéfinis Copy/Paste, que l'app n'utilise pas.  Aucune dépendance de build ni `dlopen` de `libxdo` au runtime |
| `gtk 0.18` | boucle GTK sur le thread du tray | version alignée sur celle qu'exige `tray-icon 0.19` |

Paquets système : **`gtk3`** (build + link : `libgtk-3.pc`, `libgdk-3.pc`,
`pango`, `cairo`) et **`libayatana-appindicator`** (runtime :
`libappindicator-sys` `dlopen()` `libayatana-appindicator3.so.1` ; Arch n'a pas
de `libappindicator-gtk3`, c'est AUR-only).  Un hôte appindicator est nécessaire
pour que l'icône **s'affiche** : `gnome-shell-extension-appindicator`,
`kstatusnotifieritem` (KDE), ou le `tray` de `swaybar`.  Les deux premiers sont
en `depends` du PKGBUILD, l'hôte en `optdepends`.

**Ces bibliothèques ne sont jamais embarquées dans le bundle** :
`packaging/holaf-mix.spec` retire de `a.binaries` la pile GTK3/GLib
(`libgtk-3`, `libgdk-3`, `libgdk_pixbuf`, `libpango*`, `libcairo*`, `libatk*`,
`libatspi`, `libepoxy`, `libharfbuzz`, `libglib/gobject/gio/gmodule/gthread`,
les `libX*` que seule GTK tirait et l'équivalent `libayatana` s'il apparaît),
exactement comme il le fait déjà pour `libpipewire`/`libspa`/`libpulse`.
Mesuré sur ce conteneur (bundle onedir, `--features tray`) : **37 bibliothèques**
retirées, 117 → 80.  Raison : `holaf-ui` **lie** `libgtk-3.so.0`, et PySide6
fournit en plus `plugins/platformthemes/libqgtk3.so` (donc la pile GTK était
déjà figée avant le tray) ; le bootloader préfixe `LD_LIBRARY_PATH` avec
`_MEIPASS`, donc une GTK3/GLib figée s'impose à celle du système → thème,
plugins, schémas GSettings et hôte appindicator incohérents (le bug
libpipewire, en pire).

### 10quater.2 Threading

libappindicator exige une **boucle GTK qui tourne**, et eframe possède le thread
principal.  `install()` lance donc un thread `holaf-tray` qui fait
`gtk::init()` → construit l'icône + le menu → `gtk::main()`, et rend son verdict
sur un canal (timeout 5 s).  Seuls deux objets traversent les threads : le canal
global `muda::MenuEvent` (`Send`) et le drapeau `available()` (un atome).
`poll()` — appelé **une fois par frame** par le shell, comme la pompe MIDI — vide
ce canal, donc **l'UI thread ne touche jamais GTK**.

### 10quater.3 Menu, fermeture, événements

| Entrée | Id stable | Action |
|---|---|---|
| `Show/Hide` | `holaf.tray.show_hide` | `ViewportCommand::Visible(!window_visible)` |
| `Restart audio engine` | `holaf.tray.restart_engine` | `AppState::restart_engine` (même action que le menu contextuel) |
| `Quit` | `holaf.tray.quit` | `ViewportCommand::Close` → `on_exit` → `AppState::shutdown()` |

`close_action(available, hide_on_close)` encode la règle de `closeEvent` de Qt :
**cacher** seulement si `available()` **et** la préférence
`ui_rust.tray_hide_on_close` (défaut `true`) ; sinon **quitter**.  Cacher sans
moyen de revenir laisserait un processus inatteignable — c'est précisément ce que
le code Qt évitait avec `isSystemTrayAvailable()`.

`menu_action(id)` est une fonction pure : un id inconnu (menu d'une autre app,
entrée future) est **ignoré**, jamais mal routé.

### 10quater.4 L'icône est générée, pas chargée

`tray::icon_rgba(size) -> (Vec<u8>, u32, u32)` dessine l'icône **pixel par
pixel** : tuile ardoise à coins arrondis + trois faders verticaux avec un
capuchon menthe à des hauteurs différentes, lisible à 22 × 22.  Aucune
dépendance fichier, aucun décodeur d'image, aucun appel à `assets/icons/` :

* les PNG du dépôt vivent sous **`dist/`**, qui est **gitignoré** — un checkout
  CI propre n'en a aucun, donc un `include_bytes!("…/dist/icons/holaf-mix-32.png")`
  ferait échouer `--features tray` en CI tout en passant en local ;
* le glyphe est **déterministe** et testé sans GTK (dimensions, marges
transparentes, présence des trois capuchons, clamp `8..=256`) : ces tests
  tournent dans **les trois** configurations de build.

### 10quater.5 Ce qui n'est pas vérifiable ici (Wayland)

Sous Wayland sans hôte appindicator, libappindicator « réussit » mais rien ne
s'affiche ; et `tray-icon` ne livre **aucun** événement de clic hors X11 (côté
GNOME/Wayland, les clics sur l'icône sont ignorés).  C'est pourquoi
l'interaction fiable est le **menu** (pas un toggle au clic) et pourquoi
`with_menu_on_left_click(true)` ouvre le menu au clic gauche — le menu
`Show/Hide` / `Restart audio engine` / `Quit` reste donc utilisable sous
Wayland, même si l'utilisateur est sous GNOME/Wayland (le cas de l'auteur).
Le conteneur de dev n'ayant **ni X11 ni Wayland**, seul le **repli** est
vérifié ici : `install()` journalise un avertissement
(`tray: unavailable (gtk::init failed: Failed to initialize GTK) - closing the
window will quit, not hide`), ne panique pas, `available()` reste `false` et
`close_action()` renvoie `Quit`.  Vérifié aussi **depuis le bundle PyInstaller**
(onedir) : le `holaf-ui` embarqué résout la GTK3/GLib **du système** (retirées du
bundle), démarre, et échoue proprement sur « no display server ».  Le rendu réel
de l'icône dans un panneau (GNOME + extension, KDE, sway/swaybar) et la
réception des clics sous X11 restent à confirmer sur une session graphique.

---

## 11. Raccourcis

* Inventaire chiffré : [`docs/ui-inventory.md`](./ui-inventory.md)
* Spec de la réécriture Rust : [`docs/rust-rewrite-spec.md`](./rust-rewrite-spec.md)
* État des jalons moteur : [`docs/rust-rewrite-status.md`](./rust-rewrite-status.md)
* Guide du workspace Rust : [`rust/README.md`](../rust/README.md)
