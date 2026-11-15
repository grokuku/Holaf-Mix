# 🗺️ Holaf-Mix — Roadmap

> **Légende des statuts :**
> - 🔴 **Non validé** — en attente de décision
> - 🟢 **Validé** — à implémenter
> - ⚪ **Rejeté** — ne sera pas fait
> - ✅ **Fait** — implémenté

> **Note (2026-11-15)** : le produit est désormais **100 % Rust**.  L'audit
> Python qui occupait cette page (bugs et améliorations de `main.py`,
> `audio_engine.py`, `strip_widget.py`, …) a disparu avec le code concerné ; il
> reste consultable dans l'historique git.  Les points ci-dessous concernent
> l'UI Rust actuelle (`rust/crates/holaf-ui`).

---

## 🐛 Bugs / fiabilité

| # | Description | Statut |
|---|-------------|--------|
| 1 | **Changement de paramètres FX = reconstruction de chaîne** : tourner un knob d'EQ force le rebuild complet (`AppState` replanifie la chaîne), avec une coupure de son. | 🔴 Non validé |
| 2 | **Pas de vu-mètre de gain reduction** : aucun retour visuel sur ce que font le gate/compresseur. | 🔴 Non validé |
| 3 | **Liste des devices jamais rafraîchie en live** : brancher/débrancher une interface demande un redémarrage de l'app. | 🔴 Non validé |
| 4 | **Suppression de strip sans undo** : la confirmation existe, pas l'annulation. | 🔴 Non validé |
| 5 | **Couverture live** : les tests audio réels (`*_live.rs`) ne tournent que sur une machine avec PipeWire headless (`scripts/start-pipewire-headless.sh`) ; ils ne sont pas exécutés en CI. | 🔴 Non validé |

---

## 👤 Améliorations UX

| # | Description | Statut |
|---|-------------|--------|
| 1 | **Pas de presets / configurations multiples** (Gaming, Streaming, Musique…). | 🔴 Non validé |
| 2 | **Pas de Solo / PFL (Pre-Fader Listen)**. | 🔴 Non validé |
| 3 | **Pas de raccourcis clavier** (mute, save, navigation de strips). | 🔴 Non validé |
| 4 | **Pas de peak hold sur les VU-mètres** : les barres ne retiennent pas le pic. | 🔴 Non validé |
| 5 | **Pas de valeur numérique sur le fader** (dB ou %), difficile de reproduire un réglage. | 🔴 Non validé |
| 6 | **MIDI Learn sans timeout ni annulation visible** : le bouton passe en « LEARNING » sans indiquer comment sortir. | 🔴 Non validé |
| 7 | **Labels de routing tronqués à 4 caractères** (`out_strip.label[:4]`) : « Casque » vs « Chat » devient illisible. | 🔴 Non validé |
| 8 | **Double-clic pour renommer pas découvrable** (pas de changement de curseur ni d'indice visuel). | 🔴 Non validé |
| 9 | **« SELECT APPS » n'affiche pas le nombre d'apps assignées**. | 🔴 Non validé |
| 10 | **Thème sombre uniquement** (`theme/`), pas de thème clair. | ⚪ Rejeté — choix esthétique assumé. |
| 11 | **Largeur de fenêtre verrouillée** (`layout::enforce_window_width`) : volontaire pour garder l'UI propre ; la hauteur reste libre. | ⚪ Rejeté — comportement voulu. |

---

## 🔬 R&D

| # | Description | Statut |
|---|-------------|--------|
| 1 | **Hot-reload des paramètres LADSPA sans rebuild** : injecter à chaud de nouveaux paramètres dans un nœud `filter-chain` (`pw-cli set-param` / API `pw_filter`).  Si ça marche : plus de reconstruction au toggle d'effet ni au changement de paramètre, et un vrai live tuning. | 🔴 Non validé |

---

## 📝 Notes

- Tant qu'un point n'est pas passé en 🟢 **Validé**, il ne doit **pas** être
  implémenté.
- Une fois un point implémenté, le passer en ✅ **Fait**.
- Si un point est jugé non pertinent, le passer en ⚪ **Rejeté** avec une courte
  justification.
