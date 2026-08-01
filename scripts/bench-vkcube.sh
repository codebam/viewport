#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Measure what a compositor costs to put a Vulkan client on screen, and say it
# in numbers that another compositor can be held against.
#
#   scripts/bench-vkcube.sh                       # viewport and sway, nested
#   scripts/bench-vkcube.sh --only all            # and niri as well
#   scripts/bench-vkcube.sh --only viewport       # one of them
#
# niri is worth having in the comparison and not only for a third number:
# Viewport's scrolling layout was written from it, so it is the compositor
# that model should be held against. It is fetched from nixpkgs when it is not
# already on PATH, the same way vkcube is.
#   scripts/bench-vkcube.sh --runs 5              # more repeats, median wins
#   scripts/bench-vkcube.sh --scale 0.25          # a quarter of the frames, for a smoke test
#   scripts/bench-vkcube.sh --drm                 # from a TTY: real scanout
#
# On real hardware, pin what the two compositors would otherwise each choose
# for themselves — the mode, and which monitor:
#
#   scripts/bench-vkcube.sh --drm --output DP-1 --mode 2560x1440@239.760
#
# Two monitors, which is a different question and only answerable from a TTY:
#
#   scripts/bench-vkcube.sh --drm --output DP-1 --second DP-3
#
# --output becomes the screen held busy and --second the one measured. What
# comes back is the frame rate the second monitor achieved while the first was
# saturated, which is the thing no single-output run can report. Leave --mode
# off for this: the two panels are allowed to differ, and a compositor that
# paces off the device rather than off each screen is exactly what the
# mismatch exposes.
#
# Both are pinned on every output, not only the named one, because Viewport's
# shell picks which output a window opens on and nothing on the wire overrides
# it. --output is what sway focuses and what the report checks Viewport
# against; --mode is what makes the answer the same either way.
#
# Both compositors are configured so that a lone window fills its output — a
# tiling layout, no bar, no gaps — which is what makes the two client sizes
# comparable without asking either of them for anything. `client size` in
# environment.txt is the check that it worked. `--fullscreen` asks outright as
# well; see the comment on the flag for why that is not the default.
#
# Why vkcube: it is the smallest client that draws every frame through the
# path a game uses — dmabuf, explicit present mode, no toolkit in the way — so
# what is left in the measurement is the compositor. `--c N` makes it exit
# after a fixed number of frames, which turns frame rate into a division rather
# than something scraped out of a client's own guess.
#
# What is reported, per scenario:
#
#   fps            client frames / wall seconds. The client's throughput.
#   cpu_ms_frame   compositor CPU milliseconds spent per client frame. This is
#                  the number that survives a hardware change: it does not care
#                  how fast the GPU is, only how much work the compositor does
#                  to move one buffer to the screen.
#   comp_cpu_pct   the compositor process alone, over the run.
#   sess_cpu_pct   the compositor plus everything it started — Viewport's web
#                  shell, sway's bar. Reported apart from comp_cpu_pct because
#                  the two projects put different amounts of the desktop inside
#                  the compositor process, and folding them together would
#                  flatter whichever one forks more.
#   gpu_pct        mean of amdgpu's gpu_busy_percent across the run.
#   rss_mb         compositor peak resident set (VmHWM).
#
# Present modes are run separately, and they do not answer the same question.
#
# FIFO is the honest one: every frame the client draws is a frame that reaches
# the screen, so its fps is the presented frame rate and its cpu_ms_frame is
# what the compositor spends per frame shown. It is the headline row.
#
# IMMEDIATE and MAILBOX are not presented frame rates and should not be read
# as any. A client in those modes redraws as fast as the compositor hands
# buffers back and most of what it draws is discarded before anyone sees it —
# fourteen thousand frames a second on a sixty-hertz output. What they measure
# is how promptly the compositor releases buffers, and what a client that
# never idles costs it. Read fps as client throughput and comp_cpu_pct as the
# damage.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

only=both
runs=3
scale=1
drm=0
outdir=""
vkcube=""
viewport_bin=""
niri_bin=""
niri_sock=""
clients=4
output=""
# The other monitor, for the multi-monitor scenarios. Empty runs none of them.
#
# Named rather than discovered, because "the second monitor" is not a thing the
# machine can be asked for: sysfs lists connectors in an order nobody chose,
# and picking the second connected one would silently benchmark a different
# pair of screens on a machine where one was unplugged.
second=""
mode=""
# Off by default, and not because it is the wrong thing to measure.
#
# Making a lone window fill its output needs no help from either compositor: a
# tiling layout with no bar and no gaps already does it, which is what the
# configs below set up, and the client size in the report says whether it
# worked. Fullscreen proper is the belt to that pair of braces, and on
# Viewport it has to be asked for over the control socket.
#
# That connection used to be disqualifying — an open control socket pinned a
# core, so measuring through it measured the spin. Fixed in ipc.rs, and the
# flag is safe against a build that has the fix. It stays opt-in because a run
# against an older binary would be back to measuring the spin, and because the
# config already gets the window to the same size without asking anyone.
fullscreen=0

while [ $# -gt 0 ]; do
    case "$1" in
        --only) only=$2; shift 2 ;;
        --runs) runs=$2; shift 2 ;;
        --scale) scale=$2; shift 2 ;;
        --clients) clients=$2; shift 2 ;;
        --output) output=$2; shift 2 ;;
        --second) second=$2; shift 2 ;;
        --mode) mode=$2; shift 2 ;;
        --fullscreen) fullscreen=1; shift ;;
        --no-fullscreen) fullscreen=0; shift ;;
        --drm) drm=1; shift ;;
        --out) outdir=$2; shift 2 ;;
        --vkcube) vkcube=$2; shift 2 ;;
        --viewport) viewport_bin=$2; shift 2 ;;
        --niri) niri_bin=$2; shift 2 ;;
        -h|--help) sed -n '3,52p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

# A second monitor needs two of them, and nested there is only ever one: both
# compositors open a single window on the host, so `--second` there would place
# every client on the same output and report it as though two screens had been
# measured. Refused rather than ignored — a benchmark that runs and means
# something else is worse than one that will not start.
if [ -n "$second" ]; then
    if [ "$drm" != 1 ]; then
        echo "--second needs --drm: nested, both compositors get one output and" >&2
        echo "there is no other screen to put a client on." >&2
        exit 2
    fi
    if [ -z "$output" ]; then
        echo "--second needs --output too: which screen is the busy one and which" >&2
        echo "is being measured is the whole of what these scenarios report." >&2
        exit 2
    fi
    if [ "$second" = "$output" ]; then
        echo "--second is the same output as --output ($output)." >&2
        exit 2
    fi
    # Connected, per the kernel. Catches a typo now rather than as a run whose
    # clients all silently landed on one screen.
    if ! grep -qx connected "/sys/class/drm/"*"-${second}/status" 2>/dev/null; then
        echo "no connected output named '$second'. Connected:" >&2
        for status in /sys/class/drm/card*-*/status; do
            [ -r "$status" ] && [ "$(cat "$status")" = connected ] || continue
            connector=$(basename "$(dirname "$status")")
            echo "  ${connector#*-}" >&2
        done
        exit 2
    fi
fi

