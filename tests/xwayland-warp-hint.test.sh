#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# A capture end puts the cursor back exactly where the game warped it.
#
# CS2's mouse look under Xwayland is the warp fallback: hidden cursor, a
# grab, and an XWarpPointer back to the centre every frame. Xwayland turns
# that into a pointer lock carrying `set_cursor_position_hint`, and the game
# computes its deltas against the point it last warped to — so when the
# capture ends and the compositor restores the cursor from the hint, a
# cursor that lands anywhere but the warp target is read by the game as
# camera motion on the next frame. Slightly right and downwards is what a
# hint applied to the window's *content* origin looks like when the surface
# carries CSD margins (an X11 window with `_GTK_FRAME_EXTENTS`): the hint is
# surface-local, the content origin is `extents` pixels inside it.
#
# The client plays the game's half (see tests/xwayland-warp-hint-client.c);
# this drives the compositor's half and checks where the cursor comes to
# rest. `noextents` is the control: no margins, every origin agrees, and a
# compositor that shifts even that has a different bug.
#
#   tests/xwayland-warp-hint.test.sh build/viewport
set -u

viewport=${1:-build/viewport}

if [ ! -x "$viewport" ]; then
	echo "missing $viewport — build first" >&2
	exit 2
fi

if ! pkg-config --exists x11; then
	echo "SKIP: no libX11 to build the X client against"
	exit 77
fi

if ! command -v Xwayland >/dev/null 2>&1; then
	echo "SKIP: no Xwayland for the compositor to spawn"
	exit 77
fi

root=$(cd "$(dirname "$0")/.." && pwd)
workdir=$(mktemp -d)
viewport_pid=
client_pid=

cleanup() {
	for pid in $client_pid $viewport_pid; do
		[ -n "$pid" ] && kill "$pid" 2>/dev/null
	done
	wait 2>/dev/null
	rm -rf "$workdir"
}
trap cleanup EXIT

client=$workdir/warp-hint-client
# shellcheck disable=SC2046 # pkg-config output is a word list on purpose
if ! cc "$root/tests/xwayland-warp-hint-client.c" -o "$client" $(pkg-config --cflags --libs x11 xfixes); then
	echo "SKIP: the X client would not compile"
	exit 77
fi

# A private runtime dir: this starts its own compositor and must not join,
# or be joined to, the one the developer is sitting in — and its private
# state dir, so it does not restore the developer's saved session layout.
export XDG_RUNTIME_DIR="$workdir/run"
export XDG_STATE_HOME="$workdir/state"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME"
unset WAYLAND_DISPLAY

log=$workdir/viewport.log
VIEWPORT_POINTER_DEBUG=1 "$viewport" --headless --width 1920 --height 1080 >"$log" 2>&1 &
viewport_pid=$!

display=
sock=
for _ in $(seq 1 100); do
	if ! kill -0 "$viewport_pid" 2>/dev/null; then
		echo "the compositor exited before Xwayland was up" >&2
		cat "$log" >&2
		exit 1
	fi
	display=$(sed -n 's/.*Xwayland ready on \(:[0-9]*\).*/\1/p' "$log" | head -1)
	sock=$(find "$XDG_RUNTIME_DIR" -name '*.sock' 2>/dev/null | head -1)
	if [ -n "$display" ] && [ -n "$sock" ]; then
		break
	fi
	sleep 0.2
done

if [ -z "$display" ]; then
	echo "Xwayland never came up" >&2
	cat "$log" >&2
	exit 1
fi

msg() {
	"$viewport" msg --socket "$sock" "$@" >/dev/null 2>&1
}

# Only lines after `$mark`, a phase's own cut of the log.
log_since() {
	tail -n +"$((mark + 1))" "$log" | sed 's/\x1b\[[0-9;]*m//g' | grep "$@"
}

# One command to the client, one `ok` back.
fifo=$workdir/client.fifo
mkfifo "$fifo"
client_out=$workdir/client.log

