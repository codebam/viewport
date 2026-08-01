#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Run the vkcube benchmark on real hardware, from a TTY.
#
#   ./scripts/bench-drm.sh                          # everything decided for you
#   ./scripts/bench-drm.sh --codebam                # this machine's fast timing
#   ./scripts/bench-drm.sh --mode 2560x1440@239.760 # the same thing, spelled out
#   ./scripts/bench-drm.sh --second DP-3            # both monitors at once
#   ./scripts/bench-drm.sh --only all               # viewport, sway and niri
#   ./scripts/bench-drm.sh --runs 5                 # anything else is passed through
#
# --second turns on the two-monitor scenarios: the output picked below is held
# at full rate and the named one is measured, which is the frame rate a window
# on your other screen actually gets while something is busy over here. Do not
# combine it with --codebam or --mode — pinning both panels to one timing is
# the opposite of what those scenarios are for.
#
# bench-vkcube.sh does the measuring. This exists because getting it onto real
# hardware takes three things that are easy to get wrong and produce failures
# that name something else:
#
#   the dev shell   A cargo-built compositor dlopens libvulkan, libgbm and
#                   libEGL rather than linking them, so it needs the dev
#                   shell's library path at *run* time. Without it the
#                   compositor prints its control socket, then dies at GPU
#                   init — and what surfaced was a missing socket file, which
#                   names the symptom and not the cause.
#
#   vkcube          Not a dependency of the compositor, so it is not in the
#                   dev shell. Found on PATH, or built from nixpkgs.
#
#   which output    Both compositors pick for themselves otherwise, and the
#                   first run on this machine had sway on DP-3 and Viewport on
#                   DP-1 without either of them saying so.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# Re-enter the dev shell once, then come back here with the flag set. The same
# dance run-drm.sh does, and for the same reason: relying on anyone remembering
# is what produced the failure this script exists to prevent.
if [ -z "${VIEWPORT_DEV_SHELL:-}" ]; then
    echo "entering the dev shell for the library path..." >&2
    exec nix develop "$here" --command \
        env VIEWPORT_DEV_SHELL=1 "${BASH_SOURCE[0]}" "$@"
fi

# sudo strips the library path, and libseat means root is not needed anyway.
if [ "$(id -u)" = 0 ]; then
    echo "run this as your own user: libseat and logind handle the permissions," >&2
    echo "and sudo strips the library path this needs." >&2
    exit 1
fi

# A seat with a VT. Without one there is no DRM master to take, and the failure
# reads as a permission problem rather than as "you are not on a TTY".
if [ -z "${XDG_VTNR:-}" ]; then
    echo "no XDG_VTNR: this has to run from a TTY, not from inside a session." >&2
    echo "Switch with ctrl-alt-F3, log in, and run it there." >&2
    exit 1
fi