# niri, when it is one of the compositors being measured.
#
# Not a dependency of anything here and not in the dev shell, so it is found
# the same way vkcube is: on PATH if someone arranged it, otherwise built and
# cached by the store, which is a download once and instant afterwards.
if [ "$only" = niri ] || [ "$only" = all ]; then
    if [ -z "$niri_bin" ]; then
        niri_bin=$(command -v niri || true)
    fi
    if [ -z "$niri_bin" ] || [ ! -x "$niri_bin" ]; then
        echo "building niri from nixpkgs..." >&2
        niri_out=$(nix build --no-link --print-out-paths nixpkgs#niri 2>/dev/null || true)
        niri_bin=$niri_out/bin/niri
    fi
    if [ ! -x "$niri_bin" ]; then
        echo "no niri: pass --niri PATH, or make it reachable from nixpkgs." >&2
        exit 1
    fi
fi

runtime="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
outdir=${outdir:-$root/bench-results}
mkdir -p "$outdir"

# vkcube is not a dependency of the compositor, so it is not in the dev shell.
# Found rather than required, and named in the error when it is missing.
if [ -z "$vkcube" ]; then
    vkcube=$(command -v vkcube || true)
fi
if [ -z "$vkcube" ] || [ ! -x "$vkcube" ]; then
    echo "no vkcube on PATH. Either:" >&2
    echo "  nix shell nixpkgs#vulkan-tools --command $0 $*" >&2
    echo "  $0 --vkcube /path/to/vkcube" >&2
    exit 1
fi

# Which Viewport to measure: the newest one there is, and say which.
#
# `result` is whatever `nix build` last produced, which is not the same thing
# as the tree that is checked out — a benchmark run against a symlink from
# yesterday measures yesterday's compositor and says nothing about it. That
# has already happened once here: a fixed `--exit-after` was reported as
# broken because `result` predated the fix by a day.
if [ -z "$viewport_bin" ]; then
    newest=""
    for candidate in "$root/result/bin/viewport" "$root/target/release/viewport" \
        "$root/target/debug/viewport"; do
        [ -x "$candidate" ] || continue
        if [ -z "$newest" ] || [ "$candidate" -nt "$newest" ]; then
            newest=$candidate
        fi
    done
    viewport_bin=$newest
fi
if [ "$only" != sway ] && { [ -z "$viewport_bin" ] || [ ! -x "$viewport_bin" ]; }; then
    echo "no viewport binary: build one, or pass --viewport PATH" >&2
    exit 1
fi

# A run without the shell is legitimate — building without the wpe feature is
# how you take WebKit out of a measurement, and the note on --no-shell above
# says so. It is not legitimate *here*: placing a client on a chosen monitor
# goes through the shell, because the shell is what decides which output a
# window opens on. A binary with no shell in it has nothing to ask, so both
# clients would open on one screen and the report would name two.
#
# Worth checking rather than trusting, because the picker above takes the
# newest of result/, target/release/ and target/debug/ — and `cargo test`
# rebuilds target/debug without the feature, so an afternoon of running tests
# leaves the newest binary the one that cannot do this. That is the same trap
# run-drm.sh has a check for, arriving by a different door.
if [ -n "$second" ] && [ "$only" != sway ] &&
    ! grep -qa "starting the shell at" "$viewport_bin"; then
    echo "$viewport_bin has no shell in it: built without --features wpe," >&2
    echo "or overwritten by a cargo test run." >&2
    echo >&2
    echo "--second places each client by asking the shell which monitor to" >&2
    echo "open the next window on. Without a shell there is nothing to ask," >&2
    echo "and every client lands on the same screen." >&2
    echo >&2
    echo "  nix develop --command cargo build --release -p viewport --features wpe" >&2
    exit 1
fi

# A cargo-built binary dlopens libvulkan, libgbm and libEGL rather than linking
# them, so it needs the dev shell's library path at run time and not only at
# build time — the same trap run-drm.sh exists to avoid. Getting it wrong
# produces "Failed to load the Vulkan library", which names nothing useful.
# `result/bin/viewport` is a wrapper that sets its own and needs none of this.
case "$viewport_bin" in
    "$root"/target/*)
        if [ -z "${LD_LIBRARY_PATH:-}" ]; then
            echo "warning: $viewport_bin needs the dev shell's library path." >&2
            echo "  nix develop --command $0 $*" >&2
            echo "(or --viewport $root/result/bin/viewport to measure the packaged build)" >&2
        fi
        ;;
esac

if [ "$drm" = 0 ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
    echo "no WAYLAND_DISPLAY: nested mode needs a session to nest in." >&2
    echo "From a TTY, pass --drm for real scanout instead." >&2
    exit 1
fi

# --------------------------------------------------------------------------
# The machine, recorded once. A benchmark without it is a number with no
# denominator: the next person to read the file cannot tell whether a
# regression is theirs or the GPU's.
# --------------------------------------------------------------------------
card=""
for candidate in /sys/class/drm/card*/device/gpu_busy_percent; do
    [ -r "$candidate" ] && card=$candidate && break
done

gpu_busy() { [ -n "$card" ] && cat "$card" 2>/dev/null || echo ""; }

# --------------------------------------------------------------------------
# Reading CPU out of /proc, by PID, for the compositor and its children.
#
# `ps` would be shorter, but its %CPU is an average over the process lifetime
# and not over the window being measured, which is exactly the wrong thing
# here — the startup cost would be smeared across every scenario.
# --------------------------------------------------------------------------
clock=$(getconf CLK_TCK)

# Ticks of user+system for one PID; zero if it has exited, so that a process
# that dies mid-run subtracts nothing rather than breaking the arithmetic.
ticks_of() {
    local pid=$1 stat
    if ! stat=$(cat "/proc/$pid/stat" 2>/dev/null); then
        echo 0
        return 0
    fi
    # utime and stime are the 12th and 13th fields after the command name,
    # which itself may contain spaces — so cut at the ')' that closes it
    # rather than counting from the front of the line.
    local rest=${stat#*') '}
    # shellcheck disable=SC2086 # deliberate word splitting into positionals
    set -- $rest
    echo $(( ${12:-0} + ${13:-0} ))
}

# Every descendant of a PID, the PID included.
tree_of() {
    local pid=$1
    echo "$pid"
    local kid
    for kid in $(cat "/proc/$pid/task/$pid/children" 2>/dev/null); do
        tree_of "$kid"
    done
}

tree_ticks() {
    local total=0 pid
    for pid in $(tree_of "$1"); do
        total=$(( total + $(ticks_of "$pid") ))
    done
    echo "$total"
}

peak_rss_kb() { awk '/VmHWM/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }

# --------------------------------------------------------------------------
# Starting and stopping a compositor.
#
# Stopping is by socket where a socket exists, and by recorded PID otherwise.
# Never by name: this benchmark runs inside a Viewport session often enough
# that a pattern kill would take the desktop down with it.
# --------------------------------------------------------------------------
comp_pid=""
comp_display=""
comp_kind=""
comp_log=""
sway_sock=""
comp_exe=""
comp_started=0

# When a process began, in clock ticks since boot: field 22 of /proc/pid/stat,
# counting from after the command name.
start_time_of() {
    local stat rest
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || { echo 0; return 0; }
    rest=${stat#*') '}
    # shellcheck disable=SC2086 # deliberate word splitting into positionals
    set -- $rest
    echo "${20:-0}"
}

# What the compositor left behind.
#
# Viewport's web shell runs in processes that call setsid, so by the time the
# compositor exits they are neither in its process group nor among its
# children: killing the compositor orphans them onto init, still running, a
# third of a gigabyte each and spinning. The memory is the smaller problem —
# left alone through a benchmark they turn up as the *next* compositor's CPU.
#
# Picked by executable and by start time, never by name. This is usually run
# from inside a Viewport session, whose compositor is running the same binary
# and would match any pattern; it cannot match a start time from before this
# script launched anything.
sweep_orphans() {
    [ -n "$comp_exe" ] || return 0
    [ "$comp_started" -gt 0 ] || return 0
    local pid exe
    for pid in /proc/[0-9]*; do
        pid=${pid#/proc/}
        exe=$(readlink -f "/proc/$pid/exe" 2>/dev/null) || continue
        [ "$exe" = "$comp_exe" ] || continue
        [ "$(start_time_of "$pid")" -ge "$comp_started" ] || continue
        kill -0 "$pid" 2>/dev/null || continue
        echo "   sweeping orphan $pid" >&2
        kill -TERM "$pid" 2>/dev/null || true
    done
}

start_viewport() {
    comp_kind=viewport
    comp_log=$outdir/viewport.log
    : >"$comp_log"

    # A config of our own, for the same reason sway gets one: otherwise this
    # measures whatever is in ~/.config/viewport/config.json. That is not a
    # hypothetical — the first run on real hardware picked up "scrolling",
    # which gave vkcube a half-width column against sway's whole screen, and
    # "max_refresh", which put Viewport on a 240Hz mode against sway's 120.
    #
    # tiling, so one window fills its output. adaptive_sync off, because sway
    # leaves VRR off by default and a variable refresh rate is a different
    # experiment — and on a 240Hz panel it is a large one, since the display
    # then refreshes when a frame arrives rather than on a fixed clock.
    # "hidden" is the bar mode that draws nothing; "off" is not a value the
    # shell knows and would have left the bar visible.
    local cfg=$outdir/viewport-bench.json
    python3 - "$cfg" "${output:-*}" "$mode" <<'PY'
import json
import sys

path, output, mode = sys.argv[1], sys.argv[2], sys.argv[3]
config = {
    "layout": "tiling",
    "adaptive_sync": False,
    "bar": "hidden",
    "logo": False,
    "tutorial": False,
}
if mode:
    # Every output, not only the named one. Viewport's shell decides which
    # output a window opens on and there is no message that overrides it, so
    # the way to make the client's size predictable is to leave it nowhere
    # else to land: same mode everywhere. An exact name beats "*", so naming
    # one as well is harmless and says which was asked for.
    config["outputs"] = {"*": {"mode": mode}}
    if output != "*":
        config["outputs"][output] = {"mode": mode}
with open(path, "w") as f:
    json.dump(config, f, indent=2)
PY

    local args=(--config "$cfg")
    [ "$drm" = 1 ] && args+=(--drm)
    # No setsid. It forks when it is already a process group leader, which
    # depends on whether the calling shell had job control — so `$!` was
    # sometimes the compositor and sometimes a wrapper that had already
    # exited, and everything downstream reads CPU out of /proc by that pid.
    # The EXIT trap is what stops this from outliving the script.
    # Where the shell lives, spelled out.
    #
    # The compositor falls back to a path relative to the working directory,
    # so a run started from anywhere but the source tree loads a shell that is
    # not there — and the symptom is not an error but a quieter benchmark: no
    # shell means nothing places the window, the compositor waits 2.5s, and
    # then lays it out with its own built-in fallback. The numbers still come
    # out. They are just not numbers about this desktop.
    VIEWPORT_SHELL_URL="${VIEWPORT_SHELL_URL:-file://$root/data/shell/index.html}" \
        "$viewport_bin" "${args[@]}" >"$comp_log" 2>&1 &
    comp_pid=$!
    comp_started=$(start_time_of "$comp_pid")
    sleep 0.5
    # The packaged binary is a wrapper that execs the real one, so ask the
    # kernel what is running rather than trusting the path that was invoked.
    comp_exe=$(readlink -f "/proc/$comp_pid/exe" 2>/dev/null || readlink -f "$viewport_bin")

    # The compositor names its own socket, and the control socket line is the
    # one place it prints the Wayland display it settled on.
    local waited=0
    while [ $waited -lt 200 ]; do
        comp_display=$(sed -n 's/.*control socket at .*viewport-\(wayland-[0-9]*\)\.sock.*/\1/p' \
            "$comp_log" | head -1)
        [ -n "$comp_display" ] && break
        kill -0 "$comp_pid" 2>/dev/null || break
        sleep 0.1
        waited=$(( waited + 1 ))
    done
    if [ -z "$comp_display" ]; then
        echo "viewport never announced a socket." >&2
        viewport_died
        return 1
    fi

    # Announcing the socket is not the same as surviving.
    #
    # The control socket is created while the state is built, which is before
    # the renderer and the backend exist — so a compositor that cannot open
    # the GPU prints this line and then dies, and every later step is left
    # chasing a path that has since been deleted. The first symptom of that
    # was a Python traceback from the view listener saying "No such file or
    # directory", which named the socket and not the reason.
    waited=0
    while [ $waited -lt 100 ]; do
        [ -S "$runtime/viewport-$comp_display.sock" ] && [ -S "$runtime/$comp_display" ] && break
        sleep 0.1
        waited=$(( waited + 1 ))
    done
    if ! kill -0 "$comp_pid" 2>/dev/null || [ ! -S "$runtime/$comp_display" ]; then
        echo "viewport announced $comp_display and then stopped." >&2
        viewport_died
        return 1
    fi

    # The socket file exists before the compositor is ready to draw. Give the
    # shell time to come up, or the first scenario measures WebKit's startup.
    sleep 3
}

# Why a compositor that just started is already gone.
#
# The library path is the usual answer and the one the log is worst at: a
# cargo-built binary dlopens libvulkan, so without the dev shell it fails with
# "Failed to load the Vulkan library" among a thousand lines of tracing.
viewport_died() {
    echo >&2
    sed 's/\x1b\[[0-9;]*m//g' "$comp_log" | grep -iE 'error|failed|panic' | tail -5 >&2
    echo >&2
    case "$viewport_bin" in
        "$root"/target/*)
            echo "$viewport_bin is cargo-built and dlopens libvulkan, libgbm and libEGL," >&2
            echo "so it needs the dev shell at run time:" >&2
            echo >&2
            echo "  nix develop --command $0 $*" >&2
            ;;
        *) echo "full log: $comp_log" >&2 ;;
    esac
}

start_sway() {
    comp_kind=sway
    comp_log=$outdir/sway.log
    : >"$comp_log"
    sway_sock=$runtime/bench-sway.sock
    rm -f "$sway_sock"

    # A config of our own, so the comparison is against sway's compositing and
    # not against whatever bar and wallpaper this machine's dotfiles start.
    #
    # No output size here on purpose. Nested, both compositors open one window
    # on the host and the host decides how big it is, so both end up with the
    # same output and hand the client the same number of pixels — the report
    # prints the size the client was configured at so that this can be
    # checked rather than assumed. Naming a mode does not override the host
    # anyway: sway answers `output * mode` with "Could not find config for
    # output WL-1" and keeps the size the host gave it.
    local cfg=$outdir/sway-bench.config
    cat >"$cfg" <<EOF
default_border none
default_floating_border none
gaps inner 0
gaps outer 0
focus_follows_mouse no
EOF
    # vkcube sets a title and no app_id, so the criterion has to be the title.
    if [ "$fullscreen" = 1 ]; then
        echo 'for_window [title="vkcube"] fullscreen enable' >>"$cfg"
    fi
    if [ -n "$mode" ]; then
        # sway wants the refresh rate suffixed: `2560x1440@239.760` is
        # rejected outright with "Invalid mode refresh rate", while Viewport's
        # config takes exactly that. One --mode for both, so the Hz goes on
        # here rather than in what the user types.
        local sway_mode=$mode
        case "$mode" in
            *@*Hz) ;;
            *@*) sway_mode="${mode}Hz" ;;
        esac
        echo "output ${output:-*} mode $sway_mode" >>"$cfg"
    fi
    if [ -n "$output" ]; then
        # So the workspace the clients open on is the one being measured.
        echo "focus output $output" >>"$cfg"
    fi

    local -a vars=(SWAYSOCK="$sway_sock")
    if [ "$drm" = 1 ]; then
        vars+=(WLR_BACKENDS=drm)
    else
        vars+=(WLR_BACKENDS=wayland WLR_WL_OUTPUTS=1)
    fi
    # Which socket is sway's, by watching one appear.
    #
    # sway prints the display it took at info level, which the default log
    # level does not include, and raising it to get one line would change what
    # is being benchmarked. Its IPC does not report it either — the name only
    # reaches the environment of processes sway itself starts. Watching the
    # runtime directory needs neither.
    #
    # By timestamp and not by name. A runtime directory collects the socket
    # files of every session that ever ran, and the compositor started second
    # takes the lowest free number — which is usually a name that is already
    # sitting there. Comparing the listing before against the listing after
    # therefore found nothing at all, and waited twenty seconds to say so.
    local stamp=$outdir/.stamp
    : >"$stamp"

    # No setsid, for the reason given in start_viewport.
    env "${vars[@]}" sway -c "$cfg" >"$comp_log" 2>&1 &
    comp_pid=$!

    local waited=0
    while [ $waited -lt 200 ]; do
        comp_display=$(find "$runtime" -maxdepth 1 -type s -name 'wayland-*' -newer "$stamp" \
            -printf '%f\n' 2>/dev/null | head -1)
        [ -n "$comp_display" ] && break
        kill -0 "$comp_pid" 2>/dev/null || { echo "sway exited; see $comp_log" >&2; return 1; }
        sleep 0.1
        waited=$(( waited + 1 ))
    done
    [ -n "$comp_display" ] || { echo "sway never opened a socket; see $comp_log" >&2; return 1; }
    sleep 2
}

start_niri() {
    comp_kind=niri
    comp_log=$outdir/niri.log
    : >"$comp_log"

    # A config of our own, for the reason the other two get one: otherwise this
    # measures whatever is in ~/.config/niri/config.kdl.
    #
    # niri is the model Viewport's scrolling layout was written from, so the
    # settings that matter are the ones that decide how much of the output one
    # window gets: no gaps, no focus ring, no border, and a column that is the
    # whole width. Animations off, because an animation running while a client
    # is being timed is compositor work nobody asked for and sway's config has
    # no equivalent to leave on.
    local cfg=$outdir/niri-bench.kdl
    cat >"$cfg" <<'EOF'
layout {
    gaps 0
    focus-ring { off; }
    border { off; }
    default-column-width { proportion 1.0; }
}
animations { off; }
prefer-no-csd
hotkey-overlay { skip-at-startup; }
EOF
    if [ -n "$mode" ]; then
        # niri wants the rate as a bare number after the size, which is the
        # same shape --mode already has.
        {
            echo "output \"${output:-*}\" {"
            echo "    mode \"${mode}\""
            echo "}"
        } >>"$cfg"
    fi

    if ! "$niri_bin" validate -c "$cfg" >>"$comp_log" 2>&1; then
        echo "the generated niri config is not valid; see $comp_log" >&2
        return 1
    fi

    # Same socket-watching as sway, and for the same reason: the display name
    # only reaches the environment of processes the compositor itself starts.
    local stamp=$outdir/.stamp
    : >"$stamp"

    NIRI_CONFIG="$cfg" "$niri_bin" >"$comp_log" 2>&1 &
    comp_pid=$!

    local waited=0
    while [ $waited -lt 200 ]; do
        comp_display=$(find "$runtime" -maxdepth 1 -type s -name 'wayland-*' -newer "$stamp" \
            -printf '%f\n' 2>/dev/null | head -1)
        [ -n "$comp_display" ] && break
        kill -0 "$comp_pid" 2>/dev/null || { echo "niri exited; see $comp_log" >&2; return 1; }
        sleep 0.1
        waited=$(( waited + 1 ))
    done
    [ -n "$comp_display" ] || { echo "niri never opened a socket; see $comp_log" >&2; return 1; }
    # niri's own IPC is reached through the display it took.
    niri_sock=$runtime/niri.$comp_display.sock
    sleep 2
}

# `niri msg` against the instance this script started, rather than whatever
# else may be running. It finds its socket through NIRI_SOCKET.
niri_msg() {
    NIRI_SOCKET="$niri_sock" WAYLAND_DISPLAY="$comp_display" \
        "$niri_bin" msg "$@" 2>/dev/null
}

# Make every window that appears fill its output, for as long as the
# compositor lives.
#
# sway does this from its config, at map time, with no timing to get wrong.
# Viewport's equivalent is a message on the control socket, and the socket
# speaks the shell's own message set — so this listens for view.added and
# answers it, exactly as the shell would.
#
# It runs for the whole session rather than per measurement on purpose. A
# measurement is two passes at different frame counts, and the short pass can
# be over in well under a second: anything that reached in afterwards to
# fullscreen a window would catch one pass and miss the other, and the
# subtraction between them would then be comparing two different window sizes.
#
# The same listener also answers "which output did the client land on", which
# is worth knowing: the first run on real hardware put sway on DP-3 and
# Viewport on DP-1 without either of them saying so. In that role it only
# watches, and it is only connected while the geometry probe runs — never
# across a timed pass, because of the spin above.
fullscreener_pid=""

# mode: "fullscreen" to answer view.added, "record" to only write it down.
start_view_listener() {
    local mode=$1
    [ "$comp_kind" = viewport ] || return 0
    # Advisory, both roles: fullscreen is belt to the config's braces, and the
    # output name is a field in a report. Neither is worth ending a run over,
    # and this used to end one with a traceback.
    if [ ! -S "$runtime/viewport-$comp_display.sock" ]; then
        echo "   no control socket; not watching views" >&2
        return 0
    fi
    python3 - "$runtime/viewport-$comp_display.sock" "$outdir/views.log" "$mode" <<'PY' &
import json
import socket
import sys

sock, log, mode = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.connect(sock)
except OSError as e:
    print(f"   not watching views: {e}", file=sys.stderr)
    raise SystemExit(0)

connection.sendall(b'{"type":"view.query"}\n')
buffer = b""
with open(log, "a", buffering=1) as record:
    while True:
        try:
            chunk = connection.recv(65536)
        except OSError:
            break
        if not chunk:
            break
        buffer += chunk
        while b"\n" in buffer:
            line, buffer = buffer.split(b"\n", 1)
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if message.get("type") != "view.added":
                continue
            view = message.get("id")
            record.write(
                "view {} on {} at {}x{}\n".format(
                    view,
                    message.get("output"),
                    message.get("width"),
                    message.get("height"),
                )
            )
            if mode == "fullscreen":
                connection.sendall(
                    json.dumps(
                        {"type": "view.fullscreen", "id": view, "fullscreen": True}
                    ).encode()
                    + b"\n"
                )
PY
    fullscreener_pid=$!
}

stop_view_listener() {
    [ -n "$fullscreener_pid" ] || return 0
    kill "$fullscreener_pid" 2>/dev/null || true
    wait "$fullscreener_pid" 2>/dev/null || true
    fullscreener_pid=""
}

stop_compositor() {
    stop_view_listener
    [ -n "$comp_pid" ] || return 0
    if [ "$comp_kind" = viewport ]; then
        python3 - "$runtime/viewport-$comp_display.sock" <<'PY' || true
import socket, sys
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as c:
    c.settimeout(5)
    try:
        c.connect(sys.argv[1])
        c.sendall(b'{"type":"quit"}\n')
    except OSError:
        pass
PY
    elif [ "$comp_kind" = niri ]; then
        niri_msg action quit --skip-confirmation >/dev/null 2>&1 || true
    else
        SWAYSOCK=$sway_sock swaymsg exit >/dev/null 2>&1 || true
    fi

    # Give it a second to go on its own, then take the one PID we started.
    local waited=0
    while [ $waited -lt 50 ] && kill -0 "$comp_pid" 2>/dev/null; do
        sleep 0.1
        waited=$(( waited + 1 ))
    done
    kill -0 "$comp_pid" 2>/dev/null && kill -TERM "$comp_pid" 2>/dev/null || true
    sleep 0.5
    kill -0 "$comp_pid" 2>/dev/null && kill -KILL "$comp_pid" 2>/dev/null || true
    comp_pid=""
    sweep_orphans
    comp_exe=""
    comp_started=0
}
trap 'stop_compositor' EXIT

# What size the client was actually given to draw at.
#
# Asked of the client rather than of the compositor. Viewport's control socket
# reports the size a window asked for and sway's tree reports the size its own
# layout settled on, which are two different questions and neither is "how many
# pixels did vkcube fill". The configure event is that question, it is the same
# event on both, and reading it needs nothing but a client that is already
# being run. Slow — WAYLAND_DEBUG prints every message — so it is a probe of
# its own rather than something folded into a timed run.
client_geometry() {
    env WAYLAND_DISPLAY="$comp_display" WAYLAND_DEBUG=1 "$vkcube" \
        --wsi wayland --c 400 --present_mode 2 2>&1 |
        sed -n 's/.*xdg_toplevel#[0-9]*\.configure(\([0-9]*\), \([0-9]*\).*/\1x\2/p' |
        grep -v '^0x0$' | tail -1
}

# Which connector the client ended up on.
#
# Worth printing because it is not always the same one on both sides: the
# first run on this machine put sway on DP-3 and Viewport on DP-1 without
# either of them saying so, and two panels are two sets of timings even when
# they are the same model.
# The mode each compositor actually settled on.
#
# Asked for separately from --mode because asking and getting are different
# things, and the difference is invisible in a frame rate. The first run on
# real hardware had Viewport on 240Hz and sway on 120 — each had simply taken
# what it preferred — and nothing in the results said so.
comp_mode() {
    if [ "$comp_kind" = niri ]; then
        # niri reports outputs as an object keyed by name, each with the mode
        # it is running and an index into its own mode list.
        niri_msg -j outputs |
            python3 -c 'import json,sys
try:
    outputs = json.load(sys.stdin)
except Exception:
    print("unknown"); raise SystemExit
if isinstance(outputs, dict):
    outputs = list(outputs.values())
parts = []
for o in outputs:
    modes, current = o.get("modes") or [], o.get("current_mode")
    if current is None or not isinstance(current, int) or current >= len(modes):
        continue
    m = modes[current]
    parts.append("{}={}x{}@{:.3f}".format(
        o.get("name"), m.get("width"), m.get("height"),
        (m.get("refresh_rate") or 0) / 1000))
print(" ".join(parts) if parts else "unknown")' 2>/dev/null || echo unknown
        return
    fi
    if [ "$comp_kind" = sway ]; then
        SWAYSOCK=$sway_sock swaymsg -t get_outputs -r >"$outdir/.outputs.json" 2>/dev/null || true
        python3 - "$outdir/.outputs.json" <<'PY'
import json
import sys

try:
    with open(sys.argv[1]) as f:
        outputs = [o for o in json.load(f) if o.get("active")]
    if not outputs:
        print("unknown")
    else:
        seen = []
        for output in outputs:
            mode = output.get("current_mode") or {}
            seen.append(
                "{}={}x{}@{:.3f}".format(
                    output.get("name"),
                    mode.get("width"),
                    mode.get("height"),
                    (mode.get("refresh") or 0) / 1000.0,
                )
            )
        print(" ".join(seen))
except Exception:
    print("unknown")
PY
    else
        # smithay logs the DRM mode it created each surface with; the winit
        # backend has no mode of its own to report.
        sed 's/\x1b\[[0-9;]*m//g' "$comp_log" |
            grep -oE 'size: \([0-9]+, [0-9]+\).*vrefresh: [0-9]+' |
            sed -E 's/size: \(([0-9]+), ([0-9]+)\).*vrefresh: ([0-9]+)/\1x\2@\3/' |
            sort -u | tr '\n' ' ' | sed 's/ $//' | grep . || echo "nested"
    fi
}

client_output() {
    if [ "$comp_kind" = niri ]; then
        niri_msg -j focused-output |
            python3 -c 'import json,sys
try:
    print(json.load(sys.stdin).get("name") or "unknown")
except Exception:
    print("unknown")' 2>/dev/null || echo unknown
        return
    fi
    if [ "$comp_kind" = sway ]; then
        SWAYSOCK=$sway_sock swaymsg -t get_workspaces -r >"$outdir/.workspaces.json" 2>/dev/null || true
        python3 - "$outdir/.workspaces.json" <<'PY'
import json
import sys

try:
    with open(sys.argv[1]) as f:
        focused = [w for w in json.load(f) if w.get("focused")]
    print(focused[0]["output"] if focused else "unknown")
except Exception:
    print("unknown")
PY
    else
        # Recorded by the fullscreener as each view appeared.
        if [ -s "$outdir/views.log" ]; then
            sed -n 's/.* on \([^ ]*\) at .*/\1/p' "$outdir/views.log" | tail -1
        else
            echo "unknown"
        fi
    fi
}

# --------------------------------------------------------------------------
# One pass: N clients, each drawing a fixed number of frames, with the
# compositor's CPU and the GPU's occupancy read around it.
#
# Echoes "wall comp_ticks tree_ticks gpu_mean".
# --------------------------------------------------------------------------
one_pass() {
    local mode=$1 n=$2 count=$3

    local gpu_file=$outdir/.gpu-samples
    : >"$gpu_file"
    # GPU busy is a gauge, not a counter, so it has to be sampled across the
    # run rather than differenced around it.
    (
        while :; do
            gpu_busy >>"$gpu_file"
            sleep 0.2
        done
    ) &
    local sampler=$!

    local before_comp before_tree start end
    before_comp=$(ticks_of "$comp_pid")
    before_tree=$(tree_ticks "$comp_pid")
    start=$(date +%s.%N)

    local -a pids=()
    local i
    for (( i = 0; i < n; i++ )); do
        env WAYLAND_DISPLAY="$comp_display" "$vkcube" \
            --wsi wayland --c "$count" --present_mode "$mode" \
            >/dev/null 2>&1 &
        pids+=($!)
    done
    for i in "${pids[@]}"; do wait "$i" || true; done

    end=$(date +%s.%N)
    local after_comp after_tree
    after_comp=$(ticks_of "$comp_pid")
    after_tree=$(tree_ticks "$comp_pid")

    kill "$sampler" 2>/dev/null || true
    wait "$sampler" 2>/dev/null || true

    local gpu_mean
    gpu_mean=$(awk '{s += $1; c++} END {print c ? s / c : 0}' "$gpu_file")
    awk -v start="$start" -v end="$end" 'BEGIN { printf "%.6f", end - start }'
    printf ' %d %d %s\n' \
        "$(( after_comp - before_comp ))" "$(( after_tree - before_tree ))" "$gpu_mean"
}

# --------------------------------------------------------------------------
# Put the next window on a named monitor.
#
# A Wayland client has no say in which output it lands on — that is the
# compositor's decision — so the only way to benchmark two screens at once is
# to tell the compositor where the next one goes, and then start it.
#
# The two are told differently, and the asymmetry is worth knowing about. sway
# takes any command its config language accepts over its IPC, so this is the
# same sentence a user would type. Viewport's control socket is a fixed set of
# typed requests and none of them reached the shell at all until
# `shell.command` was added for this; layout is entirely the shell's, so
# without it a keypress was the only thing that could move focus between
# monitors. `output.active` is not the same thing and does not work here: it
# runs shell to compositor, setting the compositor's own idea of which output
# is active, which the shell never reads back.
# --------------------------------------------------------------------------
place_on() {
    local target=$1
    # Kept beside the results rather than only on the terminal. Everything
    # about why a placement failed was going to a TTY that scrolls, which is
    # the one place it cannot be read afterwards — so two runs came back with
    # nothing but an empty raw-mm.tsv to explain themselves.
    local plog=$outdir/placement.log
    echo "--- $comp_kind: asking for $target at $(date +%H:%M:%S)" >>"$plog"
    case "$comp_kind" in
        sway)
            SWAYSOCK=$sway_sock swaymsg focus output "$target" >>"$plog" 2>&1 || true
            SWAYSOCK=$sway_sock swaymsg -t get_workspaces -r 2>/dev/null |
                python3 -c 'import json,sys
try:
    ws = json.load(sys.stdin)
except Exception:
    print("  could not read workspaces"); raise SystemExit
focused = [w for w in ws if w.get("focused")]
print("  focused output now: {}".format(focused[0]["output"] if focused else "none"))' >>"$plog" 2>&1 || true
            ;;
        niri)
            niri_msg action focus-monitor "$target" >>"$plog" 2>&1 || true
            niri_msg -j focused-output |
                python3 -c 'import json,sys
try:
    print("  focused output now: {}".format(json.load(sys.stdin).get("name")))
except Exception:
    print("  could not read the focused output")' >>"$plog" 2>&1 || true
            ;;
        viewport)
            # Not `|| true`. A compositor built before shell.command existed
            # answers this with an "unknown IPC message type" error, and
            # swallowing that gives a run in which both clients open on the
            # same screen, every scenario completes, and the report says two
            # monitors were measured. A benchmark that quietly measures
            # something else is the failure this whole harness is written
            # against, so the reply is read and a rejection is fatal.
            if ! python3 - "$runtime/viewport-$comp_display.sock" "$target" 2>>"$plog" <<'PY'
import json, socket, sys, time
sock, target = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX)
s.settimeout(3)
try:
    s.connect(sock)
    # By name rather than by direction. A direction would depend on how the
    # monitors are physically arranged, and would quietly measure the wrong
    # screen on a desk where they are stacked rather than side by side.
    s.sendall(json.dumps({
        "type": "shell.command",
        "command": "output.focus",
        "args": [target],
    }).encode() + b"\n")
    # Then wait until the move has actually happened, rather than until the
    # message has been sent.
    #
    # The command going out proves nothing: it reaches the shell through the
    # web view, the shell answers it on its own event loop, and only then does
    # it tell the compositor. What settles it is `output.layout`, whose
    # `active` field is the compositor's record of the output the shell last
    # named — which is the same thing that decides where the next window
    # opens. So this asks for the layout until that field is the output that
    # was asked for.
    #
    # Sleeping a fixed time instead is what this replaced, and it is the
    # difference between a benchmark that fails and one that quietly puts both
    # clients on one screen and reports two.
    deadline = time.monotonic() + 5
    buffered = b""
    asked = 0.0
    seen_active = []
    while time.monotonic() < deadline:
        if time.monotonic() - asked > 0.25:
            s.sendall(b'{"type":"output.query"}\n')
            asked = time.monotonic()
        try:
            chunk = s.recv(65536)
        except socket.timeout:
            continue
        if not chunk:
            break
        buffered += chunk
        while b"\n" in buffered:
            line, buffered = buffered.split(b"\n", 1)
            if not line.strip():
                continue
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if (message.get("type") == "error"
                    and message.get("context") in ("shell.command", "output.query")):
                sys.stderr.write("the compositor rejected {}: {}\n".format(
                    message.get("context"), message.get("message", "")))
                sys.exit(1)
            if message.get("type") != "output.layout":
                continue
            outputs = message.get("outputs") or []
            names = [o.get("name") for o in outputs]
            if target not in names:
                sys.stderr.write(
                    "the compositor has no output named {!r}; it has {}\n".format(
                        target, ", ".join(n for n in names if n)))
                sys.exit(1)
            seen_active = [o.get("name") for o in outputs if o.get("active")]
            sys.stderr.write("  saw outputs {} active {}\n".format(
                ", ".join(n for n in names if n),
                ", ".join(seen_active) if seen_active else "(none)"))
            if seen_active[:1] == [target]:
                sys.stderr.write("  moved to {}\n".format(target))
                sys.exit(0)
    sys.stderr.write(
        "the shell did not move to {} within 5s; it is still on {}\n".format(
            target, ", ".join(seen_active) if seen_active else "an unknown output"))
    sys.exit(1)
finally:
    s.close()
PY
            then
                echo >&2
                echo "could not place a client on '$target'." >&2
                echo >&2
                echo "This needs a compositor with the shell.command request" >&2
                echo "(67b84fc or later). An older one has no way to be told" >&2
                echo "which monitor to open a window on, so every client would" >&2
                echo "land on one screen and the run would report two." >&2
                echo >&2
                echo "  nix develop --command cargo build --release -p viewport --features wpe" >&2
                exit 1
            fi
            ;;
    esac
    # The shell answers this on its own event loop and the compositor has to
    # round-trip through the web view to do it, so the change is not in effect
    # when the socket write returns. Starting a client into that gap puts it on
    # whichever screen was focused before.
    sleep 0.4
}

# --------------------------------------------------------------------------
# How many vkcube windows the compositor currently has mapped.
#
# Only used to know when it is worth asking where they are; a client that has
# not mapped is on no output, and asking early answers about the wrong moment.
# --------------------------------------------------------------------------
count_clients() {
    case "$comp_kind" in
        niri)
            niri_msg -j windows |
                python3 -c 'import json,sys
try:
    print(sum(1 for w in json.load(sys.stdin)
              if "vkcube" in ((w.get("title") or "") + (w.get("app_id") or "")).lower()))
except Exception:
    print(0)' 2>/dev/null || echo 0
            ;;
        sway)
            SWAYSOCK=$sway_sock swaymsg -t get_tree -r 2>/dev/null |
                python3 -c 'import json,sys
