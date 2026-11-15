# Audit et mise à jour des dépendances — 2026-11-15

> Périmètre : `rust/**` (workspace cargo), `requirements.txt` (couche Python
> résiduelle), les dépendances **système** (BUILD.md, PKGBUILD, CI, `build.sh`)
> et le nettoyage induit dans le packaging (spec PyInstaller, workflow CI).
>
> Toutes les valeurs « après » de ce document sont **mesurées** sur ce dépôt
> (commandes données au §6), pas recopiées d'un changelog.

---

## 1. Résumé

| # | Sujet | Avant | Après | État |
|---|-------|-------|-------|------|
| 1 | **Copier le rapport de diagnostic** | feature `clipboard` **absente** : le bouton « Copy report » remplissait le presse-papier *interne* d'egui et annonçait « copied to the clipboard » | feature `clipboard` **active par défaut** (`arboard` + `smithay-clipboard`), et le message dépend de ce que le build sait réellement faire | **corrigé**, testé |
| 2 | Glyphes hors police embarquée (emoji, cyrillique, CJK) | feature `system_fonts` absente : tout caractère absent de la police par défaut s'affichait en « tofu » | feature `system_fonts` **active par défaut** (`egui_system_font_provider` + `fontique`) | **ajouté**, compile et tourne |
| 3 | **Icône de tray** | `tray-icon 0.19.3` + backend `libappindicator` ⇒ `gtk 0.18` + `glib 0.18` + `pango`, `libayatana-appindicator` au runtime, `GMainLoop` sur un thread dédié | `tray-icon 0.26.1` + feature **`ksni`** ⇒ `org.kde.StatusNotifierItem` en D-Bus pur, **zéro GTK** | **migré**, testé de bout en bout sur un bus D-Bus |
| 4 | `requirements.txt` | `numpy>=1.24.0,<2.0.0` (aucune roue au-delà de CPython 3.12), `pyinstaller` absent, en-tête « Pinned to known-compatible versions » faux | `numpy>=2,<3`, `pyinstaller` ajouté, en-tête réécrit (plages de compatibilité, pas de pin) | **corrigé**, `pip download` OK |
| 5 | `tap-plugins` (étage TUBE WARMTH) | absent de BUILD.md et du PKGBUILD alors que `spa.rs` / `export_fixtures.py` nomment `tap_tubewarmth` | documenté + déclaré (Debian/Fedora en dépendance, **AUR sur Arch** ⇒ optdepend + doc) | **corrigé** |
| 6 | Commentaire MSRV | « Arch ships 1.99.x as of 2026-11 » (périmé) | commentaire à jour (stable **1.100.0**, 2026-11-12) ; **pin `1.98` inchangé** (plancher volontaire) | **corrigé** |
| 7 | `pipewire 0.10.1`, `egui/eframe 0.37`, `image 0.25.10`, `midir 0.11.1`, `signal-hook 0.4.6`, `env_logger 0.11.11` | — | — | **déjà à jour**, non touchés |

Tests : **582 → 587** (`cargo test --workspace`), **356 → 362**
(`cargo test -p holaf-ui --features tray`), 207 tests Python inchangés et verts.

---

## 2. Tableau avant / après

### 2.1 Rust — dépendances directes de `holaf-ui`

| Crate | Avant | Après | Commentaire |
|---|---|---|---|
| `eframe` | 0.37 `default-features = false` + `glow`, `wayland`, `default_fonts` | idem **+ `clipboard` + `system_fonts`** (via les features homonymes de la crate) | §3.1, §3.2 |
| `egui` | 0.37 | 0.37 | inchangé |
| `tray-icon` | **0.19.3**, `default-features = false` | **0.26.1**, `default-features = false, features = ["ksni"]` | §3.3 |
| `gtk` | **0.18.2** (obligatoire avec `tray`) | **optionnel**, uniquement pour `--features tray-gtk` | §3.3 |
| `zbus` | — | **5.19.0** (optionnel, feature `tray`) | sonde d'hôte StatusNotifier |
| `muda` (via tray-icon) | 0.15.3 | 0.21.2 (`+ feature snapshot`) | publié en `com.canonical.dbusmenu` |
| `arboard` | absent du graphe | **3.7.0** (via `eframe/clipboard`) | presse-papier X11/Wayland |
| `smithay-clipboard` | absent | **0.7.3** | protocole `wlr-data-control` (Wayland) |
| `egui_system_font_provider` / `fontique` | absents | **0.37.0 / 0.11.1** | polices du système |
| `pipewire`, `image`, `midir`, `signal-hook`, `env_logger` | 0.10.1 / 0.25.10 / 0.11.1 / 0.4.6 / 0.11.11 | identiques | déjà les dernières versions |