# vkcube. On PATH if someone already arranged it, otherwise built and cached by
# the store — which is a download the first time and instant afterwards.
vkcube=${VKCUBE:-$(command -v vkcube || true)}
if [ -z "$vkcube" ] || [ ! -x "$vkcube" ]; then
    echo "building vkcube from nixpkgs..." >&2
    tools=$(nix build --no-link --print-out-paths nixpkgs#vulkan-tools)
    vkcube=$tools/bin/vkcube
fi
[ -x "$vkcube" ] || { echo "no vkcube at $vkcube" >&2; exit 1; }

# Which monitor. The first connected one, unless told otherwise — and said out
# loud either way, because "which output" is half of whether two runs are
# comparable.
output=""
for status in /sys/class/drm/card*-*/status; do
    [ -r "$status" ] || continue
    [ "$(cat "$status")" = connected ] || continue
    connector=$(basename "$(dirname "$status")")
    # cardN-DP-1 -> DP-1
    output=${connector#*-}
    break
done

# Anything the caller passes wins, including --output and --mode.
#
# --codebam is a stored --mode. The refresh rate is not in sysfs, so the fast
# timing has to be typed out in full — and typing `2560x1440@239.760` into a
# TTY that cannot be pasted into is exactly the sort of thing that gets a
# benchmark run at the wrong rate. It is filtered out here rather than passed
# on, because bench-vkcube.sh rejects options it does not know.
#
# --frame-log turns on the compositor's frame pacing counters. Exported here,
# after the dev shell has been re-entered, so it reaches the compositor the
# harness starts. Filtered out for the same reason --codebam is: bench-vkcube.sh
# rejects options it does not know.
#
# Not for a run whose numbers matter — it is a log line a second, and the
# counting is on the render path.
codebam_mode="2560x1440@239.760"
args=()
for arg in "$@"; do
    case "$arg" in
        --codebam) args+=(--mode "$codebam_mode") ;;
        --frame-log)
            export VIEWPORT_FRAME_LOG=1
            echo "frame pacing counters on; expect a line a second in viewport.log" >&2
            ;;
        # No --no-shell flag, and the reason is worth writing down: pointing
        # VIEWPORT_SHELL_URL at a file that does not exist does not turn the
        # shell off. WebKit loads, fails, and renders its own error page — a
        # full-screen white document, composited over everything, which hides
        # the window and leaves every bit of the shell's cost still in the
        # measurement. To take the shell out of a run, build without the wpe
        # feature and point --viewport at that binary.
        --no-commit-timing)
            # Leaves wp_fifo_v1 up and takes wp_commit_timing_v1 down, which
            # is the half of the pacing --no-fifo cannot separate.
            export VIEWPORT_COMMIT_TIMING=0
            echo "commit-timing off, fifo still on" >&2
            ;;
        --no-fifo)
            # Stops the compositor advertising wp_fifo_v1 and
            # wp_commit_timing_v1, which sends Mesa back to pacing FIFO present
            # mode on frame callbacks. One run with and one without separates
            # "the pacing protocols" from everything else — which is what the
            # switch was added for.
            export VIEWPORT_FIFO=0
            echo "wp_fifo_v1 and commit-timing off; clients fall back to frame callbacks" >&2
            ;;
        *) args+=("$arg") ;;
    esac
done
have() {
    local flag=$1 arg
    [ ${#args[@]} -eq 0 ] && return 1
    for arg in "${args[@]}"; do
        [ "$arg" = "$flag" ] && return 0
    done
    return 1
}

if [ -n "$output" ] && ! have --output; then
    args+=(--output "$output")
fi
if ! have --out; then
    args+=(--out "$here/bench-results/drm-$(date +%Y%m%d-%H%M%S)")
fi
if ! have --vkcube; then
    args+=(--vkcube "$vkcube")
fi

# No --mode by default, and that is deliberate rather than an omission.
#
# Left alone, both compositors take the timing the monitor says it prefers, so
# both run at the same rate — which is the property the comparison needs. It
# is not necessarily the fastest the panel can do: a 240Hz monitor that
# nominates a 120Hz timing will be benchmarked at 120 by both. Pinning the
# fast one is one flag, and the refresh rate is not in sysfs to be guessed at
# from here.
# Pinning one timing onto every output is what makes the single-output
# comparison fair, and it is what makes the two-monitor one meaningless: the
# mismatch between two panels is half of what those scenarios exist to expose.
if have --second && have --mode; then
    echo "--second with --mode pins both panels to one timing, which is the" >&2
    echo "opposite of what the two-monitor scenarios measure. Drop one." >&2
    exit 2
fi

if ! have --mode && ! have --second; then
    echo >&2
    echo "no --mode: both compositors will take the monitor's preferred timing." >&2
    echo "That is equal, which is what matters, but it may not be the fastest" >&2
    echo "the panel can do. To pin it, re-run with e.g.:" >&2
    echo "  $0 --mode 2560x1440@239.760" >&2
    echo >&2
fi

echo "vkcube:  $vkcube" >&2
echo "output:  ${output:-whichever each compositor picks}" >&2
echo >&2

exec "$here/scripts/bench-vkcube.sh" --drm "${args[@]}"