def walk(node):
    n = 1 if (node.get("pid") and "vkcube" in (node.get("name") or "").lower()) else 0
    for key in ("nodes", "floating_nodes"):
        for child in node.get(key) or []:
            n += walk(child)
    return n
try:
    print(walk(json.load(sys.stdin)))
except Exception:
    print(0)' 2>/dev/null || echo 0
            ;;
        viewport)
            python3 - "$runtime/viewport-$comp_display.sock" <<'PY' 2>/dev/null || echo 0
import json, socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.settimeout(1)
seen = set()
try:
    s.connect(sys.argv[1])
    s.sendall(b'{"type":"view.query"}\n')
    buffered, deadline = b"", time.monotonic() + 1.5
    while time.monotonic() < deadline:
        try:
            chunk = s.recv(65536)
        except socket.timeout:
            break
        if not chunk:
            break
        buffered += chunk
        while b"\n" in buffered:
            line, buffered = buffered.split(b"\n", 1)
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if message.get("type") == "view.added":
                seen.add(message.get("id"))
finally:
    s.close()
print(len(seen))
PY
            ;;
        *) echo 0 ;;
    esac
}

# --------------------------------------------------------------------------
# Which output each client is actually on, asked while they are both running.
#
# Written to placement.log beside the request that was supposed to put them
# there, so a run can be read afterwards and believed — or not.
# --------------------------------------------------------------------------
verify_placement() {
    local plog=$outdir/placement.log

    # Wait for both clients to be on screen before asking where they are.
    #
    # This is called the moment the second one is started, and a client that
    # has not mapped yet is on no output at all. Sampling once reported one
    # window and left it looking as though the other compositor had put both
    # on one screen — which is a false accusation of exactly the fault this
    # check exists to detect, and it fooled me first. Viewport happened to
    # pass only because its placement round-trips through the shell and takes
    # long enough for the client to appear in the meantime; sway's is two
    # swaymsg calls and got there first.
    local waited=0
    while [ "$waited" -lt 50 ]; do
        [ "$(count_clients)" -ge 2 ] && break
        sleep 0.1
        waited=$(( waited + 1 ))
    done
    echo "  verifying (after ${waited}00ms, $(count_clients) clients):" >>"$plog"

    case "$comp_kind" in
        niri)
            # niri gives each window its workspace, and each workspace its
            # output, so the two lists are joined rather than a tree walked.
            { niri_msg -j windows; echo "@@"; niri_msg -j workspaces; } |
                python3 -c 'import json,sys