`rust/Cargo.lock` : **397 → 484** paquets. L'augmentation vient de trois
familles ajoutées (presse-papier `arboard`/`smithay-clipboard`, polices
`fontique`/`harfrust`/`skrifa`, D-Bus `zbus`/`ksni`/`async-io`) ; **aucune
récrimination** : `gtk 0.18`, `glib 0.18`, `libappindicator`, `proc-macro-error`
restent *dans le lock* uniquement parce que la feature de repli `tray-gtk` les
déclare — ils ne sont **pas compilés** par le build livré (voir §5.1).

### 2.2 Rust — chaîne de compilation du tray (mesurée)

| | `--features tray` (défaut, ksni) | `--features tray-gtk` (repli) |
|---|---|---|
| bibliothèques partagées (`ldd`) | **7** (dont 5 de base : libc, libm, libgcc, ld, vdso — plus `libpipewire`, `libasound`) | **73** |
| GTK3 / GDK / GLib / Pango / ATK / appindicator | **aucune** | toute la pile + 11 `libX*` |
| `cargo tree \| grep -i gtk` | **vide** | `gtk 0.18.2`, `libappindicator 0.9.0`, `proc-macro-error 1.0.4` |
| binaire `release` strippé | 16,63 Mo | 13,02 Mo |
| paquets système à installer | **aucun** | `gtk3` + `libayatana-appindicator` |

> Le binaire ksni est **plus gros de 3,6 Mo** (zbus/async-io liés
> statiquement) mais il économise **66 bibliothèques partagées** et deux
> paquets système. C'est le compromis assumé : un `holaf-ui` autonome plutôt
> qu'une dépendance à une pile GTK3 fermée.

### 2.3 Python (`requirements.txt`)

| Paquet | Avant | Après | Pourquoi |
|---|---|---|---|
| `numpy` | `>=1.24.0,<2.0.0` | **`>=2.0.0,<3.0.0`** | le dernier 1.x est 1.26.4, roues ≤ CPython 3.12 : sur Arch (3.13/3.14) `pip install` échouait. Le code n'utilise que des API stables (`sqrt`, `mean`, `arange`, `frombuffer`, `stack`, `clip`) — vérifié : **les 207 tests Python passent avec numpy 2.4.6** |
| `pyinstaller` | **absent** | **`>=6.0.0,<7.0.0`** | `build.sh`, le PKGBUILD et le spec l'invoquent ; il manquait alors que l'en-tête prétendait décrire l'installation |
| `PySide6`, `sounddevice`, `mido`, `python-rtmidi` | bornes `>=x,<major+1` | inchangées | toujours correctes |
| en-tête | « Pinned to known-compatible versions » | décrit ce que le fichier **est** (plages de compatibilité, pas de pin ; sortie reproductible = PyInstaller) | le commentaire mentait |

### 2.4 Système

| Paquet | Avant | Après |
|---|---|---|
| `gtk3` | `depends` (lié par `holaf-ui` avec `tray`) | **retiré** (optdepend seulement pour le **fallback Qt**, dont `libqgtk3.so` charge la GTK3 système) |
| `libayatana-appindicator` | `depends` (dlopené) | **retiré** (plus rien ne le charge) |
| `tap-plugins` | absent partout | **Debian/Fedora : dépendance** ; **Arch : optdepend** (AUR-only) |
| `gnome-shell-extension-appindicator` | optdepend | optdepend, commentaire réécrit (hôte *StatusNotifier*, plus « hôte appindicator ») |
| garde-fou CI « bundle sans GTK3 » | justifié par `holaf-ui` **et** PySide6 | **conservé**, justifié par **PySide6 seul** (mesuré : `ldd PySide6/Qt/plugins/platformthemes/libqgtk3.so` ⇒ `libgtk-3.so.0` ; `libQt6Core` ⇒ `libglib-2.0`) |

