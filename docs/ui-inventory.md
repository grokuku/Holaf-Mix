# Inventaire de l'UI PySide6 — valeurs mesurées

Relevé chiffré de l'interface actuelle (`src/ui/main_window.py`,
`src/ui/widgets/strip_widget.py`, `src/ui/dialogs/effect_settings_dialog.py`),
fait pour porter l'UI vers Rust/egui **sans réinventer les valeurs**.  Chaque
ligne indique d'où vient le chiffre dans le code Python, pour qu'on puisse le
revérifier plutôt que le croire.

Le port Rust implémente ces valeurs dans
[`rust/crates/holaf-ui/src/theme/mod.rs`](../rust/crates/holaf-ui/src/theme/mod.rs)
(`Dimensions`, `ThemeColors`, `Typography`) ; les tests
`default_theme_matches_the_measured_palette` et
`theme_sizes_match_the_qt_stylesheets` verrouillent les constantes ci-dessous.

---

## 1. Fenêtre

| Élément | Valeur | Source |
|---|---|---|
| Titre | `Holaf-Mix` | `MainWindow.setWindowTitle` |
| Toujours au premier plan | oui (`Qt.WindowStaysOnTopHint`) | `__init__` |
| Largeur | **fixe**, calculée : `Σ largeur des sections` | `_adjust_window_size` → `setFixedWidth` |
| Hauteur | libre (géométrie restaurée) — et, dans le port Rust, **remplie par les strips** : le fader absorbe l'espace | `restoreGeometry` / `layout::available_strip_height` |
| Géométrie sauvegardée par défaut | ≈ **793 × 595** | `config.json` → `window_geometry` (`0x319` × `0x253`) |
| Marge du layout racine | 0, espacement 0 | `main_layout.setContentsMargins(0,0,0,0)` |
| Fond fenêtre | `#232323` | thème Rust (la fenêtre Qt n'a pas de fond propre : les sections le couvrent) |

### Largeur d'une section — formule exacte

```
largeur_section(n) = max( n*100 + max(0, n-1)*10 + 22 , 160 )
```

`STRIP_WIDTH = 100`, `SPACING = 10`, `MARGIN_TOTAL_PER_SECTION = 22`,
`MIN_SECTION_WIDTH = 160` (`_adjust_window_size`).  Les 22 px sont les 20 px de
`setContentsMargins(10, 10, 10, 10)` **plus 2 px de marge de sécurité**
(`QScrollArea` intercalé) : c'est bien 22, pas 20.  Vérifié : 4 strips → 452 px,
3 strips → 342 px, donc une fenêtre de **794 px** pour la config par défaut
(4 entrées + 3 sorties), élargie proportionnellement si la fenêtre est plus
large (`main_layout.setStretch`).

Ces valeurs sont reproduites à l'identique par `holaf_ui::layout::section_width`
(`QT_STRIP_WIDTH` / `QT_STRIP_SPACING` / `QT_SECTION_MARGIN_TOTAL` /
`QT_MIN_SECTION_WIDTH`) et verrouillées par un test (0, 1, 2, 3, 4, 7 strips).

---

## 2. Sections (`_create_section`)

| Élément | Valeur |
|---|---|
| Titre | `INPUTS` / `OUTPUTS`, gras, **14 px**, blanc |
| Marges du header | 10, 10, 10, 0 |
| Bouton « + » | **30 × 30**, rond (`border-radius: 15px`), fond `#444`, hover `#666` |
| Espace titre → bouton | 10 px |
| Fond de section (Qt) | `#2b2b2b` (entrées) / `#1e1e1e` (sorties) |
| Fond de section (thème Rust) | **identique** : `#2b2b2b` / `#1e1e1e` (neutres) |
| Marge du contenu | 10, 10, 10, 10 |
| Espacement entre strips | 10 px |
| Alignement | haut + gauche (`Qt.AlignTop | Qt.AlignLeft`) ; le `QSlider` vertical est extensible, donc la hauteur d'un strip suit la place disponible |
| Défilement | `QScrollArea`, sans cadre |

> **Les fonds de section sont neutres ; la couleur vient des strips.**
> Les deux colonnes sont deux gris (l'entrée plus claire que la sortie) : elles
> servent uniquement à séparer les deux moitiés de la fenêtre.  Toute la couleur
> de l'interface est portée par le dégradé `#444 → couleur du type` de chaque
> strip (§3).  Une version antérieure du thème Rust peignait les sections en
> bleu/rouge désaturés (`#4a6b80` / `#5a3a44`) : c'était une **mauvaise lecture
> d'une capture d'écran**, corrigée, et le test
> `theme::tests::section_backgrounds_are_neutral` interdit la régression.
> Preuve mesurée sur une capture de l'app Qt réelle : `#2b2b2b` à l'abscisse du
> fond INPUTS, `#1e1e1e` à celle du fond OUTPUTS.

---

## 3. Strip (`StripWidget`)

| Élément | Valeur | Source |
|---|---|---|
| Largeur | **100** (fixe) | `setFixedWidth(100)` |
| Marge interne | 5, 5, 5, **10** | `setContentsMargins(5,5,5,10)` |
| Espacement vertical | 5 | `layout.setSpacing(5)` |
| Bordure | 2 px `#555`, rayon **5** | stylesheet `_update_base_style` |
| Fond — entrée | dégradé vertical `#444444 → #3daee9` (haut → bas) | idem |
| Fond — sortie physique | dégradé vertical `#444444 → #e93d3d` | idem |
| Fond — sortie virtuelle (bus) | dégradé vertical `#444444 → #9b59b6` | idem |
| Fond — mode « learn » | `#444444 → #f39c12` + bordure orange `#f39c12` | idem |
| Dégradés du thème Rust | **identiques** aux quatre lignes ci-dessus | thème `holaf-dark` |
| Opacité du corps | **1.0** (opaque : les sections ne traversent pas un strip) | thème Rust |
| Hauteur naturelle (au repos) | ≈ **415 px** (rangées fixes 265 + fader 150 ; 242 + 150 sans la rangée `SELECT APPS`) | `layout::strip_fixed_height` + `fader_height` |
| Hauteur observée (capture de l'app Qt) | ≈ **555 px** dans une fenêtre de 615 px de contenu (fader ≈ 250 px) | capture 799 × 649 de l'app Qt |
| Hauteur dans le port Rust | **remplit la fenêtre** (décision utilisateur 2026-11-14) : le fader absorbe tout l'espace que les rangées fixes laissent, plancher 40 px | `layout::fader_height_for` |

### Sens du dégradé (vérifié, pas supposé)

`qlineargradient(x1:0, y1:0, x2:0, y2:1, stop:0 #444, stop:1 <accent>)` : en
QSS, `y1:0 → y2:1` va **du haut vers le bas**, donc

```
#444444 (gris, désaturé)  ← haut du strip
        ↓  dégradé
<accent> (couleur du type) ← bas du strip (juste au-dessus de MONO/MUTE/MIDI)
```

Vérifié deux fois :

1. **rendu Qt hors écran** du vrai stylesheet (`QFrame.grab()` puis lecture des
   pixels) : `#444647` en haut, `#3dace6` en bas d'un strip d'entrée de 394 px
   (hauteur naturelle de l'époque ; le port Rust l'étire désormais) ;
2. **capture de l'app qui tourne** : `#454446` en haut du strip *Mix Out*,
   `#9457ad` en bas (`#474444 → #da3e3e` pour un strip rouge).

Le strip se **désature donc vers le haut**.  Son pied paraît gris de toute
façon : les deux dernières rangées sont les boutons MONO/MUTE (`#444`) et MIDI
(`#333`), qui sont neutres par construction.

### Ordre vertical des rangées

1. **Header** : label (gras, **11 px**, blanc) centré + bouton « × » **16 × 16**
   (`color: #888`, hover `#ff5555`, `font-size: 14px`).
2. **Légende** : `SOURCE IN` / `DEVICE OUT` / `VIRTUAL BUS` — **8 px**, `#aaa`,
   marge haute 5 px.  Un bus utilise `#dcd0ff` **gras**
   (`_refresh_device_ui_state`).
3. **Combo device** : `QComboBox` — fond `#222`, bordure `#444`, rayon 3,
   **9 px**, padding 2.  Texte tronqué à **15 caractères** + `...`
   (`set_device_list`).  Entrée = `Apps / Virtual`, sortie = `Virtual Sink
   (Bus)`.  Un device absent s'affiche `"<nom> (Not Found)"`.
4. **Entrées seulement** : case `Default Sink` (**indicateur 10 × 10**,
   label 9 px `#ccc`, espacement 4), puis bouton `SELECT APPS`
   (fond `#555`, blanc, rayon 3, **9 px**, padding 2) — visible **seulement**
   si `device_name is None` **et** `not is_default`
   (`_update_app_btn_visibility`).
5. **Rangée FX** : `QPushButton` **22 × 18**, `font-size: 8px`, rayon 2,
   espacement 2 px.  5 boutons sur une entrée (`GT`, `RN`, `EQ`, `TB`, `CP`),
   3 sur une sortie (`EQ`, `TB`, `CP`) — `EffectKey::for_strip`.
6. **Zone de routage** (entrées) : cadre `rgba(0,0,0,0.3)`, rayon 3, marges 2,
   espacement 2 ; un bouton par sortie, **hauteur 20**, label =
   `label[:4].upper()`.  Sans sortie : label `No Outputs` (9 px, `#aaa`).
   Les sorties affichent à la place le label `BUS MASTER` (9 px, `#aaa`,
   marge 10 px).
7. **Fader + VU** (voir §4).
8. **MONO / MUTE** : hauteur 20, espacement 2 ; `MONO` **45 px** de large.
9. **MIDI** : pleine largeur, **hauteur 20**, `background: #333`,
   `color: #888`, `font-size: 10px` ; en mode learn : fond `#f39c12`, texte
   noir gras, label `LEARNING...`.

---

## 4. Fader et VU

| Élément | Valeur | Source |
|---|---|---|
| Disposition | `[VU 6 px] [gap 4] [slider] [gap 4] [VU 6 px]` | `fader_area_layout` |
| Largeur totale | 6 + 4 + 20 + 4 + 6 = **40 px** | (20 px = largeur utile du slider) |
| Gouttière (`groove`) | **6 px** de large, `#222`, rayon 2 | `QSlider::groove:vertical` |
| Poignée (`handle`) | **14 px** de haut, blanc, rayon 3, `margin: 0 -4px` | `QSlider::handle:vertical` |
| Partie « chargée » | `#555` (`add-page`) sous la poignée | idem |
| Plage | `0..=100`, volume = `value / 100` | `setRange` + `_on_slider_move` |
| Fond VU | `#222` | `VUMeterWidget.bg_color` |
| Dégradé VU | `0.0 → #ff3333` (rouge, haut), `0.2 → #ffff33`, `1.0 → #33ff33` (vert, bas) | `VUMeterWidget.__init__` |
| Ancrage du dégradé | sur **toute** la hauteur du widget, seule la partie `level` est révélée | `paintEvent` |
| Cadence de rafraîchissement | **40 ms** (25 Hz) | `METER_POLL_INTERVAL_MS` |
| Mono | `left = right = max(left, right)` | `update_vumeter` |

---

## 5. Palette (thème `holaf-dark`)

Toutes ces valeurs sont dans `ThemeColors::default()` et sont vérifiées une par
une par `theme::tests::default_theme_matches_the_pyside_stylesheets`.  La
colonne « source » est la règle QSS exacte : le thème Rust doit lui être
**identique**, sauf mention « thème Rust » (valeur que Qt n'a pas).

| Rôle | Couleur | Source QSS (fichier : règle) |
|---|---|---|
| Accent (FX actif) / texte | `#2ecc71` / `#ffffff` | `strip_widget.py` : `_update_fx_button_style` (actif) |
| FX inactif (fond / texte / bordure) | `#333333` / `#666666` / `#444444` | idem (inactif) |
| FX inactif au survol (fond / texte) | `#444444` / `#999999` | idem (`:hover`) |
| Routage actif (fond / texte / bordure) | `#4caf50` / `#ffffff` / `#4caf50` | `set_routing_targets` (`:checked`) |
| Routage inactif (fond / texte / bordure) | `#333333` / `#888888` / `#444444` | idem |
| Routage inactif au survol | `#444444` / `#ffffff` | idem (`:hover:!checked`) |
| MUTE engagé / texte | `#ff4444` / `#ffffff` (gras) | `_update_mute_style` |
| MUTE au repos (fond / texte) | `#444444` / `#ffffff` | idem |
| MONO engagé / texte | `#3daee9` / `#ffffff` (gras) | `_update_mono_style` |
| MONO au repos (fond / texte) | `#444444` / `#888888` | idem |
| MIDI (fond / texte) | `#333333` / `#888888` | `btn_midi` |
| MIDI au survol (fond / texte) | `#444444` / `#ffffff` | idem (`:hover`) |
| MIDI learn | `#f39c12`, texte noir gras | `set_learning` |
| Bouton neutre « + » (fond / hover) | `#444444` / `#666666` | `main_window._create_section` |
| « SELECT APPS » (fond / hover) | `#555555` / `#777777` | `strip_widget._init_ui` |
| Combo (fond / bordure) | `#222222` / `#444444` | `_style_combo` |
| Zone de routage | `rgba(0,0,0,0.3)` | `routing_container` |
| Texte primaire / secondaire / éteint | `#ffffff` / `#aaaaaa` / `#666666` | `QLabel`, légendes, `color: #666` |
| Légende de bus (`VIRTUAL BUS`) | `#dcd0ff`, gras | `_refresh_device_ui_state` |
| Case à cocher (`Default Sink`) | `#cccccc` | `QCheckBox { color: #ccc }` |
| Bordure de strip | `#555555` | `_update_base_style` |
| Suppression (repos / hover) | `#888888` / `#ff5555` | bouton « × » |
| Fader : gouttière / chargée / poignée | `#222222` / `#555555` / `#ffffff` | `QSlider::*:vertical` |
| VU : fond / bas / milieu / haut | `#222222` / `#33ff33` / `#ffff33` / `#ff3333` | `VUMeterWidget` |
| Fond de fenêtre | `#232323` | **thème Rust** (la fenêtre Qt n'a pas de fond : les sections la couvrent) |

Position des arrêts du dégradé VU (mesurées depuis le **haut** du widget) :
`0.0 → #ff3333`, **`0.2` → #ffff33**, `1.0 → #33ff33`
(`gradient.setColorAt(...)` dans `VUMeterWidget.__init__` ; le jaune est à 20 %,
la bande rouge est fine).

---

## 6. Typographie

| Rôle | Taille | Usage |
|---|---|---|
| `tiny` | **8 px** | `SOURCE IN`, libellés FX |
| `small` | **9 px** | `DEVICE OUT`, `BUS MASTER`, combo, MONO/MUTE, `No Outputs` |
| `midi` | **10 px** | bouton MIDI |
| `body` | **11 px** | nom de strip (gras) |
| `section_title` | **14 px** | `INPUTS` / `OUTPUTS` (gras) |
| `heading` | **15 px** | titres de fenêtres/dialogues, « + », glyphe « × » (14 px) |

---

## 7. Signaux Qt → opérations moteur

Chaque signal de `StripWidget` a un équivalent `UiAction` côté Rust
(`holaf_ui::app::UiAction`) et une commande moteur
(`holaf_ui::bridge::EngineCommand`, qui correspond 1:1 aux méthodes de
`holaf_core::backend::Backend`).

| Signal Qt | Slot `MainWindow` | `UiAction` | `EngineCommand` |
|---|---|---|---|
| `volume_changed(uid, v)` | `on_strip_volume_changed` | `Volume` | `SetVolume` |
| `mute_changed(uid, b)` | `on_strip_mute_changed` | `Mute` | `SetMute` |
| `mono_changed(uid, b)` | `on_strip_mono_changed` | `Mono` | `SetMono` |
| `effect_toggled(uid, key, b)` | `on_strip_effect_toggled` | `ToggleFx` | `SetEffect` |
| `effect_params_changed(uid, key)` | `on_strip_effect_params_changed` | *(dialogues FX, hors périmètre du squelette)* | `SetEffect` |
| `route_changed(src, dst, b)` | `on_strip_route_changed` | `Route` | `SetRoute` |
| `default_changed(uid, b)` | `on_strip_default_changed` | `DefaultSink` | `SetDefaultSink` (uniquement à `true`) |
| `device_changed(uid, dev)` | `on_strip_device_changed` | `DeviceChanged` | `SetDevice` |
| `delete_requested(uid)` | `on_strip_delete_requested` | `Delete` | `RemoveStrip` + `SyncSnapshot` |
| `label_changed(uid, lbl)` | `on_strip_label_changed` | `Rename` | `SyncSnapshot` |
| `midi_learn_requested(uid, p)` | `on_midi_learn_requested` | `MidiLearn` | *(aucun : le MIDI reste côté UI)* |
| `app_selection_requested(uid)` | `on_app_selection_requested` | `SelectApps` | *(aucun)* |
| — (bouton « + ») | `on_add_clicked` | `AddStrip` | `SyncSnapshot` |

---

## 8. Temporisations (main_window.py)

| Constante | Valeur | Rôle |
|---|---|---|
| `DEBOUNCE_ENGINE_RESTART_MS` | 200 | regroupe les redémarrages moteur |
| `METER_POLL_INTERVAL_MS` | 40 | 25 Hz pour les VU |
| `ENFORCE_ROUTING_INTERVAL_MS` | 5000 | ré-application du routage applicatif |
| `THREAD_POOL_MAX_WORKERS` | 4 | workers backend |
| `MIDI_FEEDBACK_SETTLE_MS` | 200 | stabilisation des LED MIDI |
| `RESTART_NOTICE_TIMEOUT_MS` | 8000 | durée du message de succès |

---

## 9. Ce qui n'est **pas** dans le squelette Rust

Ces éléments existent en Python et sont listés ici pour que l'écart soit
explicite :

* dialogue d'assignation d'applications (`AppSelectionDialog`) ;
* dialogue de paramètres d'effet (`EffectSettingsDialog`) et son menu
  contextuel ;
* menus contextuels (renommer / FX / menu applicatif « Restart audio engine »,
  « Debug window », « Open project page ») ;
* fenêtre de diagnostic (`src/diagnostics/`) ;
* icône de la zone de notification (`QSystemTrayIcon`) ;
* apprentissage MIDI réel (`midi_engine`) et retour LED ;
* géométrie de fenêtre persistée (`saveGeometry`/`restoreGeometry`) ;
* fader piloté par un contrôleur externe (le timer 20 Hz de `StripWidget`).

Le squelette Rust couvre le **layout complet de la fenêtre principale** et
l'intégralité des actions de strip.
