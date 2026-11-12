#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# The shell- and session-shaped messages, end to end.
#
# Unit tests cover how each parses. This checks the compositor half: a
# `session.save` followed by a `session.query` must return the exact bytes
# that were saved (the state is opaque, and the shell's layout format lives
# inside it), `shell.focus` must land as a `view.focused` of 0, and the lock
# messages must behave as the built-in lock screen promises — a stale
# generation is dropped (never checked with PAM), while the current one is
# heard. A lock screen that answered to any generation would be a lock that
# a queued keystroke from the previous lock could stop, which is the defect
# the generation exists to prevent.
#
# `shell.overlay` is deliberately not repeated here: it is the same
# set_shell_overlays path as `screencast.rect` (the older single-rectangle
# form checked below), and the hit-test half is xwayland-capture-overlay's.
#
# `shell.overview` and `screencast.rect` and `gesture.capture` have no direct
# socket reply and no state any other message reports: the overview routes
# input to the shell, the rect takes the pointer, and a capture only takes
# effect at the next `gesture.begin` (which needs a real touchpad). The
# observable for those is acceptance (a clean barrier) and that the compositor
# is still answering afterwards, which is what is asserted.
#
#   tests/shell-session-ipc.test.sh target/debug/viewport path/to/paint-client
set -uo pipefail

VIEWPORT=${1:?usage: shell-session-ipc.test.sh VIEWPORT PAINT_CLIENT}
PAINT_CLIENT=${2:?}

for binary in "$VIEWPORT" "$PAINT_CLIENT"; do
	if [ ! -x "$binary" ]; then
		echo "missing $binary — build first (scripts/integration.sh builds the clients)" >&2
		exit 2
	fi
done
VIEWPORT=$(realpath "$VIEWPORT")
PAINT_CLIENT=$(realpath "$PAINT_CLIENT")

WORK=$(mktemp -d)
LOG="$WORK/viewport.log"
COMPOSITOR_PID=
PAINT_PID=

cleanup() {
	[ -n "$PAINT_PID" ] && kill "$PAINT_PID" 2>/dev/null
	if [ -n "$COMPOSITOR_PID" ] && kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
		kill -TERM "$COMPOSITOR_PID" 2>/dev/null
		for _ in $(seq 20); do
			kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
			sleep 0.1
		done
		kill -KILL "$COMPOSITOR_PID" 2>/dev/null
	fi
	rm -rf "$WORK"
}
trap cleanup EXIT

failures=0
check() {
	local what=$1 expected=$2 got=$3
	if [ "$expected" = "$got" ]; then
		echo "ok: $what"
	else
		echo "FAIL: $what — wanted $expected, got $got" >&2
		failures=$((failures + 1))
	fi
}

unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="$WORK/run"
export XDG_STATE_HOME="$WORK/state"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME"

# Built-in lock: no lock_command, so `session.lock` is the shell's screen.
printf '{ "layout": "tiling", "tray": false, "idle": {} }\n' >"$WORK/config.json"

export VIEWPORT_LOG="${VIEWPORT_LOG:-viewport=debug}"

"$VIEWPORT" --headless --config "$WORK/config.json" >"$LOG" 2>&1 &
COMPOSITOR_PID=$!

SOCKET=
WAYLAND_DISPLAY=
for _ in $(seq 100); do
	SOCKET=$(sed -n 's/.*control socket at \(.*\)$/\1/p' "$LOG" | head -1)
	WAYLAND_DISPLAY=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$LOG" | head -1 | cut -d= -f2)
	[ -n "$SOCKET" ] && [ -n "$WAYLAND_DISPLAY" ] && break
	kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$SOCKET" ] || [ -z "$WAYLAND_DISPLAY" ]; then
	echo "FAIL the compositor never published its sockets" >&2
	tail -20 "$LOG" >&2
	exit 2
fi
export WAYLAND_DISPLAY
echo "ok   the compositor is up on $WAYLAND_DISPLAY, socket at $SOCKET"

msg() {
	"$VIEWPORT" msg --socket "$SOCKET" "$@"
}

wait_for() {
	local file=$1 needle=$2
	for _ in $(seq 1 "${3:-50}"); do
		if grep -q -- "$needle" "$file" 2>/dev/null; then
			return 0
		fi
		kill -0 "$COMPOSITOR_PID" 2>/dev/null || return 1
		sleep 0.1
	done
	return 1
}