---

## 3. Décisions et justifications

### 3.1 Le bug du presse-papier (le vrai motif de cet audit)

`eframe` était déclaré `default-features = false` avec seulement `glow`,
`wayland`, `default_fonts`. Or `clipboard` **n'est pas** dans ce que l'on garde
implicitement : sans elle, `egui-winit` ne construit **aucun** backend système et
retombe sur son presse-papier interne (« a fallback clipboard that just works
within the same app »). `ui.ctx().copy_text(text)` **réussissait** donc sans
jamais atteindre le bureau, pendant que le dialogue affichait
« diagnostic report copied to the clipboard ».

Correction en deux temps, parce qu'activer la feature ne suffit pas à rendre le
message honnête :

1. `clipboard` devient une **feature de la crate `holaf-ui`** qui transmet à
   `eframe/clipboard` (donc `arboard` + `smithay-clipboard`), **active par
   défaut** ;
2. tout appel passe par `crate::clipboard::copy(ctx, text)` qui renvoie un
   [`CopyOutcome`] : `System` (un backend système est compilé) ou `InApp` (seul
   le presse-papier interne d'egui a reçu le texte). Le message de la barre de
   statut est dérivé de ce verdict (`CopyOutcome::status`) : « copied to the
   clipboard » n'est affiché que dans le premier cas, sinon
   « kept in this window: this build has no system clipboard (rebuild with the
   `clipboard` feature…) ».

**Limite assumée** : ce qui est décidable *à la compilation* est la présence d'un
backend ; ce que le bureau a fait du texte ne l'est pas (`egui` expose la
commande sortante `OutputCommand::CopyText`, pas le verdict du backend). Sur un
bureau réel — X11, ou Wayland avec `wlr-data-control` — le chemin est celui de
toutes les applications natives. Documenté dans l'en-tête de
`rust/crates/holaf-ui/src/clipboard.rs`.

Tests ajoutés (4) : le contenu exact part bien dans la commande egui (lu depuis
`ctx.output()`), texte vide, formulation des deux messages, et une assertion qui
échoue si quelqu'un retire `clipboard` des features par défaut.

### 3.2 `system_fonts`

Ajouté pour la même raison structurelle (feature non impliquée par défaut) : les
noms de devices viennent de PipeWire et contiennent régulièrement des caractères
hors de la police embarquée. Coût : `egui_system_font_provider 0.37` +
`fontique` + `harfrust` + `skrifa` (déjà présents pour `fontique` côté
`epaint`/`vello_cpu`) et un scan de polices au démarrage. Le build
`--no-default-features` reste possible et n'embarque que la police par défaut.
Aucun impact sur le snapshot headless (il construit son propre `egui::Context`,
sans `eframe`) — vérifié : les PNG régénérés sont **bit-identiques** d'un run à
l'autre.

### 3.3 `ksni` plutôt que `libappindicator` (le gros morceau)

**Pourquoi** : `tray-icon 0.19` imposait `gtk 0.18`, une lignée **fermée** (la
0.19 n'a jamais été publiée pour gtk-rs, il faut passer à gtk4 + libadwaita, ce
qui n'est pas un changement de version mais un changement de framework) et
`libappindicator-sys`, qui `dlopen`e `libayatana-appindicator3.so.1` au runtime,
plus une **`GMainLoop`** sur un thread dédié puisqu'eframe possède le thread
principal. `tray-icon 0.26` + feature `ksni` remplace tout cela par un
`org.kde.StatusNotifierItem` en D-Bus, le mécanisme **natif** du bureau de
l'utilisateur (KDE/Plasma sous Wayland).

**Ce que la migration a supprimé** (mesuré, voir §2.2) : la pile GTK3/GLib du
graphe de dépendances du binaire livré, `libayatana-appindicator` des paquets et
de la doc, la boucle GTK du thread tray (`gtk::init()`/`gtk::main()`), et avec
`gtk3-macros` l'avis RUSTSEC-2024-0370 (`proc-macro-error`, non maintenu).

**Ce qu'elle a coûté** : `zbus 5` (déjà tiré par `ksni` — donc zéro compilation
supplémentaire) comme dépendance directe pour la sonde d'hôte, et +3,6 Mo sur le
binaire release (strippé : 13,02 → 16,63 Mo).