raw = sys.stdin.read().split("@@")
try:
    windows, workspaces = json.loads(raw[0]), json.loads(raw[1])
except Exception as e:
    print("    could not read the window list: {}".format(e)); raise SystemExit
where = {w.get("id"): w.get("output") for w in workspaces}
found = False
for w in windows:
    name = (w.get("title") or "") + (w.get("app_id") or "")
    if "vkcube" not in name.lower():
        continue
    found = True
    print("    window {} on {}".format(
        w.get("id"), where.get(w.get("workspace_id"), "?")))
if not found:
    print("    no vkcube windows reported")' >>"$plog" 2>&1 || true
            ;;
        sway)
            SWAYSOCK=$sway_sock swaymsg -t get_tree -r 2>/dev/null |
                python3 -c 'import json,sys
def walk(node, output):
    if node.get("type") == "output":
        output = node.get("name", output)
    name = node.get("name") or ""
    if node.get("pid") and "vkcube" in name.lower():
        print("    {} on {}".format(name, output))
    for key in ("nodes", "floating_nodes"):
        for child in node.get(key) or []:
            walk(child, output)
try:
    walk(json.load(sys.stdin), "?")
except Exception as e:
    print("    could not read the tree: {}".format(e))' >>"$plog" 2>&1 || true
            ;;
        viewport)
            # view.query replays every mapped window with the output it is on.
            # That field used to be one guess for the whole list — the output a
            # *new* window would open on — which is why this could not be asked
            # before; see notify_views in state.rs.
            python3 - "$runtime/viewport-$comp_display.sock" <<'PY' >>"$plog" 2>&1 || true