"$PAINT_CLIENT" shell-session-ipc 320 240 24 ffff0000 ff0000ff \
	>"$WORK/paint.log" 2>&1 &
PAINT_PID=$!

for _ in $(seq 1 100); do
	if grep -qE 'view [0-9]+: ' "$LOG" 2>/dev/null; then
		break
	fi
	kill -0 "$PAINT_PID" 2>/dev/null || break
	sleep 0.1
done
VID=$(msg -t view.query 2>/dev/null | grep '"app_id":"shell-session-ipc"' |
	sed -n 's/.*"id":\([0-9]*\).*/\1/p' | head -1)
if [ -z "$VID" ]; then
	echo "FAIL the paint window never mapped" >&2
	tail -20 "$LOG" >&2
	exit 2
fi
echo "ok   the paint window is view $VID"

# One subscriber for the focus and lock events.
SUB_OUT="$WORK/subscribe.jsonl"
"$VIEWPORT" msg --socket "$SOCKET" --timeout 0 -t subscribe \
	view.focused session.lock session.lock.error session.unlock \
	screencast.active \
	>"$SUB_OUT" 2>"$WORK/subscribe.err" &
SUB_PID=$!
sleep 0.5

# `view.query` replays screencast.active (false with no streams).
msg -t view.query >/dev/null 2>&1
sleep 0.5
check "view.query replays screencast.active false with no streams" yes \
	"$(grep '"type":"screencast.active"' "$SUB_OUT" | tail -1 | grep -q '"active":false' && echo yes || echo no)"

# --- session.save / session.query -----------------------------------------
if msg -t session.save --state 'probe-layout-42' >/dev/null 2>&1; then
	echo "ok: session.save was accepted"
else
	echo "FAIL: session.save was refused" >&2
	failures=$((failures + 1))
fi
RESTORED=$(msg -t session.query 2>/dev/null | tail -1)
check "session.query answers with a session.restore" yes \
	"$(printf '%s' "$RESTORED" | grep -q '"type":"session.restore"' && echo yes || echo no)"
check "and it carries the exact bytes that were saved" yes \
	"$(printf '%s' "$RESTORED" | grep -q '"state":"probe-layout-42"' && echo yes || echo no)"
# A second save replaces the first whole (the state is one opaque blob).
if msg -t session.save --state 'second-blob' >/dev/null 2>&1; then
	echo "ok: a second session.save was accepted"
else
	echo "FAIL: a second session.save was refused" >&2
	failures=$((failures + 1))
fi
RESTORED=$(msg -t session.query 2>/dev/null | tail -1)
check "and the query returns the newest blob" yes \
	"$(printf '%s' "$RESTORED" | grep -q '"state":"second-blob"' && echo yes || echo no)"

# --- shell.focus -----------------------------------------------------------
# The paint window must hold the focus first, or the fallback is vacuous.
if msg -t view.focus --id "$VID" >/dev/null 2>&1; then
	echo "ok: view.focus on the paint window was accepted (setup)"
else
	echo "FAIL: view.focus was refused" >&2
	failures=$((failures + 1))
fi
sleep 0.5
if msg -t shell.focus >/dev/null 2>&1; then
	echo "ok: shell.focus was accepted"
else
	echo "FAIL: shell.focus was refused" >&2
	failures=$((failures + 1))
fi
SEEN0=no
for _ in $(seq 1 50); do
	if grep '"type":"view.focused"' "$SUB_OUT" 2>/dev/null | grep -q '"id":0'; then
		SEEN0=yes
		break
	fi
	sleep 0.1
done
if [ "$SEEN0" = yes ]; then
	echo "ok: and it landed as a view.focused of 0 (the shell)"
else
	echo "FAIL: no view.focused of 0 after shell.focus" >&2
	failures=$((failures + 1))
fi

# --- shell.overview --------------------------------------------------------
# No event names the overview; what is observable is acceptance and that the
# desktop is still there afterwards (the overview clears every window's
# scale, so a later layout is still possible).
if msg -t shell.overview --active true >/dev/null 2>&1; then
	echo "ok: shell.overview true was accepted"
else
	echo "FAIL: shell.overview true was refused" >&2
	failures=$((failures + 1))
fi
if msg -t shell.overview --active false >/dev/null 2>&1; then
	echo "ok: shell.overview false was accepted"
