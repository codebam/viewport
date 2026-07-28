#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Run the compositor on real hardware, from a TTY.
#
# The binary dlopens libvulkan, libgbm, libEGL and libwayland rather than
# linking them, so it needs the dev shell's library path at *run* time and not
# only at build time. Getting that wrong produces "Failed to load the Vulkan
# library", which says nothing about the cause — so this script re-enters the
# dev shell itself rather than relying on anyone remembering.
#
#   ./scripts/run-drm.sh              # release build if present, else debug
#   ./scripts/run-drm.sh --headless   # any argument is passed through
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# Re-enter the dev shell once, then come back here with the flag set.
if [ -z "${VIEWPORT_DEV_SHELL:-}" ]; then
    echo "entering the dev shell for the library path..." >&2
    exec nix develop "$here" --command \
        env VIEWPORT_DEV_SHELL=1 "${BASH_SOURCE[0]}" "$@"
fi

# sudo drops LD_LIBRARY_PATH, which is the other way to arrive here without a
# working environment. libseat means root is not needed anyway.
if [ "$(id -u)" = 0 ]; then
    echo "run this as your own user: libseat and logind handle the permissions," >&2
    echo "and sudo strips the library path this needs." >&2
    exit 1
fi

binary="$here/target/release/viewport"
[ -x "$binary" ] || binary="$here/target/debug/viewport"
if [ ! -x "$binary" ]; then
    echo "no binary; build one first:" >&2
    echo "  nix develop --command cargo build -p viewport" >&2
    exit 1
fi

# A seat with a VT. Without one libseat has nothing to take devices from, and
# the failure is a permission error that looks like a device problem.
if [ -z "${XDG_VTNR:-}" ]; then
    echo "warning: no XDG_VTNR, so this is probably not a TTY login." >&2
    echo "         DRM needs a seat; expect 'Operation not permitted'." >&2
fi

# Default to arguments that make the first run diagnosable rather than silent.
: "${VIEWPORT_LOG:=info}"
export VIEWPORT_LOG

log="${VIEWPORT_LOG_FILE:-/tmp/viewport-drm.log}"
echo "logging to $log" >&2
echo "Ctrl+Alt+F1..F12 switches VT, Ctrl+Alt+Backspace quits." >&2

# Passed through unchanged if the caller supplied their own backend flag.
case " $* " in
    *" --drm "* | *" --headless "*) set -- "$@" ;;
    *) set -- --drm "$@" ;;
esac

exec "$binary" "$@" 2>&1 | tee "$log"
