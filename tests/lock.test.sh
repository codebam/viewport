#!/usr/bin/env bash
# A locker that dies must not reveal the desktop.
#
# ext-session-lock-v1 makes one promise no other protocol does: if the client
# holding the lock crashes, the session stays locked. That promise is about
# pixels, and the compositor cannot check it about itself — its own state says
# `locked` either way. The lock layer is a scene tree, and an empty tree draws
# nothing and hides nothing, so a compositor that lets the locker's surfaces go
# without putting something in their place carries on reporting a locked
# session while displaying the desktop underneath it.
#
# So: lock the session with a client that then exits without unlocking, and
# capture the screen from a second client. Every pixel must be black.
#
# The backdrop is what does the covering, so this fails against a compositor
# that ties the backdrop's lifetime to the lock client — which is what it did
# before: destroying it on lock-client destroy left the desktop visible, every
# window readable, and the shell taking input, with no client able to be
# clicked again because `locked` was still true.
set -uo pipefail

VIEWPORT=${1:?usage: lock.test.sh VIEWPORT LOCK_CLIENT CAPTURE_CLIENT PAINT_CLIENT}
LOCK_CLIENT=${2:?}
CAPTURE_CLIENT=${3:?}
PAINT_CLIENT=${4:?}

WORK=$(mktemp -d)
LOG="$WORK/viewport.log"
COMPOSITOR_PID=
PAINT_PID=

cleanup() {
	[ -n "$PAINT_PID" ] && kill "$PAINT_PID" 2>/dev/null
	if [ -n "$COMPOSITOR_PID" ] && kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
		kill -TERM "$COMPOSITOR_PID" 2>/dev/null
		# By pid, never by name: this runs on developer machines where the
		# session itself is very often another viewport.
		for _ in $(seq 20); do
			kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
			sleep 0.1
		done
		kill -KILL "$COMPOSITOR_PID" 2>/dev/null
	fi
	rm -rf "$WORK"
}
trap cleanup EXIT

# Off whatever session is already running: this starts its own compositor and
# must not join, or be joined to, the one the developer is sitting in. The
# renderer is left alone — pixman has no DRM fd, and the web engine needs one
# to share buffers, so forcing it here stops the compositor before it starts.
unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"

"$VIEWPORT" --headless --timeout 1 >"$LOG" 2>&1 &
COMPOSITOR_PID=$!

# libwayland picks the socket name and it is printed at startup; there is no
# way to ask for a particular one, so it is read back out of the log.
WAYLAND_DISPLAY=
for _ in $(seq 100); do
	WAYLAND_DISPLAY=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$LOG" \
		| head -1 | cut -d= -f2)
	[ -n "$WAYLAND_DISPLAY" ] && break
	kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
	sleep 0.1
done

if [ -z "$WAYLAND_DISPLAY" ]; then
	echo "FAIL the compositor never published a wayland socket"
	tail -20 "$LOG"
	exit 2
fi
export WAYLAND_DISPLAY
echo "ok   the compositor is up on $WAYLAND_DISPLAY"

# Put something bright on the screen first.
#
# Without this the test cannot fail: a headless desktop with no windows is
# already black, so "every pixel is black" is true whether or not anything is
# covering it. A red window fills the output — no shell is laying anything out
# here, so the compositor's own fallback tiler gives the only window the whole
# screen — and the check below confirms it really is visible before the lock is
# taken. That is what the lock then has to hide.
"$PAINT_CLIENT" lock-test 1920 1080 0 ffff0000 ffff0000 \
	>"$WORK/paint.log" 2>&1 &
PAINT_PID=$!

# The fallback tiler waits a couple of seconds before placing an unplaced
# window, so this cannot be a short sleep.
sleep 5

if "$CAPTURE_CLIENT" --output 000000 >"$WORK/precheck.log" 2>&1; then
	echo "FAIL nothing visible on screen before locking, so the test would"
	echo "     pass no matter what the lock did"
	tail -20 "$WORK/paint.log"
	tail -20 "$LOG"
	exit 2
fi
echo "ok   there is something on screen for the lock to hide"

# Lock, then die without unlocking.
if ! "$LOCK_CLIENT" crash; then
	echo "FAIL the locker could not lock the session"
	sed -n '1,40p' "$LOG"
	exit 2
fi
echo "ok   the locker locked the session and exited without unlocking"

if ! kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
	echo "FAIL the compositor died along with the locker"
	sed -n '1,60p' "$LOG"
	exit 1
fi
echo "ok   the compositor survives the locker"

# Give it a moment to notice the client is gone and repaint.
sleep 1

# The whole point: what is on the screen now.
if "$CAPTURE_CLIENT" --output 000000; then
	echo "ok   the screen stays covered after the locker dies"
	status=0
else
	echo "FAIL the screen is not covered after the locker dies"
	status=1
fi

grep -q "session stays locked" "$LOG" \
	&& echo "ok   and the compositor says so" \
	|| echo "note no 'session stays locked' line in the log"

exit $status