else
	echo "FAIL: shell.overview false was refused" >&2
	failures=$((failures + 1))
fi
if msg -t view.query 2>/dev/null | grep -q "\"app_id\":\"shell-session-ipc\""; then
	echo "ok: and the window is still listed after the overview"
else
	echo "FAIL: the window vanished after the overview" >&2
	failures=$((failures + 1))
fi

# --- screencast.rect (the older single-rectangle overlay) -------------------
# `modifiers` has no headless check here: it is only emitted on the physical
# key path (under `bar: auto`), and the synthetic `input.key` path
# (`inject_key`) updates the seat's modifiers without sending the event, so
# there is no socket-driven way to produce one. See docs/ipc-coverage.md.
if msg -t screencast.rect --x 0 --y 0 --width 120 --height 60 >/dev/null 2>&1; then
	echo "ok: screencast.rect was accepted"
else
	echo "FAIL: screencast.rect was refused" >&2
	failures=$((failures + 1))
fi
# A zero size says nothing is above the windows any more (the way an empty
# shell.overlay list does).
if msg -t screencast.rect --x 0 --y 0 --width 0 --height 0 >/dev/null 2>&1; then
	echo "ok: a zero-size screencast.rect was accepted"
else
	echo "FAIL: a zero-size screencast.rect was refused" >&2
	failures=$((failures + 1))
fi

# --- gesture.capture --------------------------------------------------------
if msg --raw '{"type":"gesture.capture","gestures":[{"kind":"swipe","fingers":3},{"kind":"pinch","fingers":2}]}' >/dev/null 2>&1; then
	echo "ok: gesture.capture (via --raw) was accepted"
else
	echo "FAIL: gesture.capture (via --raw) was refused" >&2
	failures=$((failures + 1))
fi
if msg --raw '{"type":"gesture.capture","gestures":[]}' >/dev/null 2>&1; then
	echo "ok: an empty gesture.capture releases everything, and was accepted"
else
	echo "FAIL: an empty gesture.capture was refused" >&2
	failures=$((failures + 1))
fi
check "gesture.capture is offerable with -t" "0" \
	"$(msg -t gesture.capture >/dev/null 2>&1; echo "$?")"
# The typed form carries the same list over --gestures, as JSON.
check "gesture.capture carries gestures with -t" "0" \
	"$(msg -t gesture.capture --gestures '[{"kind":"swipe","fingers":3}]' >/dev/null 2>&1; echo "$?")"
if msg -t output.query >/dev/null 2>&1; then
	echo "ok: and the compositor is still answering after gesture.capture"
else
	echo "FAIL: the compositor stopped answering" >&2
	failures=$((failures + 1))
fi

# --- error paths: session/shell/screencast/gesture --------------------------
err() {
	msg "$@" >/dev/null 2>"$WORK/err.txt"
	local code=$?
	echo "$code $(cat "$WORK/err.txt" | tr '\n' ' ')"
}
check "session.save without a state is a usage error" yes \
	"$(err -t session.save | grep -q "^2 .*missing field" && echo yes || echo no)"
check "session.query with a field is a usage error" yes \
	"$(err -t session.query --bogus 1 | grep -q "^2 .*takes no fields" && echo yes || echo no)"
check "shell.focus with a field is a usage error" yes \
	"$(err -t shell.focus --bogus 1 | grep -q "^2 .*takes no fields" && echo yes || echo no)"
check "session.unlock without a password is a usage error" yes \
	"$(err -t session.unlock --generation 1 | grep -q "^2 .*missing field" && echo yes || echo no)"
check "gesture.capture with a bad kind is a usage error" yes \
	"$(msg --raw '{"type":"gesture.capture","gestures":[{"kind":"hover","fingers":3}]}' >/dev/null 2>&1; [ $? -eq 2 ] && echo yes || echo no)"

# --- the lock, with stale generations dropped -------------------------------
# Last: locking changes what every later message means, so everything above
# runs unlocked. The compositor is then left locked and killed by cleanup.
if ! msg -t session.lock >/dev/null 2>&1; then
	echo "FAIL: session.lock was refused" >&2
	exit 1
fi
# The event names this lock; a shell that crashed and restarted must draw
# again and say so again before anything of its buffer reaches the screen.
if wait_for "$SUB_OUT" '"type":"session.lock"}' 50; then
	echo "ok: locking arrived as a session.lock on the socket"