**Le piège corrigé au passage** : le backend ksni de `tray-icon` appelle
`assume_sni_available(true)`, c'est-à-dire qu'il **réussit même sans hôte**.
Sans garde-fou, `available()` valait `true` sur un bureau sans
StatusNotifierWatcher et « fermer = cacher » aurait laissé une fenêtre cachée
sans icône pour la rappeler — exactement le bug que le
`QSystemTrayIcon::isSystemTrayAvailable()` de Qt évitait. `tray::install()`
interroge donc D-Bus (`NameHasOwner` sur `org.kde.StatusNotifierWatcher`) avant
de revendiquer un tray :

| Sonde | Verdict | Conséquence |
|---|---|---|
| le nom a un propriétaire | `HostProbe::Present` | l'icône est construite, `available()` = succès du build, fermer **cache** |
| le nom n'a pas de propriétaire, **ou** pas de bus de session | `HostProbe::Absent` | warning explicite, `available()` = `false`, fermer **quitte** |
| le bus existe mais ne répond pas | `HostProbe::Unknown` | on construit quand même et on laisse `tray-icon` décider |

La politique pure (`close_action`, `menu_action`, `icon_rgba`) est **inchangée** et
toujours testée dans **toutes** les configurations de features.

**Le repli `tray-gtk`** est conservé : `--features tray-gtk` reconstruit l'ancien
montage (`tray-icon/libappindicator` + `gtk 0.18` + `GMainLoop`), et
`HOLAF_TRAY_GTK=1` le sélectionne depuis `scripts/build_rust.sh`. Il n'est
**jamais** activé par défaut et n'est pas impliqué par `tray` (si les deux
features sont demandées, ksni gagne — c'est aussi le choix interne de
`tray-icon`).

**Preuve de bout en bout, sans bureau** — `rust/crates/holaf-ui/tests/tray_live.rs` :
le test démarre un `dbus-daemon --session` **privé**, vérifie d'abord le cas sûr
(bus sans hôte ⇒ `available() == false`, fermer quitte), puis sert un **faux
`org.kde.StatusNotifierWatcher`** et vérifie que l'item **s'enregistre** et
publie le menu attendu, lu par le protocole `com.canonical.dbusmenu`
(`GetLayout`) : `Show/Hide`, `Restart audio engine`, séparateur, `Quit`.

### 3.4 `tap-plugins` : la consigne était fausse sur Arch

L'audit demandait de l'ajouter aux `depends` du PKGBUILD. Vérification faite sur
`archlinux.org` (recherche `tap-plugins` : **0 résultat**) et sur l'AUR
(`tap-plugins 1.0.1-3`) : le paquet **n'existe pas dans les dépôts Arch**.
Un `depends` sur un paquet inexistant rend le PKGBUILD insoluble pour tout le
monde. Décision :

* **Debian/Ubuntu** (`tap-plugins`) et **Fedora** (`ladspa-tap-plugins`) :
  ajoutés aux listes d'installation de `build.sh` ;
* **Arch** : `optdepends` + commande AUR documentée dans BUILD.md §1.3 et
  README ;
* `build.sh` avertit au démarrage si `/usr/lib/ladspa/tap_tubewarmth.so` manque
  (l'UI n'offre que les FX dont elle trouve le fichier — aucun autre étage n'est
  affecté).

Vérification exhaustive des plugins LADSPA cités par le code
(`rust/crates/holaf-core/src/fx.rs`, `spa.rs`, `tools/export_fixtures.py`) :

| Fichier | Paquet | État |
|---|---|---|
| `gate_1410.so`, `mbeq_1197.so`, `sc4_1882.so` | `swh-plugins` | déjà déclaré (depends) |
| `tap_tubewarmth.so` | `tap-plugins` | **ajouté** (cf. ci-dessus) |
| `librnnoise_ladspa.so` | aucun paquet (à compiler) | optdepend `rnnoise` + doc — inchangé |

### 3.5 MSRV

