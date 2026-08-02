#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The same benchmark, once per shell backend.
#
#   ./scripts/bench-backends.sh                  # wpe, webkitgtk and chromium
#   ./scripts/bench-backends.sh --only chromium  # one of them
#   ./scripts/bench-backends.sh --runs 5         # anything else goes through
#
# What this measures that `bench-drm.sh` on its own does not: the compositor is
# the same binary three times over and only the engine drawing the desktop
# changes, so the difference between the three runs is the engine and the
# architecture around it — in-process against out-of-process, WebKit against
# Blink — and nothing else.
#
# Each backend is a *package*, not a flag. The packaged wrapper is what names
# the engine (`VIEWPORT_SHELL_BACKEND`) and puts the right shell program in
# bin/ beside the compositor, so pointing `--viewport` at each in turn is the
# only honest way to run this: a `--shell-backend=chromium` against the wpe
# package would go looking for a program that is not installed.
#
# Runs have to be on a TTY, for the reason docs/benchmarks.md gives: a nested
# run is paced by the host compositor, so only its CPU and memory columns mean
# anything. `bench-drm.sh` refuses if there is no VT, which is the check that
# matters here too.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# The seat this is actually sitting at.
#
# `bench-drm.sh` refuses without `XDG_VTNR`, and libseat needs `XDG_SESSION_ID`
# to name a session that is *current* — a tmux started under an older session
# carries that session's id, and taking DRM master then fails with "Operation
# not permitted" on card1, which reads as a permissions problem and is not one.
# Both are derived here rather than assumed, so this works from a shell that
# was not started by the login on this VT.
if [ -z "${XDG_VTNR:-}" ] || [ -z "${XDG_SESSION_ID:-}" ]; then
    while read -r session _; do
        [ -n "$session" ] || continue
        seat=$(loginctl show-session "$session" -p Seat --value 2>/dev/null || true)
        vt=$(loginctl show-session "$session" -p VTNr --value 2>/dev/null || true)
        state=$(loginctl show-session "$session" -p State --value 2>/dev/null || true)
        if [ "$seat" = seat0 ] && [ -n "$vt" ] && [ "$vt" != 0 ] && [ "$state" = active ]; then
            export XDG_SESSION_ID=$session XDG_VTNR=$vt
            echo "using session $session on tty$vt" >&2
            break
        fi
    done < <(loginctl list-sessions --no-legend 2>/dev/null | awk -v u="$USER" '$3 == u {print $1}')
fi

backends=(wpe webkitgtk chromium)
passthrough=()
stamp=$(date +%Y%m%d-%H%M%S)
outroot="$here/bench-results/backends-$stamp"

while [ $# -gt 0 ]; do
    case "$1" in
        --only) backends=("$2"); shift 2 ;;
        --out) outroot=$2; shift 2 ;;
        *) passthrough+=("$1"); shift ;;
    esac
done

# Built before anything is measured, and all of them, so that a build failure
# in the third does not land after two runs have already had the machine to
# themselves. `.#wpe` is the one that can take hours — it is the backend whose
# engine is compiled from source — and finding that out first is the point.
declare -A binary
for backend in "${backends[@]}"; do
    echo "building .#$backend..." >&2
    path=$(nix build "$here#$backend" --no-link --print-out-paths)
    binary[$backend]=$path/bin/viewport
    [ -x "${binary[$backend]}" ] || {
        echo "no viewport binary in $path" >&2
        exit 1
    }
done

mkdir -p "$outroot"
for backend in "${backends[@]}"; do
    out=$outroot/$backend
    echo >&2
    echo "=== $backend ===" >&2
    echo "    ${binary[$backend]}" >&2
    # `--only viewport`: sway and niri do not have a shell backend, and running
    # them three times would measure the same thing three times.
    "$here/scripts/bench-drm.sh" \
        --only viewport \
        --viewport "${binary[$backend]}" \
        --out "$out" \
        ${passthrough[@]+"${passthrough[@]}"}
done

echo >&2
echo "results under $outroot" >&2
for backend in "${backends[@]}"; do
    summary=$outroot/$backend/summary.md
    [ -f "$summary" ] && {
        echo >&2
        echo "--- $backend ---" >&2
        cat "$summary" >&2
    }
done
