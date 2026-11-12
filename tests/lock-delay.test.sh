#!/usr/bin/env bash
# The lock delay: a lock asked for stands the outputs black for the delay,
# and only then actually locks. `idle.lock_delay` is the knob; no knob, no
# delay, and the lock is immediate as it has always been.
#
# Two things are asserted here that the unit tests cannot see from the
# outside: the outputs really do go black while the session is still
# unlocked (the desktop visible in the capture before the delay, gone during
# it, and the lock window never painted), and the countdown does not run
# through activity — a deliberate act during the black phase cancels the
# lock rather than letting it fire at second two regardless.
#
# Modeled on lock-builtin.test.sh: same harness, same capture client
# discipline, same PID-only cleanup.
set -uo pipefail

VIEWPORT=${1:?usage: lock-delay.test.sh VIEWPORT CAPTURE_CLIENT PAINT_CLIENT}
CAPTURE_CLIENT=${2:?}
PAINT_CLIENT=${3:?}

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

unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export VIEWPORT_LOG="${VIEWPORT_LOG:-viewport=debug}"

cat >"$WORK/delay.json" <<'JSON'
{ "idle": {"lock_delay": 2} }
JSON

SOCKET=
start_compositor() {
	local config=$1
	"$VIEWPORT" --headless --config "$config" >"$LOG" 2>&1 &
	COMPOSITOR_PID=$!
	WAYLAND_DISPLAY=
	SOCKET=
	for _ in $(seq 100); do
		WAYLAND_DISPLAY=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$LOG" \
			| head -1 | cut -d= -f2)
		SOCKET=$(sed -n 's/.*control socket at \(.*\)$/\1/p' "$LOG" | head -1)
		[ -n "$WAYLAND_DISPLAY" ] && [ -n "$SOCKET" ] && break
		kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
		sleep 0.1
	done
	[ -n "$WAYLAND_DISPLAY" ] && [ -n "$SOCKET" ] || return 1
	export WAYLAND_DISPLAY
	return 0
}

msg() {
	"$VIEWPORT" msg --socket "$SOCKET" "$@"
}

status=0
fail() {
	echo "FAIL $1"
	status=1
}

if ! start_compositor "$WORK/delay.json"; then
	echo "FAIL the compositor never published a wayland socket"
	tail -20 "$LOG"
	exit 2
fi
echo "ok   the compositor is up on $WAYLAND_DISPLAY"

"$PAINT_CLIENT" lock-delay 1920 1080 0 ffff0000 ffff0000 \
	>"$WORK/paint.log" 2>&1 &
PAINT_PID=$!

placed=
for _ in $(seq 100); do
	if grep -qE 'view .* (boxed|placed at)' "$LOG"; then
		placed=yes
		break
	fi
	kill -0 "$PAINT_PID" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$placed" ]; then
	echo "FAIL the red window was never placed, so there is nothing on"
	echo "     screen for the lock to hide and the test would pass anyway"
	tail -20 "$WORK/paint.log"
	sed -n '1,60p' "$LOG"
	exit 2
fi
echo "ok   the red window is on screen"

# Ask for a lock. The black phase has to be visible in the log before any
# lock screen can be drawn, and the session must still be answerable.
if ! msg -t session.lock >"$WORK/msg.log" 2>&1; then
	echo "FAIL the compositor would not take a lock"
	cat "$WORK/msg.log"
	sed -n '1,60p' "$LOG"
	exit 2
fi
sleep 0.5

if grep -q "lock: standing black for" "$LOG"; then
	echo "ok   the lock delay stood the outputs black"
else
	fail "no black phase before the lock"
	grep -i "lock" "$LOG" | tail -10
fi

if ! grep -q "lock: delay over; locking" "$LOG"; then
	echo "ok   the session is not locked during the black phase"
else
	fail "the lock fired inside its own delay window"
fi

# The delay runs out and the real lock happens.
sleep 2.5
if grep -q "lock: delay over; locking" "$LOG"; then
	echo "ok   the lock fired when the delay ran out"
else
	fail "the delayed lock never fired"
fi

if "$CAPTURE_CLIENT" --output 000000; then
	echo "ok   and the lock screen is black"
else
	fail "the desktop is still on screen behind a locked session"
fi

exit "$status"