import json, socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.settimeout(2)
try:
    s.connect(sys.argv[1])
    s.sendall(b'{"type":"view.query"}\n')
    seen, buffered, deadline = {}, b"", time.monotonic() + 3
    while time.monotonic() < deadline:
        try:
            chunk = s.recv(65536)
        except socket.timeout:
            break
        if not chunk:
            break
        buffered += chunk
        while b"\n" in buffered:
            line, buffered = buffered.split(b"\n", 1)
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if message.get("type") == "view.added":
                seen[message.get("id")] = message.get("output")
    for view, output in sorted(seen.items(), key=lambda kv: kv[0] or 0):
        print("    view {} on {}".format(view, output))
    if not seen:
        print("    no windows reported")
finally:
    s.close()
PY
            ;;
    esac
}

# --------------------------------------------------------------------------
# One multi-monitor pass: a client on each screen, timed apart.
#
# The single-output pass times the batch, which is all that fps means when
# every client is on the same screen. Here it is the wrong measurement twice
# over: the two clients are on outputs that may not even share a refresh rate,
# and the number being looked for is whether *one* of them is being starved by
# the other. So each client's own wall time is recorded and each screen gets
# its own frame rate.
#
# Echoes "wall_primary wall_second comp_ticks tree_ticks gpu_mean".
# --------------------------------------------------------------------------
one_pass_mm() {
    local mode_a=$1 count_a=$2 mode_b=$3 count_b=$4

    local gpu_file=$outdir/.gpu-samples
    : >"$gpu_file"
    (
        while :; do
            gpu_busy >>"$gpu_file"
            sleep 0.2
        done
    ) &
    local sampler=$!

    local before_comp before_tree
    before_comp=$(ticks_of "$comp_pid")
    before_tree=$(tree_ticks "$comp_pid")

    # The busy client first, and left running for the whole of the other's
    # measurement. That ordering is the test: what is being asked is whether a
    # screen kept at full rate stops the *other* screen being paced, which is
    # only a question while both are drawing.
    place_on "$output"
    local start_a
    start_a=$(date +%s.%N)
    env WAYLAND_DISPLAY="$comp_display" "$vkcube" \
        --wsi wayland --c "$count_a" --present_mode "$mode_a" \
        >/dev/null 2>&1 &
    local pid_a=$!

    # Mapped before the second is placed, or moving focus moves nothing and
    # both clients open on the same screen — which reads as a successful run
    # and measures one monitor twice.
    sleep 1

    place_on "$second"
    local start_b
    start_b=$(date +%s.%N)
    env WAYLAND_DISPLAY="$comp_display" "$vkcube" \
        --wsi wayland --c "$count_b" --present_mode "$mode_b" \
        >/dev/null 2>&1 &
    local pid_b=$!

    # Where the clients actually are, while both are still up.
    #
    # Everything up to here moves *focus* and then starts something. Nothing
    # has ever checked that the window went where the focus went, and the
    # frame rates cannot stand in for it: with both panels at the same
    # refresh, a FIFO client reads the same on either screen, so a run with
    # both clients on one monitor produces numbers indistinguishable from a
    # correct one. Advisory rather than fatal — a run that measured the wrong
    # thing should say so in the results, not vanish at the end of it.
    verify_placement

    wait "$pid_b" 2>/dev/null || true
    local end_b
    end_b=$(date +%s.%N)
    wait "$pid_a" 2>/dev/null || true
    local end_a
    end_a=$(date +%s.%N)

    kill "$sampler" 2>/dev/null || true
    wait "$sampler" 2>/dev/null || true

    local after_comp after_tree gpu_mean
    after_comp=$(ticks_of "$comp_pid")
    after_tree=$(tree_ticks "$comp_pid")
    gpu_mean=$(awk '{s += $1; c++} END {print c ? s / c : 0}' "$gpu_file")

    awk -v sa="$start_a" -v ea="$end_a" -v sb="$start_b" -v eb="$end_b" \
        'BEGIN { printf "%.6f %.6f", ea - sa, eb - sb }'
    printf ' %d %d %s\n' \
        "$(( after_comp - before_comp ))" "$(( after_tree - before_tree ))" "$gpu_mean"
}

