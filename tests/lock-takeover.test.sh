#!/usr/bin/env bash
# A second locker may take over a dead lock screen, and only a dead one.
#
# ext-session-lock-v1 hands the compositor a decision the protocol does not
# make for it: smithay grants every `lock` request, builds a fresh lock object
# for it and calls the handler, so refusing is the compositor's job or nobody's.
#
# Both answers are wrong on their own. Granting every lock puts two clients on
# one screen — only the newer one is drawn, and unlocking the one you can see
# leaves the other holding a lock that nothing on screen can reach, which is a
# session still locked after a correct password. Refusing every lock takes away
# the only way out of a locker that crashed, which is running another one.
#
# So the rule is about pixels rather than about who asked first: a lock screen
# that is drawing may not be taken over, and one that is not may. This tests
# both halves against a live compositor, because the state that decides it —
# whether the locker's surfaces are still alive — is not something a unit test
# can hold.
set -uo pipefail

VIEWPORT=${1:?usage: lock-takeover.test.sh VIEWPORT LOCK_CLIENT}
LOCK_CLIENT=${2:?}

WORK=$(mktemp -d)
LOG="$WORK/viewport.log"
COMPOSITOR_PID=
HOLD_PID=

cleanup() {
	[ -n "$HOLD_PID" ] && kill "$HOLD_PID" 2>/dev/null
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

# Its own compositor, not the developer's session. See lock.test.sh.
unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"

"$VIEWPORT" --headless --timeout 1 >"$LOG" 2>&1 &
COMPOSITOR_PID=$!

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

status=0

# A locker that locks and draws, and stays.
"$LOCK_CLIENT" hold >"$WORK/hold.log" 2>&1 &
HOLD_PID=$!

for _ in $(seq 100); do
	grep -q "locked and drawing" "$WORK/hold.log" && break
	kill -0 "$HOLD_PID" 2>/dev/null || break
	sleep 0.1
done

if ! grep -q "locked and drawing" "$WORK/hold.log"; then
	echo "FAIL the first locker never drew a lock screen"
	cat "$WORK/hold.log"
	sed -n '1,40p' "$LOG"
	exit 2
fi
echo "ok   the first locker is up and drawing"

# The half that was broken: a second locker over it.
"$LOCK_CLIENT" second >"$WORK/second.log" 2>&1
case $? in
0)
	echo "ok   a second locker is refused while the first is drawing"
	;;
1)
	echo "FAIL a second locker was handed a session another locker is drawing"
	cat "$WORK/second.log"
	status=1
	;;
*)
	echo "FAIL the second locker could not run at all"
	cat "$WORK/second.log"
	exit 2
	;;
esac

if ! kill -0 "$HOLD_PID" 2>/dev/null; then
	echo "FAIL the first locker died during the refusal"
	cat "$WORK/hold.log"
	status=1
fi

# And the half that must keep working: the locker dies, so the next one in is
# allowed to take the session over. Without this the refusal above would be a
# session nobody can ever unlock.
kill -KILL "$HOLD_PID" 2>/dev/null
wait "$HOLD_PID" 2>/dev/null
HOLD_PID=
# Long enough for the compositor to see the connection drop; its surfaces are
# what the decision reads, and they die with the client rather than on a timer.
sleep 1

if "$LOCK_CLIENT" unlock >"$WORK/takeover.log" 2>&1; then
	echo "ok   a locker may take over once the first one is gone"
else
	echo "FAIL a dead lock screen could not be taken over — the session is stuck"
	cat "$WORK/takeover.log"
	sed -n '1,60p' "$LOG"
	status=1
fi

if ! kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
	echo "FAIL the compositor died somewhere in all this"
	sed -n '1,60p' "$LOG"
	status=1
fi

exit $status
