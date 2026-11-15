#!/usr/bin/env bash
# ============================================================================
#  scripts/check-wayland-guard.sh — « holaf-ui est un client WAYLAND »
# ============================================================================
#  Ce que l'on protège (la raison d'être de ce garde-fou) :
#
#    Le bureau de l'utilisateur est Wayland exclusif.  Un EFRAME compilé avec
#    sa feature `x11` (ou un WINIT avec son backend `x11`) fait apparaître le
#    binaire avec l'icône « X11 », le fait passer par XWayland et lui fait
#    réclamer un serveur X.  C'est le bug qui a motivé ce garde-fou : il ne
#    doit jamais revenir silencieusement.
#
#  Ce que ce garde-fou NE peut PAS exiger : « zéro crate X11 dans tout le
#  graphe ».  Depuis que le presse-papier système est activé (feature
#  `clipboard` d'eframe), `arboard` est dans le graphe — et `arboard` déclare
#  `x11rb` en dépendance **non optionnelle** sur Linux (Cargo.toml amont :
#  `[target.'cfg(all(unix, not(...)))'.dependencies] x11rb = "0.13"`, sans
#  `optional = true` ni feature pour la couper).  Aucune combinaison de
#  features ne retire `x11rb` : même `default-features = false` +
#  `wayland-data-control` (déjà ce que fait `egui-winit`) le laisse entrer.
#
#  La distinction qui compte — et que ce script vérifie — est donc :
#
#    * `x11rb` / `x11rb-protocol` = implémentation **X11 en Rust pur**
#      (protocole par socket Unix), tirée UNIQUEMENT par `arboard`.  Sur un
#      bureau Wayland, `arboard` n'emprunte jamais ce chemin : il teste
#      `WAYLAND_DISPLAY` et utilise `wlr-data-control` (`wl-clipboard-rs`)
#      dès qu'il est présent.  Aucune bibliothèque C X11 n'est liée (`ldd` ne
#      montre ni `libX11` ni `libxcb`).
#
#    * `x11-dl` (et `as-raw-xcb-connection`, `xcb-*`) = le **vrai** client
#      X11 C, `dlopen`é par `winit`/`glutin`.  C'est LUI qui décide du backend
#      de fenêtre.  Il doit être absent du build livré — c'est le cœur de ce
#      garde-fou.
#
#  Usage :
#      scripts/check-wayland-guard.sh          # défaut (livré) + `tray`
#      HOLAF_GUARD_SKIP_X11_BUILD=1 scripts/check-wayland-guard.sh
#                                              # saute le `cargo check` X11
#
#  Sortie : code 0 = tout est conforme.  En CI, les messages d'erreur sont
#  préfixés `::error::` pour apparaître dans l'annotation GitHub.
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT/rust"

# Le presse-papier est la SEULE justification admise pour la présence de
# crates X11 dans le graphe livré (voir l'en-tête).  Toute autre provenance
# est un échec.
X11_ALLOWED_CRATES=("x11rb" "x11rb-protocol")
X11_ALLOWED_PARENTS=("arboard" "x11rb")

die() {
    echo "::error::$*"
    exit 1
}

# Les deux configurations mesurées : `--no-default-features` n'est pas testée
# ici (elle n'a ni presse-papier ni polices système : le graphe y est
# trivialement plus petit, et c'est `--features tray` qui est LIVRÉ).
CONFIGS=("" "--features tray")

# Normalise une sortie `cargo tree --prefix none` en noms de paquets uniques :
#   « ├── arboard v3.7.0 (*) »  →  « arboard »
pkg_names() {
    sed -E -e 's/^[^[:alnum:]_.]*//' -e 's/ v[0-9][^ ]*//' -e 's/ \(\*\)$//' \
        -e 's/[[:space:]]+$//' | grep -E '^[[:alnum:]_.-]+$' | sort -u
}

# Features ACTIVÉES d'un paquet, une par ligne.
#
# ATTENTION (piège mesuré) : il faut passer par `-i <paquet> --depth 1`.
# Sur l'arbre complet (`cargo tree -e features`), cargo déduplique les
# occurrences répétées d'un paquet et n'affiche la liste de ses features
# qu'UNE fois, sur une occurrence qui peut être marquée `(*)` plus bas :
# `grep 'eframe feature "x11"'` sur l'arbre complet renvoyait donc un FAUX
# NÉGATIF même avec `--features x11` (mesuré).  La vue inverse par paquet est
# exhaustive.
features_of() {
    local cfg="$1" pkg="$2"
    # shellcheck disable=SC2086
    cargo tree --locked -p holaf-ui $cfg -e features -i "$pkg" --depth 1 --prefix none 2>/dev/null \
        | grep -E "^${pkg} feature \"" || true
}