# --------------------------------------------------------------------------
# One measurement: the same scenario at two frame counts, subtracted.
#
# A single timed run cannot be divided into a frame rate, because a fixed cost
# sits in front of every one of them — Vulkan initialisation, the first
# configure, shader compilation, and the stretch before the window is mapped
# during which vkcube draws at several thousand frames a second into a surface
# nobody is compositing. At 400 frames that prefix *is* the measurement: this
# harness first reported 5164fps for a client that had not been on screen yet.
#
# Running the same thing twice and dividing the differences removes any cost
# that does not scale with frame count, whatever it was, without having to
# know what it was or ask the client to tell us. The same subtraction is what
# makes the CPU figure the compositor's marginal cost per frame rather than
# its startup smeared over a short run.
# --------------------------------------------------------------------------
measure() {
    local scenario=$1 mode=$2 n=$3 low=$4 high=$5 run=$6

    local a b
    a=$(one_pass "$mode" "$n" "$low")
    b=$(one_pass "$mode" "$n" "$high")
    # shellcheck disable=SC2086 # four fields, deliberately split
    set -- $a
    local wall_a=$1 comp_a=$2 tree_a=$3
    # shellcheck disable=SC2086
    set -- $b
    local wall_b=$1 comp_b=$2 tree_b=$3 gpu=$4

    local rss
    rss=$(peak_rss_kb "$comp_pid")

    awk -v scenario="$scenario" -v kind="$comp_kind" -v run="$run" \
        -v wall_a="$wall_a" -v wall_b="$wall_b" \
        -v comp_a="$comp_a" -v comp_b="$comp_b" \
        -v tree_a="$tree_a" -v tree_b="$tree_b" \
        -v low="$low" -v high="$high" -v n="$n" \
        -v clock="$clock" -v rss="$rss" -v geometry="$comp_geometry" \
        -v idle="$idle_comp_pct" -v gpu="$gpu" '
    # Every quotient is worked out before the printf. Inside an argument list
    # awk reads ">" as a redirection, so the comparisons have to happen where
    # there is nothing to redirect to.
    BEGIN {
        wall = wall_b - wall_a
        total = (high - low) * n
        comp_s = (comp_b - comp_a) / clock
        tree_s = (tree_b - tree_a) / clock
        # What the compositor would have burned over this stretch with no
        # client at all, taken back out. Without it a compositor that redraws
        # on a timer is charged its whole standing cost against however many
        # frames the client happened to ask for, and looks worse the fewer of
        # them there were.
        net_s = comp_s - (idle / 100) * wall
        if (net_s < 0) net_s = 0
        fps = 0; cpu_ms_frame = 0; net_ms_frame = 0; comp_pct = 0; tree_pct = 0
        if (wall > 0) {
            fps = total / wall
            comp_pct = comp_s * 100 / wall
            tree_pct = tree_s * 100 / wall
        }
        if (total > 0) {
            cpu_ms_frame = comp_s * 1000 / total
            net_ms_frame = net_s * 1000 / total
        }
        printf "%s\t%s\t%d\t%.2f\t%.2f\t%.4f\t%.4f\t%.1f\t%.1f\t%.1f\t%.1f\t%s\n",
            kind, scenario, run, wall, fps, cpu_ms_frame, net_ms_frame,
            comp_pct, tree_pct, gpu, rss / 1024, geometry
    }'
}

