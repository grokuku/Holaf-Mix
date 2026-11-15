#!/bin/sh
# ============================================================================
#  holaf-mix — thin launcher for the Rust UI (`holaf-ui`)
# ============================================================================
#  Holaf-Mix has no Python anymore: this script does nothing but hand the
#  command line to the `holaf-ui` binary that sits next to it.  It exists only
#  to keep the familiar `holaf-mix` command working (tarball, /usr/bin).
#
#  All the logic — config resolution, engine, MIDI, tray — lives in Rust:
#      holaf-ui --help
#
#  The script follows symlinks, so a `ln -s ~/apps/holaf-mix-x.y.z/holaf-mix
#  ~/.local/bin/holaf-mix` still finds the real `holaf-ui`.
# ============================================================================
set -eu

self=$0
# Follow symlinks (bounded: a symlink loop is a broken install, not a case to
# spin on).
links=0
while [ -L "$self" ] && [ "$links" -lt 32 ]; do
    target=$(readlink "$self")
    case $target in
        /*) self=$target ;;
        *) self=$(dirname -- "$self")/$target ;;
    esac
    links=$((links + 1))
done

dir=$(CDPATH= cd -- "$(dirname -- "$self")" && pwd -P)

if [ ! -x "$dir/holaf-ui" ]; then
    echo "holaf-mix: cannot find the executable '$dir/holaf-ui'" >&2
    echo "           (the tarball must be unpacked as a whole; do not move the launcher alone)" >&2
    exit 1
fi

exec "$dir/holaf-ui" "$@"