# ---------------------------------------------------------------------------
# 1. Le backend Wayland est bien là, et eframe/winit n'ont PAS la feature x11.
# ---------------------------------------------------------------------------
check_wayland_only() {
    local cfg="$1"
    local eframe winit arboard

    eframe="$(features_of "$cfg" eframe)"
    winit="$(features_of "$cfg" winit)"
    [ -n "$eframe" ] || die "cannot read eframe's features (${cfg:-default build})"
    [ -n "$winit" ] || die "cannot read winit's features (${cfg:-default build})"

    echo "$eframe" | grep -q 'feature "wayland"' \
        || die "the Wayland backend is gone from eframe (${cfg:-default build})"
    echo "$winit" | grep -q 'feature "wayland"' \
        || die "the Wayland backend is gone from winit (${cfg:-default build})"

    if echo "$eframe" | grep -q 'feature "x11"'; then
        die "holaf-ui builds eframe WITH its x11 feature (${cfg:-default build})"
    fi
    if echo "$winit" | grep -q 'feature "x11"'; then
        die "holaf-ui builds winit WITH its x11 backend (${cfg:-default build})"
    fi

    # Le presse-papier est la seule raison pour laquelle des crates X11 ont le
    # droit d'exister : sans `wayland-data-control`, `arboard` ne compilerait
    # pas son chemin Wayland et la copie ne quitterait pas la fenêtre (c'est
    # exactement le bug « Copy report » déjà corrigé).
    arboard="$(features_of "$cfg" arboard)"
    if [ -n "$arboard" ]; then
        echo "$arboard" | grep -q 'feature "wayland-data-control"' \
            || die "arboard is in the graph WITHOUT wayland-data-control: the clipboard would not reach a Wayland compositor (${cfg:-default build})"
    fi

    echo "  OK   wayland backend present, eframe/winit without x11 (${cfg:-default build})"
}

# ---------------------------------------------------------------------------
# 2. Aucun client X11 C (`x11-dl` & co) dans le graphe livré.
# ---------------------------------------------------------------------------
check_no_x11_c_libs() {
    local cfg="$1"
    local names

    # shellcheck disable=SC2086
    names="$(cargo tree --locked -p holaf-ui $cfg -e normal --prefix none --format '{p}' 2>/dev/null \
        | pkg_names)"

    local forbidden
    forbidden="$(echo "$names" | grep -E '^(x11-dl|xcb|xcb-[a-z-]+|libxcb|libx11|as-raw-xcb-connection|xkbcommon-x11)$' || true)"
    if [ -n "$forbidden" ]; then
        die "holaf-ui pulls an X11 *client* crate without --features x11 (${cfg:-default build}): $(echo "$forbidden" | tr '\n' ' ')"
    fi

    echo "  OK   no x11-dl / libxcb / xcb-* / as-raw-xcb-connection (${cfg:-default build})"
}

# ---------------------------------------------------------------------------
# 3. Les crates X11 présentes viennent TOUTES de `arboard` (presse-papier).
# ---------------------------------------------------------------------------
check_x11_origin() {
    local cfg="$1"
    local pkg parents parent ok

    for pkg in "${X11_ALLOWED_CRATES[@]}"; do
        # shellcheck disable=SC2086
        if ! cargo tree --locked -p holaf-ui $cfg -i "$pkg" --depth 1 --prefix none -e normal >/dev/null 2>&1; then
            continue # la crate n'est pas dans ce graphe : parfait
        fi

        # shellcheck disable=SC2086
        parents="$(cargo tree --locked -p holaf-ui $cfg -i "$pkg" --depth 1 --prefix none -e normal 2>/dev/null \
            | pkg_names | grep -vx "$pkg" || true)"

        ok=1
        while IFS= read -r parent; do
            [ -n "$parent" ] || continue
            local allowed=0
            for a in "${X11_ALLOWED_PARENTS[@]}"; do
                [ "$parent" = "$a" ] && allowed=1
            done
            if [ "$allowed" -ne 1 ]; then
                ok=0
                echo "::error::$pkg is pulled by '$parent', which is NOT the clipboard (${cfg:-default build})"
            fi
        done <<< "$parents"

        [ "$ok" -eq 1 ] || die "the X11 crate $pkg does not come from arboard alone (${cfg:-default build}) — a windowing/GL crate is pulling it"

        echo "  OK   $pkg only from arboard (clipboard) (${cfg:-default build})"
    done
}

# ---------------------------------------------------------------------------
# 4. La configuration `--features x11` (opt-in, jamais livrée) marche encore :
#    c'est le contrôle « positif » qui prouve que les tests ci-dessus ne sont
#    pas vides par accident.
# ---------------------------------------------------------------------------
check_optin_x11() {
    if [ "${HOLAF_GUARD_SKIP_X11_BUILD:-0}" = "1" ]; then
        echo "  SKIP --features x11 build (HOLAF_GUARD_SKIP_X11_BUILD=1)"
    else
        cargo check --locked -p holaf-ui --features x11
    fi

    features_of "--features x11" eframe | grep -q 'feature "x11"' \
        || die "--features x11 no longer enables eframe/x11"
    features_of "--features x11" winit | grep -q 'feature "x11"' \
        || die "--features x11 no longer enables winit/x11"
    cargo tree --locked -p holaf-ui --features x11 -e normal --prefix none --format '{p}' 2>/dev/null \
        | pkg_names | grep -qx 'x11-dl' \
        || die "--features x11 no longer enables winit's x11 backend (x11-dl)"

    echo "  OK   holaf-ui builds with --features x11 (opt-in, not shipped)"
}

echo "== guard: holaf-ui is a Wayland client (X11 only via the clipboard, never as a window backend) =="
for cfg in "${CONFIGS[@]}"; do
    check_wayland_only "$cfg"
    check_no_x11_c_libs "$cfg"
    check_x11_origin "$cfg"
done
check_optin_x11
echo "== guard PASSED =="
