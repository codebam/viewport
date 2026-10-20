#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# A shell change never moves a game's camera.
#
# The cursor is captured by a game (hidden cursor, a grab, Xwayland's pointer
# lock behind it), and then the shell does what pressing Mod4 does: it reveals
# the bar, opens a menu over the pointer and closes it again. None of that
# touches the mouse — but each of those is a `shell.overlay` change, and the
# compositor re-hit-tests the pointer when the shell's stack changes, then
# sends the result as a motion to the surface under it. For a client holding a
# lock that motion is an absolute position in its own coordinates, and a game
# reads every absolute position as aim: a keystroke fired the camera.
#
# The client plays the game's half (see tests/xwayland-warp-hint-client.c). It
# counts the motions that land where it warped to; anything at all arriving
# while it holds the lock and nothing else is happening is the compositor
# inventing input. `noextents` is the control — a window whose surface and
# content agree, where the invented motion would at least have been accurate,
# and where the count must be zero for the same reason.
#
#   tests/xwayland-capture-overlay.test.sh build/viewport
set -u

viewport=${1:-build/viewport}

if [ ! -x "$viewport" ]; then
	echo "missing $viewport — build first" >&2
	exit 2
fi

if ! pkg-config --exists x11 xfixes; then
	echo "SKIP: no libX11/libXfixes to build the X client against"
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

fifo=$workdir/client.fifo
mkfifo "$fifo"
client_out=$workdir/client.log

# One command to the client, one `ok` back.
say() {
	echo "$1" >&3
	sleep 0.2
}

# The motions that landed on the game's recenter target since the last ask.
motions_at_target() {
	say motioncount
	sed -n 's/^motioncount: \([0-9]*\)$/\1/p' "$client_out" | tail -1
}

run_phase() {
	local phase=$1 passthrough=$2
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

	# The game's own recenter is the last thing that may arrive. Spend it, so
	# what is counted below is only what the compositor invents.
	echo "warp 400 300" >&3
	sleep 0.3
	motions_at_target >/dev/null

	local fail=0

	# Pressing Mod4 under `bar: auto` reveals the bar. The bar is a
	# passthrough rectangle: it draws in front and takes no pointer, so a
	# lock over it stays the lock it was.
	msg -t shell.overlay --rects \
		'[{"x":0,"y":0,"width":1920,"height":40,"passthrough":true,"blur":false,"keep_focus":false}]'
	sleep 0.4
	local revealed
	revealed=$(motions_at_target)
	if [ "${revealed:-0}" != "0" ]; then
		echo "FAIL($phase): revealing the bar moved the camera ${revealed} time(s)" >&2
		echo "      The bar is a shell overlay and revealing it is a" >&2
		echo "      \`shell.overlay\` change with no pointer in it. A locked" >&2
		echo "      client must be sent no motion for it: a game reads an" >&2
		echo "      absolute position as aim, so a keystroke fired a shot." >&2
		fail=1
	fi

	# A launcher or a menu: a rectangle that does take the pointer, opened
	# over where the captured pointer sits. Still no mouse moved.
	msg -t shell.overlay --rects \
		'[{"x":0,"y":0,"width":1920,"height":40,"passthrough":true,"blur":false,"keep_focus":false},
		  {"x":300,"y":200,"width":900,"height":700,"passthrough":false,"blur":false,"keep_focus":false}]'
	sleep 0.4
	local menu
	menu=$(motions_at_target)
	if [ "${menu:-0}" != "0" ]; then
		echo "FAIL($phase): a menu over the pointer moved the camera ${menu} time(s)" >&2
		echo "      Nothing moved the mouse. See the note above." >&2
		fail=1
	fi

	# And the click that opens it, and the one that closes it: a press is the
	# other thing that re-hit-tests the pointer before forwarding it.
	msg -t input.button --button 272 --pressed true
	sleep 0.2
	msg -t input.button --button 272 --pressed false
	sleep 0.4
	local pressed
	pressed=$(motions_at_target)
	if [ "${pressed:-0}" != "0" ]; then
		echo "FAIL($phase): a click moved the camera ${pressed} time(s)" >&2
		echo "      A press re-runs the hit test and forwards a button," >&2
		echo "      never a position: the lock owns where the cursor is." >&2
		fail=1
	fi

	# Closing it puts the window back under the pointer; that is a leave and
	# enter for a free pointer, and still silence for a locked one.
	msg -t shell.overlay --rects \
		"[{\"x\":0,\"y\":0,\"width\":1920,\"height\":40,\"passthrough\":${passthrough},\"blur\":false,\"keep_focus\":false}]"
	sleep 0.4
	msg -t shell.overlay --rects '[]'
	sleep 0.4
	local closed
	closed=$(motions_at_target)
	if [ "${closed:-0}" != "0" ]; then
		echo "FAIL($phase): closing the shell's overlays moved the camera ${closed} time(s)" >&2
		fail=1
	fi

	# The lock is still the game's, and still anchored where the game put it.
	local got
	say query
	got=$(sed -n 's/^query: \([0-9-]*\) \([0-9-]*\)$/\1 \2/p' "$client_out" | tail -1)

	echo "quit" >&3
	exec 3>&-
	wait "$client_pid" 2>/dev/null
	client_pid=

	if [ "$got" != "400 300" ]; then
		echo "FAIL($phase): the capture's anchor came back as ($got), not (400, 300)" >&2
		echo "      The shell's changes moved the cursor itself, not only" >&2
		echo "      its events. The game's next recenter reads that as a jump." >&2
		fail=1
	fi

	if [ "$fail" -eq 0 ]; then
		echo "PASS($phase): no shell change reached the captured game"
		return 0
	fi
	echo "  --- every motion the game saw ---" >&2
	sed -n 's/^event: /      /p' "$client_out" | tail -10 >&2
	return 1
}

status=0
run_phase noextents true || status=1
run_phase extents false || status=1

if [ "$status" -eq 0 ]; then
	echo "PASS: xwayland-capture-overlay"
else
	sed 's/\x1b\[[0-9;]*m//g' "$log" | grep -E "pointer:" | tail -20 >&2
fi
exit "$status"
