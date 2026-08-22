#!/usr/bin/env bash
# The lock screen the compositor draws itself, and the one way it may fail.
#
# `tests/lock.test.sh` is the same promise for somebody else's locker: a client
# that takes the lock and dies must not reveal the desktop. This is that
# promise for the lock screen drawn by the shell, where it is harder to keep
# and easier to get wrong — because the thing drawing the lock screen is the
# same page that draws the desktop, out of the same buffer, and "draw the
# shell" and "show the desktop" are one instruction apart.
#
# The compositor's answer is that it draws none of that buffer on a locked
# screen until the page has said it painted a lock screen for *this* lock and
# painted a frame after saying so — see `lock_screen_is_drawing`. So the case
# to test is the one where that never happens: a session locked with no page
# able to draw. That is exactly the compositor this suite starts, which has no
# working shell, and it stands in for every way a real one can fail — a crash,
# a hang, a page that will not paint.
#
# What must be true then: the screen is black, it stays black, and nothing that
# can be said on the control socket makes it anything else. A message claiming
# the lock screen has been drawn must not be enough on its own, and a password
# must go to PAM rather than being taken at its word.
#
# The second half is the escape hatch. `idle.lock_command` still means what it
# always meant: somebody who configured swaylock gets swaylock and not this.
set -uo pipefail

VIEWPORT=${1:?usage: lock-builtin.test.sh VIEWPORT CAPTURE_CLIENT PAINT_CLIENT}
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

unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export VIEWPORT_LOG="${VIEWPORT_LOG:-viewport=debug}"

# A config of this test's own, so the developer's own `idle` block — which may
# well name swaylock — cannot decide which half of this file runs.
cat >"$WORK/builtin.json" <<'JSON'
{ "idle": {} }
JSON

# The control socket this compositor made, named rather than guessed.
#
# `viewport msg` finds it from `WAYLAND_DISPLAY` and falls back to the newest
# `viewport-*.sock` in the runtime directory when that name is not there — and
# the runtime directory on a developer's machine holds the socket of the
# session they are sitting in, and of every other test running beside this one.
# A message sent to one of those is a message that never reaches the compositor
# under test, and it comes back as an error from a build that has never heard
# of it, which reads exactly like the feature being broken. So it is read out
# of the log and passed with `--socket`.
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

stop_compositor() {
	[ -n "$PAINT_PID" ] && kill "$PAINT_PID" 2>/dev/null
	PAINT_PID=
	[ -n "$COMPOSITOR_PID" ] || return 0
	kill -TERM "$COMPOSITOR_PID" 2>/dev/null
	for _ in $(seq 20); do
		kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
		sleep 0.1
	done
	kill -KILL "$COMPOSITOR_PID" 2>/dev/null
	wait "$COMPOSITOR_PID" 2>/dev/null
	COMPOSITOR_PID=
}

status=0
fail() {
	echo "FAIL $1"
	status=1
}

# ── The built-in lock screen, with nothing able to draw one ──────────────────

if ! start_compositor "$WORK/builtin.json"; then
	echo "FAIL the compositor never published a wayland socket"
	tail -20 "$LOG"
	exit 2
fi
echo "ok   the compositor is up on $WAYLAND_DISPLAY"

# Something bright, so "every pixel is black" can fail. Without it a headless
# desktop with no windows is already black and this test cannot tell a lock
# screen from a compositor that draws nothing at all.
"$PAINT_CLIENT" lock-builtin 1920 1080 0 ffff0000 ffff0000 \
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

if "$CAPTURE_CLIENT" --output 000000 >"$WORK/precheck.log" 2>&1; then
	echo "FAIL nothing visible on screen before locking, so the test would"
	echo "     pass no matter what the lock did"
	tail -20 "$LOG"
	exit 2
fi
echo "ok   there is something on screen for the lock to hide"

# Lock, the way the `lock` binding, the lid and the power menu's row all do.
if ! msg -t session.lock >"$WORK/msg.log" 2>&1; then
	echo "FAIL the compositor would not take a lock"
	cat "$WORK/msg.log"
	sed -n '1,60p' "$LOG"
	exit 2
fi
sleep 1

if grep -q "the shell draws the lock screen" "$LOG"; then
	echo "ok   with no lock_command configured, locking is the shell's screen"
else
	fail "locking did not choose the built-in lock screen"
	grep -i "lock" "$LOG" | tail -10
fi

if "$CAPTURE_CLIENT" --output 000000; then
	echo "ok   and with nothing able to draw one, the screen is black"
else
	fail "the desktop is still on screen behind a locked session"
fi

# A message claiming the lock screen has been drawn.
#
# The compositor requires a *frame* after this before it draws any of the
# shell's buffer, and there is no shell here to paint one — so this must change
# nothing. It is not a hypothetical: the control socket is not the shell's
# alone, so anything on the machine can say this, and if saying it were enough
# the guard would be a comment rather than a check.
msg -t session.lock.drawn --generation 1 >>"$WORK/msg.log" 2>&1
sleep 0.5
if "$CAPTURE_CLIENT" --output 000000; then
	echo "ok   and saying the lock screen is drawn does not put the desktop back"
else
	fail "a claim that the lock screen was drawn revealed the desktop"
fi

# A password. Whatever PAM says — and on a test runner it will not say yes to
# this — the session must not come back on the strength of the message itself.
msg -t session.unlock --generation 1 --password not-the-password \
	>>"$WORK/msg.log" 2>&1
sleep 1
if "$CAPTURE_CLIENT" --output 000000; then
	echo "ok   and a password nobody set does not unlock it"
else
	fail "a wrong password unlocked the session"
fi
if grep -q "session unlocked" "$LOG"; then
	fail "the compositor unlocked on a password it should have refused"
fi

# And it says what has happened, because from the front this is a black screen
# that eats every key and nothing else can explain it.
if grep -q "the shell has not drawn a lock screen" "$LOG"; then
	echo "ok   and the compositor says why the screen is black"
else
	fail "nothing in the log explains a locked session with nothing drawing"
fi

stop_compositor

# ── The escape hatch: idle.lock_command still runs somebody else's locker ────

cat >"$WORK/command.json" <<JSON
{ "idle": { "lock_command": "touch $WORK/ran-the-locker" } }
JSON

if ! start_compositor "$WORK/command.json"; then
	echo "FAIL the compositor never came up for the second half"
	tail -20 "$LOG"
	exit 2
fi

msg -t session.lock >>"$WORK/msg.log" 2>&1
ran=
for _ in $(seq 50); do
	[ -e "$WORK/ran-the-locker" ] && { ran=yes; break; }
	sleep 0.1
done
if [ -n "$ran" ]; then
	echo "ok   a configured lock_command is still what locking runs"
else
	fail "idle.lock_command was not run; the escape hatch is gone"
	grep -i "lock" "$LOG" | tail -10
fi
if grep -q "the shell draws the lock screen" "$LOG"; then
	fail "the built-in screen was drawn over a configured locker"
fi

exit "$status"