run_phase() {
	local phase=$1
	local extra=()
	if [ "$phase" = "noextents" ]; then
		extra=(noextents)
	fi

	# Everything the compositor says from here belongs to this phase.
	mark=$(wc -l <"$log")

	: >"$client_out"
	DISPLAY=$display "$client" "${extra[@]}" >"$client_out" 2>&1 <"$fifo" &
	client_pid=$!
	exec 3>"$fifo"

	# The client says where its window landed; the cursor has to be over it
	# before a constraint asked for is a constraint activated.
	local wx wy
	for _ in $(seq 1 50); do
		read -r wx wy < <(sed -n 's/^window: \([0-9-]*\) \([0-9-]*\) .*/\1 \2/p' "$client_out")
		[ -n "${wx:-}" ] && break
		sleep 0.1
	done
	if [ -z "${wx:-}" ]; then
		echo "FAIL($phase): the client never mapped its window" >&2
		cat "$client_out" >&2
		return 1
	fi

	# The view id is not the map order nor anything the client can know —
	# find it as the only X11 window here, whatever the shell has resized
	# it to since.
	local vid
	vid=$("$viewport" msg --socket "$sock" -t view.query 2>/dev/null |
		grep '"xwayland":true' |
		sed -n 's/.*"id":\([0-9]*\).*/\1/p' | tail -1)
	if [ -z "${vid:-}" ]; then
		echo "FAIL($phase): the client's window has no view" >&2
		"$viewport" msg --socket "$sock" -t view.query 2>&1 | head -5 >&2
		cat "$client_out" >&2
		return 1
	fi

	# Placed first, so "the window is under the cursor" is a fact this
	# test decides rather than the shell's layout policy. The cursor must
	# be over the window or the constraint the grab asks for is not one
	# the compositor activates.
	msg -t view.layout --id "$vid" --x "$wx" --y "$wy" --width 800 --height 600
	sleep 0.3
	msg -t input.pointer --x $((wx + 400)) --y $((wy + 300))
	sleep 0.3

	# Capture, then the game's first recenter. The recenter is not decoration
	# here: Xwayland materializes its pointer lock when the warp hits the
	# hidden cursor, and only then does the compositor see the constraint
	# and its cursor position hint. The log says so, unconditionally, on
	# every constraint. The one repeat is for the window-to-surface
	# association losing a race with the grab.
	echo "capture" >&3
	echo "warp 400 300" >&3
	local locked= attempt=0
	for _ in $(seq 1 12); do
		sleep 0.5
		if log_since -q "a client asked for a"; then
			locked=1
			break
		fi
		attempt=$((attempt + 1))
		if [ "$attempt" = 4 ]; then
			echo "capture" >&3
			echo "warp 400 300" >&3
		fi
	done
	if [ -z "$locked" ]; then
		echo "FAIL($phase): the grab never became a pointer lock" >&2
		sed 's/\x1b\[[0-9;]*m//g' "$log" | tail -30 >&2
		exec 3>&-
		wait "$client_pid" 2>/dev/null
		client_pid=
		return 1
	fi

	# The capture ends: the compositor restores the cursor from the hint.
	echo "release" >&3
	sleep 0.3
	echo "query" >&3
	sleep 0.1

	local got
	got=$(sed -n 's/^query: \([0-9-]*\) \([0-9-]*\)$/\1 \2/p' "$client_out" | tail -1)
	echo "quit" >&3
	exec 3>&-
	wait "$client_pid" 2>/dev/null
	client_pid=

	if [ "$got" = "400 300" ]; then
		echo "PASS($phase): the cursor came back to the warp target ($got)"
		return 0
	fi
	echo "FAIL($phase): the warp target (400, 300) came back as ($got)" >&2
	echo "      A game reads that difference as camera motion — the aim drifts." >&2
	echo "      The hint is surface-local; the cursor must return to the" >&2
	echo "      surface origin plus the hint, not the window's content." >&2
	sed -n 's/^event: /      /p' "$client_out" | tail -8 >&2
	return 1
}

status=0
run_phase noextents || status=1
run_phase extents || status=1

if [ "$status" -eq 0 ]; then
	echo "PASS: xwayland-warp-hint"
else
	sed 's/\x1b\[[0-9;]*m//g' "$log" | grep -E "pointer:|hint" | tail -20 >&2
fi
exit "$status"