else
	echo "FAIL: no session.lock event after session.lock" >&2
	failures=$((failures + 1))
fi
LOCK=$(grep '"type":"session.lock"}' "$SUB_OUT" | tail -1)
GEN=$(printf '%s' "$LOCK" | sed -n 's/.*"generation":\([0-9]*\).*/\1/p')
check "the session.lock names its generation" yes \
	"$([ -n "$GEN" ] && echo yes || echo no)"
check "and whether a password can be checked" yes \
	"$(printf '%s' "$LOCK" | grep -q '"can_authenticate":' && echo yes || echo no)"

# A drawn claim for a lock that is not this one is named in the log and
# dropped. It must not stand in for the current lock (no "has drawn" line
# for the current generation after this).
if [ -n "$GEN" ]; then
	WRONG=$((GEN + 999))
	if msg -t session.lock.drawn --generation "$WRONG" >/dev/null 2>&1; then
		echo "ok: a stale session.lock.drawn was accepted (and ignored)"
	else
		echo "FAIL: a stale session.lock.drawn was refused" >&2
		failures=$((failures + 1))
	fi
	if wait_for "$LOG" "the shell says it drew lock $WRONG" 50; then
		echo "ok: and the log names the stale generation it dropped"
	else
		echo "FAIL: the stale drawn was not named in the log" >&2
		failures=$((failures + 1))
	fi
	check "and it did not count as this lock being drawn" no \
		"$(grep -q "the shell has drawn lock $GEN" "$LOG" && echo yes || echo no)"

	# A password for the stale lock is dropped without reaching PAM.
	if msg -t session.unlock --generation "$WRONG" --password wrong-password >/dev/null 2>&1; then
		echo "ok: a stale session.unlock was accepted (and ignored)"
	else
		echo "FAIL: a stale session.unlock was refused" >&2
		failures=$((failures + 1))
	fi
	if wait_for "$LOG" "a password arrived for lock $WRONG" 50; then
		echo "ok: and the log names the stale attempt it dropped"
	else
		echo "FAIL: the stale password was not named in the log" >&2
		failures=$((failures + 1))
	fi
	check "and the session is still locked (no unlock event)" no \
		"$(grep -q "session unlocked" "$LOG" && echo yes || echo no)"

	# A wrong password for the current lock goes to PAM and comes back as a
	# session.lock.error, in PAM's own words, and the session stays locked.
	# The message text is PAM's, so the assertion is on the generation and
	# the presence of a message, not on the words.
	if msg -t session.unlock --generation "$GEN" --password not-the-password >/dev/null 2>&1; then
		echo "ok: a wrong-password session.unlock was accepted (sent to PAM)"
	else
		echo "FAIL: a wrong-password session.unlock was refused" >&2
		failures=$((failures + 1))
	fi
	PAM_ERR=no
	for _ in $(seq 1 100); do
		if grep '"type":"session.lock.error"' "$SUB_OUT" 2>/dev/null | grep -q "\"generation\":$GEN"; then
			PAM_ERR=yes
			break
		fi
		sleep 0.1
	done
	check "and it arrived as a session.lock.error for this lock" yes "$PAM_ERR"
	check "and the session is still locked after it" no \
		"$(grep -q "session unlocked" "$LOG" && echo yes || echo no)"

	# The current generation is heard: claiming it drawn is named as such.
	if msg -t session.lock.drawn --generation "$GEN" >/dev/null 2>&1; then
		echo "ok: the current session.lock.drawn was accepted"
	else
		echo "FAIL: the current session.lock.drawn was refused" >&2
		failures=$((failures + 1))
	fi
	if wait_for "$LOG" "the shell has drawn lock $GEN" 50; then
		echo "ok: and the log names the current lock as drawn"
	else
		echo "FAIL: the current drawn was not named in the log" >&2
		failures=$((failures + 1))
	fi
else
	echo "FAIL: there was no generation to test stale ones against" >&2
	failures=$((failures + 1))
fi

kill "$SUB_PID" 2>/dev/null
wait "$SUB_PID" 2>/dev/null
SUB_PID=

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed; the compositor's log:" >&2
	tail -30 "$LOG" >&2
	exit 1
fi
echo "all shell-session-ipc checks passed"