# --------------------------------------------------------------------------
# What the compositor costs with nothing to draw.
#
# Separate from the scenarios because it answers a different question, and one
# a frame rate cannot: a compositor that redraws the whole screen on a timer
# burns this whether or not anybody asked for a frame. Ten seconds, no client,
# nothing moving.
# --------------------------------------------------------------------------
measure_idle() {
    local seconds=$1
    local before_comp before_tree after_comp after_tree
    before_comp=$(ticks_of "$comp_pid")
    before_tree=$(tree_ticks "$comp_pid")
    sleep "$seconds"
    after_comp=$(ticks_of "$comp_pid")
    after_tree=$(tree_ticks "$comp_pid")
    awk -v comp="$(( after_comp - before_comp ))" -v tree="$(( after_tree - before_tree ))" \
        -v clock="$clock" -v seconds="$seconds" \
        'BEGIN { printf "%.1f %.1f", comp * 100 / clock / seconds, tree * 100 / clock / seconds }'
}

# --------------------------------------------------------------------------
# The scenarios.
#
# name, present mode, concurrent clients, low frame count, high frame count.
#
# The two counts differ per scenario because they are chosen in seconds, not
# in frames: FIFO is pinned near the refresh rate and IMMEDIATE runs two
# orders of magnitude above it, so one pair of counts cannot give both a run
# long enough to be steady and short enough to finish.
# --------------------------------------------------------------------------
scale_count() { awk -v n="$1" -v s="$scale" 'BEGIN { printf "%d", (n * s < 60) ? 60 : n * s }'; }

scenarios=(
    "fifo:2:1:180:780"
    "fifo-x${clients}:2:${clients}:180:780"
    "immediate:0:1:3000:15000"
    "mailbox:1:1:3000:15000"
)

# --------------------------------------------------------------------------
# The multi-monitor scenarios.
#
# name : present mode on the busy screen : present mode on the other one :
# frames the other one draws.
#
# The busy client's frame count is not listed because it is not measured — it
# is there to keep its screen saturated for the whole of the other's run, and
# is given enough frames to outlast it.
#
# Two scenarios, and they ask different questions.
#
#   mm-fifo    both screens paced normally. Each client should reach its own
#              output's refresh rate, which on a desk with a 240Hz and a 120Hz
#              panel is two different numbers — and a compositor that paces
#              off the device rather than off each screen cannot produce both.
#
#   mm-stress  one screen driven by a client that never idles, the other paced
#              normally. The second number is the whole point: it is what a
#              terminal on your other monitor gets while something is running
#              flat out over here.
#
# That second one is not hypothetical. It is the shape of the bug in
# 8eada16 — the barrier tick asked the device whether anything had flipped
# lately rather than asking each screen, so a screen animating at the refresh
# rate kept the stamp fresh and silenced the tick for the other one, whose
# windows that flip never visits. A terminal there fell to 8.8 commits a
# second on a 240Hz panel. Nothing measured on one output can see it.
# --------------------------------------------------------------------------
mm_scenarios=(
    "mm-fifo:2:2:600"
    "mm-stress:0:2:600"
)

mm_results=$outdir/raw-mm.tsv
printf 'compositor\tscenario\trun\toutput\trole\tfps\twall_s\tcomp_cpu_pct\tsess_cpu_pct\tgpu_pct\n' >"$mm_results"

results=$outdir/raw.tsv
printf 'compositor\tscenario\trun\twall_s\tfps\tcpu_ms_frame\tnet_ms_frame\tcomp_cpu_pct\tsess_cpu_pct\tgpu_pct\trss_mb\tclient_size\n' >"$results"
: >"$outdir/environment.txt"

comp_geometry=unknown
idle_comp_pct=0
idle_sess_pct=0

bench_one() {
    local which=$1
    echo "== $which ==" >&2
    case "$which" in
        viewport) start_viewport ;;
        sway) start_sway ;;
        niri) start_niri ;;
        *) echo "unknown compositor: $which" >&2; return 1 ;;
    esac
    echo "   display $comp_display, pid $comp_pid" >&2
    [ "$fullscreen" = 1 ] && start_view_listener fullscreen

    # Put Viewport on the output that was asked for.
    #
    # sway has had this since the beginning — start_sway writes `focus output`
    # into its config — and Viewport had no equivalent at any level, which the
    # note at the top of this file recorded as a limitation rather than a bug:
    # the shell picks which output a window opens on and nothing on the wire
    # overrode it. So `--output DP-1` pinned the mode everywhere, told sway
    # where to put its client, and let Viewport put its own wherever the shell
    # had started. On a two-monitor desk that is a coin toss, and it is why a
    # run could come back measuring the other screen.
    #
    # shell.command is what makes it possible to say. Done for every run and
    # not only the two-monitor ones, because a single-output comparison in
    # which the two compositors used different monitors was never comparing
    # what it said it was.
    if [ "$comp_kind" = viewport ] && [ -n "$output" ]; then
        place_on "$output"
    fi

    # Warm up: first-frame costs are shader compilation and buffer allocation,
    # which are real but are not what a steady-state number is measuring.
    env WAYLAND_DISPLAY="$comp_display" "$vkcube" --wsi wayland \
        --c 2000 --present_mode 0 >/dev/null 2>&1 || true

    # Only around the probe: a connection held across a timed pass would be
    # measuring the spin rather than the compositor.
    [ "$fullscreen" = 1 ] || start_view_listener record
    comp_geometry=$(client_geometry)
    comp_geometry=${comp_geometry:-unknown}
    [ "$fullscreen" = 1 ] || stop_view_listener
    local idle
    idle=$(measure_idle 10)
    idle_comp_pct=${idle% *}
    idle_sess_pct=${idle#* }
    echo "   client $comp_geometry, idle cpu ${idle_comp_pct}% (session ${idle_sess_pct}%)" >&2
    {
        echo "$which"
        if [ "$which" = viewport ]; then
            echo "  binary       $viewport_bin ($(date -r "$viewport_bin" '+%Y-%m-%d %H:%M'))"
        elif [ "$which" = niri ]; then
            echo "  binary       $("$niri_bin" --version)"
        else
            echo "  binary       $(sway --version)"
        fi
        echo "  display      $comp_display"
        echo "  client size  $comp_geometry"
        echo "  client output $(client_output)"
        echo "  fullscreen   $([ "$fullscreen" = 1 ] && echo yes || echo no)"
        echo "  mode asked   ${mode:-whatever the output preferred}"
        echo "  mode got     $(comp_mode)"
        echo "  idle cpu     ${idle_comp_pct}% compositor, ${idle_sess_pct}% session"
        if [ "$which" = viewport ] &&
            sed 's/\x1b\[[0-9;]*m//g' "$comp_log" | grep -q 'did not place'; then
            echo "  LAYOUT       the shell did not place the window; the compositor's"
            echo "               built-in fallback did. This is not the desktop's layout."
        fi
    } >>"$outdir/environment.txt"

    local run entry name mode n low high
    for (( run = 1; run <= runs; run++ )); do
        for entry in "${scenarios[@]}"; do
            IFS=: read -r name mode n low high <<<"$entry"
            echo "   run $run  $name" >&2
            measure "$name" "$mode" "$n" "$(scale_count "$low")" "$(scale_count "$high")" "$run" |
                tee -a "$results" >&2
        done
    done

    # The other monitor, if there is one to use.
    if [ -n "$second" ]; then
        local mode_a mode_b count_b count_a fields wall_a wall_b ticks tree gpu
        for (( run = 1; run <= runs; run++ )); do
            for entry in "${mm_scenarios[@]}"; do
                IFS=: read -r name mode_a mode_b count_b <<<"$entry"
                count_b=$(scale_count "$count_b")
                # Enough to outlast the measured client rather than a fixed
                # number: in IMMEDIATE the busy one runs two orders of
                # magnitude faster, so the same count would have it finish
                # almost at once and leave most of the other's run uncontended
                # — which is the measurement quietly not happening.
                if [ "$mode_a" = 0 ]; then
                    count_a=$(( count_b * 200 ))
                else
                    count_a=$(( count_b * 3 ))
                fi
                echo "   run $run  $name" >&2
                fields=$(one_pass_mm "$mode_a" "$count_a" "$mode_b" "$count_b")
                read -r wall_a wall_b ticks tree gpu <<<"$fields"
                awk -v comp="$comp_kind" -v scenario="$name" -v run="$run" \
                    -v out_a="$output" -v out_b="$second" \
                    -v wall_a="$wall_a" -v wall_b="$wall_b" \
                    -v count_b="$count_b" -v ticks="$ticks" -v tree="$tree" \
                    -v gpu="$gpu" -v clock="$clock" \
                    'BEGIN {
                        span = (wall_a > wall_b) ? wall_a : wall_b
                        cpu = span > 0 ? ticks * 100 / clock / span : 0
                        sess = span > 0 ? tree * 100 / clock / span : 0
                        # The busy client is not given a frame rate. It is
                        # there to hold its screen at full rate and is killed
                        # by its own frame count, not by anything meaningful.
                        printf "%s\t%s\t%d\t%s\tbusy\t\t%.3f\t%.1f\t%.1f\t%.1f\n",
                            comp, scenario, run, out_a, wall_a, cpu, sess, gpu
                        fps = (wall_b > 0) ? count_b / wall_b : 0
                        printf "%s\t%s\t%d\t%s\tmeasured\t%.1f\t%.3f\t%.1f\t%.1f\t%.1f\n",
                            comp, scenario, run, out_b, fps,
                            wall_b, cpu, sess, gpu
                    }' | tee -a "$mm_results" >&2
            done
        done
    fi

    stop_compositor
}

