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
#   ./scripts/run-drm.sh                    # release build if present, else debug
#   ./scripts/run-drm.sh --exit-after 120   # stop by itself after two minutes
#   ./scripts/run-drm.sh --layout solar     # tiling, scrolling or solar
#   ./scripts/run-drm.sh --headless         # any argument is passed through
set -euo pipefail

# Which layout model to start in, overriding the config file's "layout".
#
# Passed straight through — the compositor has taken --layout since the solar
# layout landed — so the only thing done here is checking the name. That is
# worth doing at this end: the compositor warns and carries on with whatever it
# had, and a warning scrolling past on a TTY at the moment a session starts is
# not something anybody reads. What you see instead is the layout you did not
# ask for, and no reason given.
for arg in "$@"; do
    case $arg in
        --layout=*) layout=${arg#--layout=} ;;
        --layout) want_layout=1; continue ;;
        *) [ "${want_layout:-}" = 1 ] && layout=$arg; want_layout= ;;
    esac
done
case "${layout:-tiling}" in
    tiling | scrolling | solar) ;;
    *)
        echo "unknown layout '${layout}': expected tiling, scrolling or solar." >&2
        exit 1
        ;;
esac

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# Re-enter the dev shell once, then come back here with the flag set.
if [ -z "${VIEWPORT_DEV_SHELL:-}" ]; then
    echo "entering the dev shell for the library path..." >&2
    exec nix develop "$here#wpe" --command \
        env VIEWPORT_DEV_SHELL=1 "${BASH_SOURCE[0]}" "$@"
fi

# sudo drops LD_LIBRARY_PATH, which is the other way to arrive here without a
# working environment. libseat means root is not needed anyway.
if [ "$(id -u)" = 0 ]; then
    echo "run this as your own user: libseat and logind handle the permissions," >&2
    echo "and sudo strips the library path this needs." >&2
    exit 1
fi

# --features wpe is not the default: the shell needs WebKit, and everything
# else builds and tests without it.
binary="$here/target/release/viewport"
[ -x "$binary" ] || binary="$here/target/debug/viewport"
if [ ! -x "$binary" ]; then
    echo "no binary; build one first:" >&2
    echo "  nix develop .#wpe --command cargo build -p viewport --features wpe" >&2
    exit 1
fi

# The shell is behind a feature flag, and `cargo test` builds this same path
# without it — so a test run silently replaces a working binary with one that
# has no web engine in it. What that looks like is a session that comes up
# grey where the wallpaper and the bar should be, with nothing on screen to say
# why.
#
# This string exists only in a build that has the shell.
if ! grep -qa "starting the shell at" "$binary"; then
    echo "$binary has no shell in it: built without --features wpe," >&2
    echo "or overwritten by a cargo test run. Build it again:" >&2
    echo "  nix develop .#wpe --command cargo build --release -p viewport --features wpe" >&2
    exit 1
fi

# A seat with a VT. Without one libseat has nothing to take devices from, and
# the failure is a permission error that looks like a device problem.
if [ -z "${XDG_VTNR:-}" ]; then
    echo "warning: no XDG_VTNR, so this is probably not a TTY login." >&2
    echo "         DRM needs a seat; expect 'Operation not permitted'." >&2
fi

# Default to arguments that make the first run diagnosable rather than silent.
# Debug by default. Every bring-up question so far has been "what did the shell
# and the compositor actually say to each other", and an info-level log cannot
# answer it — three runs were spent re-running to get one. Override with
# VIEWPORT_LOG=info for a quiet run.
: "${VIEWPORT_LOG:=debug}"
export VIEWPORT_LOG

log="${VIEWPORT_LOG_FILE:-/tmp/viewport-drm.log}"
echo "logging to $log" >&2
echo "Ctrl+Alt+F1..F12 switches VT, Ctrl+Alt+Backspace quits." >&2
echo "If neither works, --exit-after 120 makes it stop on its own," >&2
echo "or run ./scripts/quit.sh from another TTY." >&2

# Passed through unchanged if the caller supplied their own backend flag.
case " $* " in
    *" --drm "* | *" --headless "*) set -- "$@" ;;
    *) set -- --drm "$@" ;;
esac

exec "$binary" "$@" 2>&1 | tee "$log"
