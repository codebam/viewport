#!/usr/bin/env bash
# The ext-session-lock-v1 lifecycle, including the requests a correct client
# never makes and the takeover a crashed or hung one needs.
#
# Every protocol-error probe checks the exact interface and enum value in
# lock-client, then takes and releases an ordinary lock on a fresh connection.
# The takeover probes also follow each surface's frame callbacks and
# wl_keyboard: output captures deliberately return black for every locked frame
# rather than leaking the password UI, so a callback is the outside evidence
# that a surface reached the render pass. A refused or superseded locker may try
# to paint, but only the accepted surface is presented or receives focus. The
# control socket deliberately refuses input.key while locked, so enter/leave
# events pin focus ownership rather than pretending to inject a password. A
# panic, abort, wedge, wrong error, hidden surface, or hidden focus recipient
# all fail.
set -uo pipefail

VIEWPORT=${1:?usage: lock-lifecycle.test.sh VIEWPORT LOCK_CLIENT}
LOCK_CLIENT=${2:?}

WORK=$(mktemp -d)
LOG="$WORK/viewport.log"
COMPOSITOR_PID=
LOCKER_PID=
PIDS=()
status=0

cleanup() {
	local pid
	for pid in "${PIDS[@]}"; do
		kill "$pid" 2>/dev/null
	done
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

# Keep this run independent of a developer's idle.lock_command. The last
# scenario also exercises takeover by the built-in lock through this config.
cat >"$WORK/builtin.json" <<'JSON'
{ "idle": {} }
JSON

"$VIEWPORT" --headless --config "$WORK/builtin.json" >"$LOG" 2>&1 &
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
if [ -z "$WAYLAND_DISPLAY" ] || [ -z "$SOCKET" ]; then
	echo "FAIL the compositor never published its sockets"
	tail -30 "$LOG"
	exit 2
fi
export WAYLAND_DISPLAY
echo "ok   the compositor is up on $WAYLAND_DISPLAY"

msg() {
	"$VIEWPORT" msg --socket "$SOCKET" "$@"
}


fail() {
	echo "FAIL $1"
	status=1
}

ok() {
	echo "ok   $1"
}

wait_text() {
	local file=$1 text=$2
	for _ in $(seq 100); do
		grep -q "$text" "$file" 2>/dev/null && return 0
		sleep 0.1
	done
	return 1
}

frame_count() {
	grep -c '^frame-done' "$1" 2>/dev/null || true
}

wait_next_frame() {
	local file=$1 before=$2 what=$3
	for _ in $(seq 100); do
		if [ "$(frame_count "$file")" -gt "$before" ]; then
			ok "$what"
			return 0
		fi
		sleep 0.1
	done
	fail "$what"
	return 1
}

start_locker() {
	local mode=$1 file=$2
	"$LOCK_CLIENT" "$mode" >"$file" 2>&1 &
	LOCKER_PID=$!
	PIDS+=("$LOCKER_PID")
}

stop_locker() {
	local pid=$1
	kill -KILL "$pid" 2>/dev/null
	wait "$pid" 2>/dev/null
}

compositor_alive() {
	if kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
		return 0
	fi
	fail "the compositor died"
	tail -60 "$LOG"
	return 1
}

fresh_unlock() {
	local after=$1
	# Let the offending connection's hangup reach Smithay's destroyed hook.
	sleep 0.2
	if timeout 15 "$LOCK_CLIENT" unlock >"$WORK/fresh-$after.log" 2>&1; then
		ok "a fresh ordinary lock/unlock succeeds after $after"
		compositor_alive
	else
		fail "a fresh ordinary lock/unlock failed after $after"
		cat "$WORK/fresh-$after.log"
		compositor_alive
		tail -40 "$LOG"
	fi
}

fresh_second() {
	local after=$1
	if timeout 15 "$LOCK_CLIENT" second >"$WORK/fresh-second-$after.log" 2>&1; then
		ok "a fresh client still gets the ordinary refusal after $after"
	else
		fail "a fresh client did not get the ordinary refusal after $after"
		cat "$WORK/fresh-second-$after.log"
		compositor_alive
	fi
}

run_error_probe() {
	local mode=$1
	if timeout 15 "$LOCK_CLIENT" "$mode" >"$WORK/$mode.log" 2>&1; then
		ok "$mode gets its exact protocol refusal"
	else
		fail "$mode did not get its exact protocol refusal"
		cat "$WORK/$mode.log"
	fi
	fresh_unlock "$mode"
}

# Object construction, destruction, and lock-surface commit errors. The client
# validates both the interface and error code; these shell-side checks prove
# that only the offending connection died.
for probe in \
	duplicate-output \
	already-attached \
	already-committed \
	role \
	destroy-locked \
	commit-before-ack \
	null-buffer \
	dimensions-mismatch \
	invalid-serial; do
	run_error_probe "$probe"
done

# Unlock, then a different client locks and paints. Both clients verify that
# every output was configured and drawn; the capture distinguishes the second.
if timeout 15 "$LOCK_CLIENT" draw-unlock >"$WORK/draw-unlock.log" 2>&1; then
	ok "a drawing locker can unlock_and_destroy"
else
	fail "a drawing locker could not unlock_and_destroy"
	cat "$WORK/draw-unlock.log"
fi
start_locker hold-green "$WORK/relock-green.log"
if wait_text "$WORK/relock-green.log" "locked and drawing"; then
	ok "a new client is granted the lock after unlock_and_destroy"
	wait_next_frame "$WORK/relock-green.log" 0 \
		"the new locker's configured surfaces reach the compositor's render pass"
else
	fail "the new locker did not configure and draw after relock"
	cat "$WORK/relock-green.log"
fi
stop_locker "$LOCKER_PID"
sleep 1
fresh_unlock relock

# A drawing lock refuses a second one. Both the early unlock and the attempt to
# paint after `finished` get protocol errors; presentation and keyboard focus
# stay exactly where they were.
start_locker hold "$WORK/refused-hold.log"
if ! wait_text "$WORK/refused-hold.log" "locked and drawing"; then
	echo "FAIL the first locker never drew"
	cat "$WORK/refused-hold.log"
	exit 2
fi
if timeout 15 "$LOCK_CLIENT" second >"$WORK/second.log" 2>&1; then
	ok "a second lock over a drawing one is refused"
else
	fail "a second lock over a drawing one was not refused"
	cat "$WORK/second.log"
fi
fresh_second second
if timeout 15 "$LOCK_CLIENT" unlock-refused >"$WORK/unlock-refused.log" 2>&1; then
	ok "unlock before locked gets invalid_unlock"
else
	fail "unlock before locked did not get invalid_unlock"
	cat "$WORK/unlock-refused.log"
fi
fresh_second unlock-refused
if timeout 15 "$LOCK_CLIENT" second-draw >"$WORK/second-draw.log" 2>&1; then
	ok "a refused locker's hostile commit gets its own protocol refusal"
else
	fail "a refused locker's hostile commit was not refused correctly"
	cat "$WORK/second-draw.log"
fi
fresh_second second-draw
REFUSED_FRAMES=$(frame_count "$WORK/refused-hold.log")
wait_next_frame "$WORK/refused-hold.log" "$REFUSED_FRAMES" \
	"the accepted locker still reaches the render pass after the refused draw"
sleep 0.5
if grep -q '^frame-done' "$WORK/second-draw.log"; then
	fail "the refused locker's surface reached the render pass"
else
	ok "the refused locker's surface never reaches the render pass"
fi
if grep -q 'keyboard-leave' "$WORK/refused-hold.log"; then
	fail "the accepted locker lost keyboard focus during the refusal"
else
	ok "the accepted locker never loses keyboard focus during the refusal"
fi
if grep -q 'keyboard-enter' "$WORK/second-draw.log"; then
	fail "the refused locker received keyboard focus"
else
	ok "the refused locker never received keyboard focus"
fi
if msg -t session.query >/dev/null 2>&1; then
	ok "the control socket still answers after refused-lock probes"
else
	fail "the control socket stopped answering after refused-lock probes"
fi
stop_locker "$LOCKER_PID"
sleep 1
fresh_unlock refused-draw

# No lock surface at all: another locker may take over, and the superseded lock
# must be told `finished` so it does not wait forever on a session it no longer
# owns.
start_locker idle "$WORK/idle.log"
if ! wait_text "$WORK/idle.log" "idle lock ready"; then
	echo "FAIL the idle locker never locked"
	cat "$WORK/idle.log"
	exit 2
fi
IDLE_PID=$LOCKER_PID
start_locker hold-green "$WORK/idle-survivor.log"
if ! wait_text "$WORK/idle-survivor.log" "locked and drawing"; then
	fail "a locker with no surface could not be taken over"
	cat "$WORK/idle-survivor.log"
fi
if wait "$IDLE_PID"; then
	ok "a superseded locker with no surface gets finished"
else
	fail "a superseded locker with no surface did not get finished"
	cat "$WORK/idle.log"
fi
wait_next_frame "$WORK/idle-survivor.log" 0 \
	"the survivor reaches the render pass after a surfaceless takeover"
stop_locker "$LOCKER_PID"
sleep 1
fresh_unlock idle-takeover

# A configured surface with no buffer is also not drawing. Once superseded it
# can legally commit its acknowledged size, but that stale red frame must not
# be rendered or focused. The green survivor keeps presentation and focus.
start_locker idle-surface "$WORK/stale-surface.log"
if ! wait_text "$WORK/stale-surface.log" "idle lock ready"; then
	echo "FAIL the empty-surface locker never configured"
	cat "$WORK/stale-surface.log"
	exit 2
fi
STALE_PID=$LOCKER_PID
start_locker hold-green "$WORK/stale-survivor.log"
if ! wait_text "$WORK/stale-survivor.log" "locked and drawing"; then
	fail "a locker with an empty surface could not be taken over"
fi
if wait_text "$WORK/stale-surface.log" "superseded surface drew red"; then
	ok "a superseded locker gets finished and can attempt its stale draw"
else
	fail "the empty-surface locker did not get finished"
	cat "$WORK/stale-surface.log"
fi
STALE_SURVIVOR_FRAMES=$(frame_count "$WORK/stale-survivor.log")
wait_next_frame "$WORK/stale-survivor.log" "$STALE_SURVIVOR_FRAMES" \
	"the survivor still reaches the render pass after a legal stale draw"
sleep 0.5
if grep -q '^frame-done' "$WORK/stale-surface.log"; then
	fail "the superseded locker's stale surface reached the render pass"
else
	ok "the superseded locker's legal stale draw never reaches the render pass"
fi
if wait_text "$WORK/stale-survivor.log" 'keyboard-enter' &&
		! grep -q 'keyboard-leave' "$WORK/stale-survivor.log"; then
	ok "the surviving locker keeps keyboard focus after a stale draw"
else
	fail "the surviving locker lost keyboard focus after a stale draw"
fi
if grep -q 'keyboard-enter' "$WORK/stale-surface.log"; then
	fail "the superseded locker received keyboard focus"
	cat "$WORK/stale-surface.log"
else
	ok "the superseded locker never receives keyboard focus"
fi
stop_locker "$STALE_PID"
stop_locker "$LOCKER_PID"
sleep 1
fresh_unlock stale-surface

# The stale lock object cannot unlock its replacement. Smithay answers
# invalid_unlock; the accepted locker's green pixels and control socket remain.
start_locker stale-unlock "$WORK/stale-unlock.log"
if ! wait_text "$WORK/stale-unlock.log" "idle lock ready"; then
	echo "FAIL the stale-unlock locker never configured"
	cat "$WORK/stale-unlock.log"
	exit 2
fi
STALE_UNLOCK_PID=$LOCKER_PID
start_locker hold-green "$WORK/stale-unlock-survivor.log"
if ! wait_text "$WORK/stale-unlock-survivor.log" "locked and drawing"; then
	fail "the survivor did not draw for the stale-unlock probe"
fi
if wait "$STALE_UNLOCK_PID"; then
	ok "unlock from a superseded lock object gets invalid_unlock"
else
	fail "unlock from a superseded lock object was not refused correctly"
	cat "$WORK/stale-unlock.log"
fi
wait_next_frame "$WORK/stale-unlock-survivor.log" 0 \
	"a stale unlock leaves the accepted lock reaching the render pass"
if msg -t session.query >/dev/null 2>&1; then
	ok "the accepted lock still answers after a stale unlock"
else
	fail "the accepted lock stopped answering after a stale unlock"
fi
stop_locker "$LOCKER_PID"
sleep 1
fresh_unlock stale-unlock

# The built-in recovery path retires an undrawing external lock by the same
# rule. With no shell in this test its own screen is black, and a later working
# external locker can still take over and draw.
start_locker idle "$WORK/builtin-idle.log"
if ! wait_text "$WORK/builtin-idle.log" "idle lock ready"; then
	echo "FAIL the built-in takeover locker never locked"
	cat "$WORK/builtin-idle.log"
	exit 2
fi
BUILTIN_IDLE_PID=$LOCKER_PID
if msg -t session.lock >/dev/null 2>&1; then
	ok "the built-in lock can take over an undrawing external lock"
else
	fail "the built-in lock could not take over an undrawing external lock"
fi
if wait "$BUILTIN_IDLE_PID"; then
	ok "built-in takeover sends the old locker finished"
else
	fail "built-in takeover did not send the old locker finished"
	cat "$WORK/builtin-idle.log"
fi
start_locker hold-green "$WORK/builtin-survivor.log"
if wait_text "$WORK/builtin-survivor.log" "locked and drawing"; then
	wait_next_frame "$WORK/builtin-survivor.log" 0 \
		"an external locker recovers the undrawing built-in lock and renders"
else
	fail "an external locker could not recover the built-in lock"
fi
stop_locker "$LOCKER_PID"
sleep 1
fresh_unlock builtin-takeover

compositor_alive
exit "$status"