case "$only" in
    both) bench_one viewport; bench_one sway ;;
    all) bench_one viewport; bench_one sway; bench_one niri ;;
    *) bench_one "$only" ;;
esac
rm -f "$outdir/.gpu-samples" "$outdir/.geometry"

# --------------------------------------------------------------------------
# The summary. Median rather than mean: one scheduler hiccup in a five-second
# run moves a mean by more than the difference being looked for.
# --------------------------------------------------------------------------
summary=$outdir/summary.md
python3 - "$results" "$summary" <<'PY'
import statistics
import sys
from collections import defaultdict

raw, out = sys.argv[1], sys.argv[2]
rows = defaultdict(list)
order = []
with open(raw) as f:
    header = f.readline().rstrip("\n").split("\t")
    for line in f:
        cells = line.rstrip("\n").split("\t")
        if len(cells) != len(header):
            continue
        row = dict(zip(header, cells))
        key = (row["scenario"], row["compositor"])
        if key not in rows:
            order.append(key)
        rows[key].append(row)

numeric = [
    "fps",
    "cpu_ms_frame",
    "net_ms_frame",
    "comp_cpu_pct",
    "sess_cpu_pct",
    "gpu_pct",
    "rss_mb",
]


def median(key, field):
    return statistics.median(float(r[field]) for r in rows[key])


scenarios = []
for scenario, compositor in order:
    if scenario not in scenarios:
        scenarios.append(scenario)

# Whichever compositors this run actually measured, Viewport first because it
# is the subject and the others are what it is being held against.
compositors = [c for c in ("viewport", "sway", "niri")
               if any(k[1] == c for k in rows)]
others = [c for c in compositors if c != "viewport"]

# A run that did not measure Viewport is not Viewport against anything, and
# saying so would put its name on a table it has no row in.
if "viewport" in compositors:
    title = "Viewport against {}".format(
        " and ".join(others)) if others else "Viewport"
else:
    title = " and ".join(compositors) if compositors else "nothing"
lines = ["# vkcube: {}".format(title), ""]
lines.append(
    "| scenario | compositor | fps | cpu ms/frame | net ms/frame | comp cpu % "
    "| session cpu % | gpu % | rss MB | client |"
)
lines.append("| --- " * 10 + "|")
for scenario in scenarios:
    for compositor in compositors:
        key = (scenario, compositor)
        if key not in rows:
            continue
        size = rows[key][0]["client_size"]
        lines.append(
            "| {} | {} | {:.1f} | {:.3f} | {:.3f} | {:.1f} | {:.1f} | {:.1f} | {:.0f} | {} |".format(
                scenario, compositor, *(median(key, f) for f in numeric), size
            )
        )

# Nothing to compare against when only one compositor ran, or when the one
# everything is expressed relative to is not among them.
if "viewport" in compositors and others:
    lines += [
        "",
        "## Ratios",
        "",
        "Viewport over each of the others. Above 1.00 means more of whatever the",
        "column counts, which is better for fps and worse for everything else.",
    ]
for other in (others if "viewport" in compositors else []):
    lines += [
        "",
        "**against {}**".format(other),
        "",
        "| scenario | fps | cpu ms/frame | net ms/frame | session cpu % |",
        "| --- " * 5 + "|",
    ]
    for scenario in scenarios:
        a, b = (scenario, "viewport"), (scenario, other)
        if a not in rows or b not in rows:
            continue
        cells = []
        for field in ("fps", "cpu_ms_frame", "net_ms_frame", "sess_cpu_pct"):
            base = median(b, field)
            cells.append("{:.2f}x".format(median(a, field) / base) if base else "n/a")
        lines.append("| {} | {} |".format(scenario, " | ".join(cells)))

lines += [
    "",
    "## Reading this",
    "",
    "- `fps` is client frames, and only in the FIFO rows is every one of them",
    "  a frame that reached the screen. IMMEDIATE and MAILBOX discard most of",
    "  what they draw; their fps says how fast the compositor returns buffers,",
    "  not how fast it presents.",
    "- `cpu ms/frame` includes whatever the compositor burns while idle,",
    "  divided over the frames the client asked for. `net ms/frame` takes the",
    "  measured idle cost back out, and is the marginal price of one more",
    "  frame. Where the two are far apart, the standing cost dominates and the",
    "  idle figure in environment.txt is the number that matters.",
    "- Nested runs are paced by the host compositor, and the two nested",
    "  backends do not pace alike, so the FIFO frame rates compare the",
    "  backends rather than the compositing. CPU, memory and GPU occupancy",
    "  survive nesting; frame rate does not. Run with --drm from a TTY for a",
    "  presented frame rate that means something.",
]

with open(out, "w") as f:
    f.write("\n".join(lines) + "\n")
print("\n".join(lines))
PY

# --------------------------------------------------------------------------
# The multi-monitor table, appended to the same summary.
#
# Its own section rather than more rows in the one above, because the columns
# do not mean the same thing: there, fps is every client on one screen added
# together, and here it is one client on one named screen while another screen
# is deliberately busy.
# --------------------------------------------------------------------------
if [ -n "$second" ]; then
    python3 - "$mm_results" "$summary" "$output" "$second" <<'PY'
import statistics
import sys
from collections import defaultdict

raw, out, primary, secondary = sys.argv[1:5]
rows = defaultdict(list)
order = []
with open(raw) as f:
    header = next(f).rstrip("\n").split("\t")
    for line in f:
        row = dict(zip(header, line.rstrip("\n").split("\t")))
        if row.get("role") != "measured":
            continue
        key = (row["scenario"], row["compositor"])
        if key not in rows:
            order.append(key)
        rows[key].append(row)

if not order:
    sys.exit(0)


def median(key, field):
    values = [float(r[field]) for r in rows[key] if r.get(field)]
    return statistics.median(values) if values else 0.0


lines = [
    "",
    "## Two monitors",
    "",
    "One client on `{}` held at full rate, and the measured client on `{}`."
    .format(primary, secondary),
    "",
    "| scenario | compositor | fps on {} | comp cpu % | session cpu % | gpu % |"
    .format(secondary),
    "| --- " * 6 + "|",
]

scenarios = []
for scenario, _ in order:
    if scenario not in scenarios:
        scenarios.append(scenario)

mm_compositors = [c for c in ("viewport", "sway", "niri")
                  if any(k[1] == c for k in rows)]
mm_others = [c for c in mm_compositors if c != "viewport"]

for scenario in scenarios:
    for compositor in mm_compositors:
        key = (scenario, compositor)
        if key not in rows:
            continue
        lines.append("| {} | {} | {:.1f} | {:.1f} | {:.1f} | {:.1f} |".format(
            scenario, compositor,
            median(key, "fps"), median(key, "comp_cpu_pct"),
            median(key, "sess_cpu_pct"), median(key, "gpu_pct")))

for other in mm_others:
    both = [s for s in scenarios
            if (s, "viewport") in rows and (s, other) in rows]
    if not both:
        continue
    lines += [
        "",
        "### Ratios against {}".format(other),
        "",
        "Viewport over {}, on `{}`. Above 1.00 is more frames reaching the"
        .format(other, secondary),
        "screen that was not the busy one, which is better.",
        "",
        "| scenario | fps | comp cpu % |",
        "| --- " * 3 + "|",
    ]
    for scenario in both:
        a, b = (scenario, "viewport"), (scenario, other)
        cells = []
        for field in ("fps", "comp_cpu_pct"):
            denominator = median(b, field)
            cells.append("{:.2f}x".format(median(a, field) / denominator)
                         if denominator else "-")
        lines.append("| {} | {} |".format(scenario, " | ".join(cells)))

lines += [
    "",
    "### Reading this",
    "",
    "- The number to look at is `fps on {}`, and what to hold it against is".format(secondary),
    "  that monitor's own refresh rate in environment.txt — not the other",
    "  monitor's. Two panels at different rates should produce two different",
    "  frame rates, and a compositor pacing off the device rather than off",
    "  each screen cannot produce both.",
    "- `mm-stress` is the one that matters. A client that never idles on the",
    "  other screen is the condition under which a screen stops being paced at",
    "  all: the barrier tick asked the device whether anything had flipped",
    "  lately, so one screen running flat out silenced it for the other, and a",
    "  terminal there fell to single figures. If this row is far below that",
    "  monitor's refresh rate, that fault is back.",
    "- The busy client gets no frame rate. It is there to hold its screen at",
    "  full rate and stops on its own frame count, which measures nothing.",
    "- CPU here is the whole compositor across both screens, not per output.",
    "  There is no per-output attribution to be had: it is one process, and one",
    "  render loop serving both.",
]

with open(out, "a") as f:
    f.write("\n".join(lines) + "\n")
print("\n".join(lines))
PY
fi

echo >&2
echo "raw:     $results" >&2
[ -n "$second" ] && echo "raw (mm): $mm_results" >&2
echo "summary: $summary" >&2