Le **pin reste `1.98`** : c'est un plancher volontairement bas. Le commentaire
(« Arch ships 1.99.x as of 2026-11 ») était périmé — la stable est **1.100.0**
(2026-11-12) — et ne documentait pas la vraie contrainte, désormais écrite :
le plancher doit couvrir le plus exigeant des dépendances (`eframe` /
`egui_system_font_provider` 0.37 ⇒ 1.95 ; `tray-icon`/`muda` ⇒ 1.90) tout en
restant sous la version d'Arch.

---

## 4. Ce qui a été nettoyé en cascade (après validation de ksni)

| Fichier | Changement |
|---|---|
| `packaging/PKGBUILD` | `gtk3` et `libayatana-appindicator` retirés des `depends` ; `tap-plugins` en optdepend ; `gtk3` redevient un optdepend **pour le fallback Qt** ; en-tête « `ldd` » réécrit (plus de pile GTK) |
| `packaging/holaf-mix.spec` | `_GTK_SYSTEM_LIBS` **conservé** mais ré-justifié : ce n'est plus `holaf-ui` qui tire GTK, c'est `PySide6` (`libqgtk3.so` + `libglib` de `libQt6Core`) ; commentaires mis à jour, message de log précisé |
| `.github/workflows/build.yml` | `gtk3`/`libayatana-appindicator` retirés de la liste `pacman` ; l'étape « tray » devient `cargo check --features tray` **+ un garde-fou `cargo tree`** qui échoue si GTK/GLib revient ; le garde-fou « bundle sans GTK3 » est conservé (raison : PySide6) et re-documenté |
| `scripts/build_rust.sh` | messages et commentaires (plus de GTK) ; la feature devient paramétrable (`HOLAF_TRAY_GTK=1` ⇒ `tray-gtk`) ; repli sans tray conservé |
| `build.sh` | `gtk3`/`libgtk-3-dev`/`gtk3-devel` + `libayatana-*` retirés des trois listes ; `tap-plugins` (Debian/Fedora) et note AUR (Arch) ajoutés ; l'avertissement « gtk+-3.0 manquant » est remplacé par un avertissement `tap_tubewarmth.so` |
| `BUILD.md` | §1.3 (runtime) et §5.1 (build) réécrits : plus de GTK, `tap-plugins` documenté (AUR + Debian), dépannage « tray absent » décrit avec le **nouveau** message de log, section « libgtk-3.so.0 » réattribuée au fallback Qt |
| `rust/README.md` | tableau du tray réécrit (backend ksni, sonde d'hôte, preuve `tray_live`, repli `tray-gtk`) ; `ldd` « avec tray » corrigé (plus de GTK, plus de pile X11) ; `tap-plugins` ajouté au tableau runtime |
| `README.md` | commandes d'installation runtime mises à jour |
| `docs/ui-rust-design.md` | encadré **« MISE À JOUR 2026-11-15 — le backend est `ksni` »** en tête de §10quater (les sections historiques sont conservées), tableaux de dépendances et §10quater.2/.5 réécrits |
| `docs/migration-to-pure-rust.md` | statut B2 (tray) et §10quater mis à jour |
| `docs/ui-rust-snapshot*.png` | **régénérés** (le rendu avait changé : plus de barre de statut, `MUTED` de la strip Mic conforme au `config.json` courant, libellés de devices à jour) |

---

## 5. Avis de sécurité et état du graphe

| Avis | Avant | Après |
|---|---|---|
| **RUSTSEC-2024-0370** (`proc-macro-error` 1.0.4, non maintenu) | présent dans le build `tray` (tiré par `gtk3-macros`) | **résolu** : `proc-macro-error` n'est plus atteignable que via `--features tray-gtk` ; `cargo tree -p holaf-ui --features tray` ne le montre pas |
| **RUSTSEC-2024-0411 → 0420** (famille `gtk3-rs` : atk/gdk/gtk/glib 0.18) | signalés à l'époque de l'audit | **WITHDRAWN** dans `advisory-db` — et de toute façon hors du build livré depuis ksni |
| `gtk 0.18` / `glib 0.18` | obligatoires | **optionnels** (repli `tray-gtk`) ; jamais compilés par le build livré |

`cargo audit` n'est pas installé dans le conteneur (`no such command: audit`) :
le contrôle ci-dessus a été fait en croisant `Cargo.lock`, `cargo tree` et
l'état de `advisory-db` (RUSTSEC-2024-0411..0420 retirés le 2026-09-08).
Relancer `cargo audit` sur une machine qui l'a reste la vérification de
référence.

### 5.1 Le `Cargo.lock` contient encore `gtk 0.18` — est-ce un problème ?

Non : `Cargo.lock` enregistre **toutes** les dépendances déclarées, y compris
celles des features optionnelles. `gtk 0.18` n'est atteignable que par
`--features tray-gtk`, et le chemin livré ne le compile ni ne le lie.
Les deux commandes qui font foi :

```bash
cargo tree -p holaf-ui --features tray -e normal | grep -iE '(gtk|glib|gdk|pango|atk|appindicator) v'   # → vide
ldd rust/target/release/holaf-ui | grep -iE 'gtk|gdk|glib|pango|appindicator'                            # → vide
```

La deuxième est vérifiée automatiquement en CI (nouvelle étape), pour qu'une
réintroduction silencieuse casse le job.

---

## 6. Vérifications effectuées

Toutes exécutées dans ce conteneur (Debian 12, cargo 1.99.1, pas de serveur
graphique, pas de bus de session) :

| Commande | Résultat |
|---|---|
| `cargo build -p holaf-ui` | OK, **0 warning** |
| `cargo build -p holaf-ui --no-default-features` | OK (ni `holaf-pw`, ni `midir`, ni clipboard/polices système) |
| `cargo build -p holaf-ui --features tray` | OK, **0 warning** |
| `cargo build -p holaf-ui --features x11` | OK |
| `cargo build -p holaf-ui --features tray-gtk` | OK (repli libappindicator, avec `gtk3` installé ici) |
| `cargo test --workspace` | **587 passés**, 0 échec (avant : 582) |
| `cargo test -p holaf-ui --features tray` | **362 passés**, 0 échec (avant : 356) |
| `cargo clippy -p holaf-ui --all-targets --features tray` | **0 warning sur `holaf-ui`** (les 4 restants sont dans `holaf-pw`, non modifié) |
| `cargo clippy --workspace --all-targets` | mêmes warnings préexistants (`holaf-core`, `holaf-pw`, `holafd`), **aucun nouveau** |
| `python3 -m unittest discover -s tests` | **207 tests OK** (1 skip), y compris **avec numpy 2.4.6** imposé par `PYTHONPATH` |
| `pip download --no-deps -r requirements.txt` | les 6 paquets se résolvent (numpy 2.4.6, pyinstaller 6.22.3, PySide6 6.12.0, …) |
| `python3 -m PyInstaller --version` | 6.22.3 (présent, donc `build.sh` peut packager) |
| `cargo tree -p holaf-ui --features tray` | pas de `gtk`/`glib`/`gdk`/`pango`/`atk`/`libappindicator` |
| `ldd rust/target/release/holaf-ui` (tray ksni) | 7 entrées, **0 bibliothèque GTK/GLib** |
| `cargo test -p holaf-ui --features tray --test tray_live` | OK (bus D-Bus privé + faux watcher : enregistrement + menu publié) |
| régénération des captures (`holaf-ui-snapshot`) | 3 PNG régénérés, **déterministes** (deux runs ⇒ mêmes octets) |
| `python3 -c "yaml.safe_load(...)"` sur `.github/workflows/build.yml` | YAML valide, 1 job |
| `bash -n` sur `build.sh`, `scripts/*.sh`, `assets/icons/build_icons.sh`, `packaging/PKGBUILD` | OK |
| `python3 -c "ast.parse(...)"` sur `packaging/holaf-mix.spec` | OK |

---

## 7. Ce qui n'a pas pu être vérifié ici (et comment le vérifier chez soi)

| Sujet | Pourquoi | À vérifier sur une machine avec session graphique |
|---|---|---|
| **Copie réelle dans le presse-papier** | pas de serveur X/Wayland : `arboard` ne peut pas s'initialiser, donc aucun backend système à interroger | bouton **Copy report** → coller dans un éditeur. Le log doit dire `tray`-indifférent mais **pas** « no system clipboard » ; l'absence de la ligne `clipboard: no system clipboard in this build` prouve que la feature est active |
| **Rendu des glyphes système** (`system_fonts`) | dépend des polices installées | un nom de device avec emoji/CJK/cyrillique ne doit plus s'afficher en carrés |
| **Icône de tray réellement dessinée** | le conteneur n'a ni bureau ni hôte (le test prouve l'enregistrement D-Bus et le menu, pas le **rendu** dans un panneau) | lancer sous KDE/Plasma : l'icône doit apparaître dans la zone de notification, et le menu doit contenir Show/Hide / Restart audio engine / Quit |
| **Clics sur l'icône** | le backend ksni ne transmet pas les clics à l'application (comme `libappindicator` avant lui) | c'est le **menu** (clic gauche) qui pilote tout ; rien à faire côté code |
| **Bouton « Copy report » sur Wayland sans `wlr-data-control`** (GNOME/mutter) | limitation d'egui/`smithay-clipboard`, pas de ce dépôt | si la copie ne va pas au presse-papier, le message doit au moins ne pas mentir (feature active = message « copied » ; le cas « backend refusé par le compositeur » n'est pas détectable — voir la limite §3.1) |
| **MIDI réel** (AKAI MIDI Mix) | pas de `/dev/snd/seq` dans le conteneur (les tests MIDI « live » utilisent un contrôleur virtuel et s'auto-skippent) | déjà couvert par `cargo test -p holaf-ui --features midi --test midi_live` sur une machine avec ALSA |
| `cargo audit` | sous-commande absente du conteneur | `cargo install cargo-audit && cargo audit` |

---

## 8. Fichiers modifiés

Code Rust :

* `rust/crates/holaf-ui/Cargo.toml` — features `clipboard` / `system_fonts` /
  `tray-gtk`, `tray-icon 0.26` + `ksni`, `zbus`, dev-dep `zbus`
* `rust/crates/holaf-ui/src/clipboard.rs` — **nouveau** (copie + verdict honnête)
* `rust/crates/holaf-ui/src/lib.rs` — module `clipboard`
* `rust/crates/holaf-ui/src/dialogs.rs` — « Copy report » passe par
  `clipboard::copy` et n'annonce que ce qui a eu lieu
* `rust/crates/holaf-ui/src/tray.rs` — backend `ksni`, sonde d'hôte
  StatusNotifier, suppression de `gtk::init`/`gtk::main` du chemin par défaut,
  tests mis à jour (+ `HostProbe`)
* `rust/crates/holaf-ui/src/main.rs` — commentaires (plus de GTK)
* `rust/crates/holaf-ui/tests/tray_live.rs` — **nouveau** (preuve D-Bus)
* `rust/Cargo.toml` — commentaire MSRV (pin inchangé)
* `rust/Cargo.lock`

Python / packaging / CI / doc :

* `requirements.txt`
* `build.sh`, `scripts/build_rust.sh`
* `packaging/PKGBUILD`, `packaging/holaf-mix.spec`
* `.github/workflows/build.yml`
* `BUILD.md`, `README.md`, `rust/README.md`
* `docs/ui-rust-design.md`, `docs/migration-to-pure-rust.md`
* `docs/dependency-audit.md` — **ce document**
* `docs/ui-rust-snapshot.png`, `docs/ui-rust-snapshot-tall.png`,
  `docs/ui-rust-dialog.png` — régénérés

---

## 9. Reste à faire (proposé, non fait ici)

1. **`cargo audit` en CI** : une étape `cargo install cargo-audit && cargo audit`
   (ou `rustsec/audit-check`) figerait l'état RUSTSEC dans le temps. Non ajoutée
   ici faute de pouvoir la valider dans ce conteneur.
2. **Retirer complètement le repli `tray-gtk`** quand une version de
   `tray-icon` aura supprimé le backend `libappindicator` : cela sortirait
   `gtk 0.18` du `Cargo.lock` lui-même (pas seulement du build).
3. **`libqgtk3.so`** : le jour où la couche Python/Qt disparaît
   (`docs/migration-to-pure-rust.md`), l'exclusion GTK du spec PyInstaller et le
   garde-fou CI correspondant peuvent disparaître à leur tour — plus rien alors
   ne tirera GTK dans ce dépôt.
4. **Presse-papier** : quand `egui` exposera un verdict de backend (ou si l'on
   accepte une dépendance directe à `arboard` pour faire un aller-retour de
   lecture), le message pourra devenir exact même dans le cas « compositeur qui
   refuse `zwlr_data_control_manager_v1` ».
