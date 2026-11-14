#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# The view.* messages, end to end, without a shell.
#
# The unit tests in viewport-ipc cover how each message parses. What they cannot
# cover is the half this file is about: that a request sent with
# `viewport msg` is accepted (a no-reply message is accepted when the barrier
# behind it comes back clean, a refusal is a non-zero exit with the reason on
# stderr), and that the window the request names visibly changes state — a
# focus lands as a `view.focused`, a minimize lands as the `minimized` flag
# in the next `view.query`, a close lands as a `view.removed`, and a layout
# lands in the placement line the compositor logs.
#
# One paint client is the window: deterministically sized, and (in `retitle`
# mode) able to change its own title on SIGUSR1, which is what `view.props`
# reports. Fullscreen, maximized and minimized go through the foreign-toplevel
# manager as well: the compositor records those states in the same place for
# both the socket and the protocol, so the watch client printing
# `fullscreen=1` is the observable that a socket-driven change reached the
# window, not just that the message parsed.
#
# `view.parent` has no test here: it is only sent for a dialog parented over
# xdg-foreign (or a portal window), and neither client the suite builds can
# create one. The paint popup is a popup, not a dialog view. The message is
# listed in docs/ipc-coverage.md as not testable with these clients.
#
# Three messages are sent with `--raw` because `viewport msg -t` does not
# offer them at all: `view.opacity_rule`, `view.tearing` and
# `view.idle_inhibit` are missing from the `TYPES` table in
# crates/viewport/src/msg.rs (see FIXES.md). Their `-t` form is checked below
# as a known failure, and `--raw` — which is still `viewport msg`, and still
# goes through the compositor's own parser — is what the functional checks use.
#
#   tests/view-ipc.test.sh target/debug/viewport \
#     path/to/paint-client path/to/foreign-toplevel-client
set -uo pipefail

VIEWPORT=${1:?usage: view-ipc.test.sh VIEWPORT PAINT_CLIENT FT_CLIENT}
PAINT_CLIENT=${2:?}
FT_CLIENT=${3:?}

for binary in "$VIEWPORT" "$PAINT_CLIENT" "$FT_CLIENT"; do
	if [ ! -x "$binary" ]; then
		echo "missing $binary — build first (scripts/integration.sh builds the clients)" >&2
		exit 2
	fi
done
VIEWPORT=$(realpath "$VIEWPORT")
PAINT_CLIENT=$(realpath "$PAINT_CLIENT")
FT_CLIENT=$(realpath "$FT_CLIENT")

WORK=$(mktemp -d)
LOG="$WORK/viewport.log"
COMPOSITOR_PID=
PAINT_PID=
FOOT_PID=
WATCH_PID=

# By PID only. Pattern matching on "viewport" has killed a live session more
# than once and this runs on the same machine as one.
cleanup() {
	[ -n "$FOOT_PID" ] && kill "$FOOT_PID" 2>/dev/null
	[ -n "$PAINT_PID" ] && kill "$PAINT_PID" 2>/dev/null
	[ -n "$WATCH_PID" ] && kill "$WATCH_PID" 2>/dev/null
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

# Off whatever session is already running: this starts its own compositor and
# must not join, or be joined to, the one the developer is sitting in.
unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="$WORK/run"
export XDG_STATE_HOME="$WORK/state"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME"

printf '{ "layout": "tiling", "tray": false }\n' >"$WORK/config.json"

# Debug so the placement line below is visible. A value from the caller wins.
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

# One subscriber for the whole run: focused/props/removed/configured and the
# fullscreen replay, read back from a file once the subscriber is stopped.
SUB_OUT="$WORK/subscribe.jsonl"
"$VIEWPORT" msg --socket "$SOCKET" --timeout 0 -t subscribe \
	view.focused view.props view.removed view.configured shell.command \
	>"$SUB_OUT" 2>"$WORK/subscribe.err" &
SUB_PID=$!
sleep 0.5

msg() {
	"$VIEWPORT" msg --socket "$SOCKET" "$@"
}

# The view id for an app_id, from the one query the shell itself uses.
view_id_for() {
	msg -t view.query 2>/dev/null |
		grep "\"app_id\":\"$1\"" |
		sed -n 's/.*"id":\([0-9]*\).*/\1/p' | head -1
}

# One field of the replayed `view.added` for a view. String fields are
# unquoted; booleans and numbers pass through the same way.
added_field() {
	local id=$1 field=$2
	msg -t view.query 2>/dev/null |
		grep "\"id\":$id," |
		sed -n "s/.*\"$field\":\"*\([^,\"]*\)\"*.*/\1/p" | head -1
}

# Wait (up to $2 tenths) for $1 to appear in a file.
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

# Wait for one line carrying two substrings. JSON key order is the msg
# client's (sorted Value), not the struct's, so a single-substring wait on
# a type-and-id pair is order-dependent, which is why this checks both.
wait_for_both() {
	local file=$1 needle1=$2 needle2=$3
	for _ in $(seq 1 "${4:-50}"); do
		if grep -- "$needle1" "$file" 2>/dev/null | grep -q -- "$needle2"; then
			return 0
		fi
		kill -0 "$COMPOSITOR_PID" 2>/dev/null || return 1
		sleep 0.1
	done
	return 1
}

# The foreign-toplevel watch client: started before anything maps, and prints
# the first toplevel's state once per change. Socket-driven fullscreen /
# maximized / minimized must land here.
WATCH_OUT="$WORK/watch.log"
"$FT_CLIENT" watch >"$WATCH_OUT" 2>"$WORK/watch.err" &
WATCH_PID=$!
sleep 0.5

# retitle lets the props check produce an identity change on cue.
"$PAINT_CLIENT" viewport-view-ipc 320 240 24 ffff0000 ff0000ff retitle \
	>"$WORK/paint.log" 2>&1 &
PAINT_PID=$!

for _ in $(seq 1 100); do
	if grep -qE 'view [0-9]+: ' "$LOG" 2>/dev/null; then
		break
	fi
	kill -0 "$PAINT_PID" 2>/dev/null || break
	sleep 0.1
done
VID=$(view_id_for "viewport-view-ipc")
if [ -z "$VID" ]; then
	echo "FAIL the paint window never mapped, so there is no view to drive" >&2
	tail -10 "$WORK/paint.log" >&2
	tail -20 "$LOG" >&2
	exit 2
fi
echo "ok   the paint window is view $VID"

# --- view.added fields, as the shell reads them -----------------------------
check "view.added names the output" "HEADLESS-1" \
	"$(added_field "$VID" output)"
check "view.added carries the app_id" "viewport-view-ipc" \
	"$(added_field "$VID" app_id)"
# The replay is a replay (the shell reloads and rebuilds its tree from it);
# a fresh map is not.
check "the replayed view.added says so" "true" \
	"$(added_field "$VID" replay)"

# --- view.layout ----------------------------------------------------------
if msg -t view.layout --id "$VID" --x 10 --y 20 --width 800 --height 600 >/dev/null 2>&1; then
	echo "ok: view.layout was accepted"
else
	echo "FAIL: view.layout was refused" >&2
	failures=$((failures + 1))
fi
if wait_for "$LOG" "placed at 800x600" 50; then
	echo "ok: the layout landed (the placement line says so)"
else
	echo "FAIL: no placement line for the layout — nothing moved" >&2
	tail -10 "$LOG" >&2
	failures=$((failures + 1))
fi
check "and the window is still listed afterwards" "$VID" \
	"$(view_id_for "viewport-view-ipc")"

# A layout the shell animates through must not spam the shell with a
# `view.configured` for every frame: an ordinary (in-minimum) size is silent.
# The file is truncated in place so this can only see what this layout did.
SUB_MARK=$(wc -l <"$SUB_OUT")
sleep 0.5
check "an in-minimum layout says nothing about being configured" no \
	"$(tail -n +"$((SUB_MARK + 1))" "$SUB_OUT" 2>/dev/null | grep -q '"type":"view.configured"' && echo yes || echo no)"

# --- view.focus -----------------------------------------------------------
if msg -t view.focus --id "$VID" >/dev/null 2>&1; then
	echo "ok: view.focus was accepted"
else
	echo "FAIL: view.focus was refused" >&2
	failures=$((failures + 1))
fi
if wait_for_both "$SUB_OUT" '"type":"view.focused"' "\"id\":$VID" 50; then
	echo "ok: and it landed as a view.focused for that window"
else
	echo "FAIL: no view.focused for the focused window" >&2
	failures=$((failures + 1))
fi

# --- view.visible ---------------------------------------------------------
# Hiding unmaps the window; it stays mapped in the view registry, so
# `view.query` still lists it. The observable that it happened is that the
# message was accepted and the window is still there to be shown again —
# `view.layout` re-shows it, which is also how a workspace switch returns.
if msg -t view.visible --id "$VID" --visible false >/dev/null 2>&1; then
	echo "ok: view.visible false was accepted"
else
	echo "FAIL: view.visible false was refused" >&2
	failures=$((failures + 1))
fi
check "a hidden window is still listed (it is unmapped, not closed)" "$VID" \
	"$(view_id_for "viewport-view-ipc")"
if msg -t view.layout --id "$VID" --x 10 --y 20 --width 800 --height 600 >/dev/null 2>&1; then
	echo "ok: laying it out again was accepted (which also re-shows it)"
else
	echo "FAIL: the re-layout was refused" >&2
	failures=$((failures + 1))
fi

# --- view.fullscreen / view.maximized / view.minimized --------------------
# Two observables: the foreign-toplevel state (recorded synchronously for
# both the socket and the protocol), and for fullscreen the
# `window.fullscreen.set` shell command replayed by the next `view.query`.
FETCH() {
	msg -t view.fullscreen --id "$VID" --fullscreen true >/dev/null 2>&1
	msg -t view.query >/dev/null 2>&1
}
FETCH
if wait_for "$WATCH_OUT" "fullscreen=1" 50; then
	echo "ok: view.fullscreen true published fullscreen=1 to a taskbar"
else
	echo "FAIL: a taskbar never saw the socket-driven fullscreen" >&2
	failures=$((failures + 1))
fi
if wait_for "$SUB_OUT" "window.fullscreen.set" 50; then
	echo "ok: and the next view.query replayed window.fullscreen.set"
else
	echo "FAIL: no window.fullscreen.set replay after view.query" >&2
	failures=$((failures + 1))
fi
if msg -t view.fullscreen --id "$VID" --fullscreen false >/dev/null 2>&1; then
	echo "ok: view.fullscreen false was accepted"
else
	echo "FAIL: view.fullscreen false was refused" >&2
	failures=$((failures + 1))
fi
if wait_for "$WATCH_OUT" "fullscreen=0" 50; then
	echo "ok: and clearing it published fullscreen=0"
else
	echo "FAIL: clearing fullscreen was never published" >&2
	failures=$((failures + 1))
fi

if msg -t view.maximized --id "$VID" --maximized true >/dev/null 2>&1; then
	echo "ok: view.maximized true was accepted"
else
	echo "FAIL: view.maximized true was refused" >&2
	failures=$((failures + 1))
fi
if wait_for "$WATCH_OUT" "maximized=1" 50; then
	echo "ok: and it published maximized=1 to a taskbar"
else
	echo "FAIL: a taskbar never saw the socket-driven maximized" >&2
	failures=$((failures + 1))
fi
if msg -t view.maximized --id "$VID" --maximized false >/dev/null 2>&1; then
	echo "ok: view.maximized false was accepted"
else
	echo "FAIL: view.maximized false was refused" >&2
	failures=$((failures + 1))
fi
if wait_for "$WATCH_OUT" "maximized=0" 50; then
	echo "ok: and clearing it published maximized=0"
else
	echo "FAIL: clearing maximized was never published" >&2
	failures=$((failures + 1))
fi

if msg -t view.minimized --id "$VID" --minimized true >/dev/null 2>&1; then
	echo "ok: view.minimized true was accepted"
else
	echo "FAIL: view.minimized true was refused" >&2
	failures=$((failures + 1))
fi
if wait_for "$WATCH_OUT" "minimized=1" 50; then
	echo "ok: and it published minimized=1 to a taskbar"
else
	echo "FAIL: a taskbar never saw the socket-driven minimized" >&2
	failures=$((failures + 1))
fi
# The shell-observable half: the replayed view.added carries it.
for _ in $(seq 1 50); do
	[ "$(added_field "$VID" minimized)" = "true" ] && break
	sleep 0.1
done
check "and the next view.query says minimized true" "true" \
	"$(added_field "$VID" minimized)"
# Minimizing the focused window takes the focus away to the shell (id 0).
if wait_for_both "$SUB_OUT" '"type":"view.focused"' '"id":0' 50; then
	echo "ok: and minimizing the focused window dropped focus to the shell"
else
	echo "FAIL: minimizing did not clear the focus" >&2
	failures=$((failures + 1))
fi
if msg -t view.minimized --id "$VID" --minimized false >/dev/null 2>&1; then
	echo "ok: view.minimized false was accepted"
else
	echo "FAIL: view.minimized false was refused" >&2
	failures=$((failures + 1))
fi
for _ in $(seq 1 50); do
	[ "$(added_field "$VID" minimized)" = "false" ] && break
	sleep 0.1
done
check "and the next view.query says minimized false again" "false" \
	"$(added_field "$VID" minimized)"

# --- view.opacity / view.opacity_rule / view.tearing / view.idle_inhibit --
# These set render- or policy-only state that no socket message reports, so
# the observable is acceptance (the barrier behind the message is clean) and
# that the window is still listed and the compositor still answers. An
# out-of-range opacity is clamped, not refused, which the second check pins.
if msg -t view.opacity --id "$VID" --opacity 0.5 >/dev/null 2>&1; then
	echo "ok: view.opacity was accepted"
else
	echo "FAIL: view.opacity was refused" >&2
	failures=$((failures + 1))
fi
if msg -t view.opacity --id "$VID" --opacity 2.0 >/dev/null 2>&1; then
	echo "ok: an out-of-range opacity is clamped, not refused"
else
	echo "FAIL: an out-of-range opacity was refused" >&2
	failures=$((failures + 1))
fi
if msg --raw "{\"type\":\"view.opacity_rule\",\"id\":$VID,\"opacity\":0.5}" >/dev/null 2>&1; then
	echo "ok: view.opacity_rule (via --raw) was accepted"
else
	echo "FAIL: view.opacity_rule (via --raw) was refused" >&2
	failures=$((failures + 1))
fi
# Each -t form below must be accepted; a type `msg -t` omits is refused at
# the prompt with exit 2 without sending anything. The --raw checks exercise
# the same handler without going through the typed table.
check "view.opacity_rule is offerable with -t" "0" \
	"$(msg -t view.opacity_rule --id "$VID" --opacity 0.5 >/dev/null 2>&1; echo "$?")"
if msg --raw "{\"type\":\"view.tearing\",\"id\":$VID,\"tearing\":true}" >/dev/null 2>&1; then
	echo "ok: view.tearing (via --raw) was accepted"
else
	echo "FAIL: view.tearing (via --raw) was refused" >&2
	failures=$((failures + 1))
fi
check "view.tearing is offerable with -t" "0" \
	"$(msg -t view.tearing --id "$VID" --tearing true >/dev/null 2>&1; echo "$?")"
# The blur effect is render-only state too, with the same two observables:
# acceptance of both the raw and the typed form.
if msg --raw "{\"type\":\"view.blur\",\"id\":$VID,\"blur\":true}" >/dev/null 2>&1; then
	echo "ok: view.blur (via --raw) was accepted"
else
	echo "FAIL: view.blur (via --raw) was refused" >&2
	failures=$((failures + 1))
fi
check "view.blur is offerable with -t" "0" \
	"$(msg -t view.blur --id "$VID" --blur true >/dev/null 2>&1; echo "$?")"
if msg --raw "{\"type\":\"view.idle_inhibit\",\"id\":$VID,\"inhibit\":true}" >/dev/null 2>&1; then
	echo "ok: view.idle_inhibit (via --raw) was accepted"
else
	echo "FAIL: view.idle_inhibit (via --raw) was refused" >&2
	failures=$((failures + 1))
fi
check "view.idle_inhibit is offerable with -t" "0" \
	"$(msg -t view.idle_inhibit --id "$VID" --inhibit true >/dev/null 2>&1; echo "$?")"
# The decoration effect is render-only state with no socket answer of its
# own, so the observable is acceptance plus liveness, like the four above.
check "view.decorations is offerable with -t" "0" \
	"$(msg -t view.decorations --id "$VID" --decorate false >/dev/null 2>&1; echo "$?")"
if msg --raw "{\"type\":\"view.decorations\",\"id\":$VID}" >/dev/null 2>&1; then
	echo "ok: view.decorations (null) was accepted"
else
	echo "FAIL: view.decorations (null) was refused" >&2
	failures=$((failures + 1))
fi
check "and the window is still listed after all six" "$VID" \
	"$(view_id_for "viewport-view-ipc")"

# --- view.props, from an identity change ----------------------------------
SUB_MARK=$(wc -l <"$SUB_OUT")
kill -USR1 "$PAINT_PID" 2>/dev/null
for _ in $(seq 1 100); do
	if grep -q 'retitled' "$WORK/paint.log" 2>/dev/null; then
		break
	fi
	sleep 0.05
done
if wait_for "$SUB_OUT" "\"type\":\"view.props\"" 50; then
	echo "ok: a title change arrived as view.props"
else
	echo "FAIL: no view.props after the window retitled" >&2
	failures=$((failures + 1))
fi
# The props fields the shell reads.
PROPS=$(grep '"type":"view.props"' "$SUB_OUT" | tail -1)
check "view.props carries the new title" yes \
	"$(printf '%s' "$PROPS" | grep -q '"title":"viewport-capture-retitled"' && echo yes || echo no)"
check "and the window that changed" yes \
	"$(printf '%s' "$PROPS" | grep -q "\"id\":$VID" && echo yes || echo no)"
# `view.query` confirms the compositor saw it, so the props cannot have raced
# ahead of the change.
for _ in $(seq 1 100); do
	if msg -t view.query 2>/dev/null | grep -q 'viewport-capture-retitled'; then
		break
	fi
	sleep 0.05
done
check "and a fresh view.query agrees" yes \
	"$(msg -t view.query 2>/dev/null | grep -q 'viewport-capture-retitled' && echo yes || echo no)"

# --- view.configured, the clamp (foot, where it is installed) --------------
# A client configured below its minimum is entitled to ignore the configure,
# so the compositor raises it to the minimum and tells the shell once. That
# needs a client that advertises one; foot does (6x13). Without foot there is
# nothing to clamp and the check is skipped rather than failed.
if command -v foot >/dev/null 2>&1; then
	foot -e sleep 30 >"$WORK/foot.log" 2>&1 &
	FOOT_PID=$!
	for _ in $(seq 1 100); do
		if msg -t view.query 2>/dev/null | grep -q '"app_id":"foot"'; then
			break
		fi
		kill -0 "$FOOT_PID" 2>/dev/null || break
		sleep 0.1
	done
	FOOT_VID=$(view_id_for "foot")
	if [ -n "$FOOT_VID" ]; then
		SUB_MARK=$(wc -l <"$SUB_OUT")
		if msg -t view.layout --id "$FOOT_VID" --x 0 --y 0 --width 1 --height 1 >/dev/null 2>&1; then
			echo "ok: a below-minimum layout was accepted (clamped)"
		else
			echo "FAIL: a below-minimum layout was refused" >&2
			failures=$((failures + 1))
		fi
		if wait_for_both "$SUB_OUT" '"type":"view.configured"' "\"id\":$FOOT_VID" 50; then
			echo "ok: and it arrived as view.configured for that window"
			CONF=$(grep '"type":"view.configured"' "$SUB_OUT" | tail -1)
			check "with the raised minimum, not the requested size" yes \
				"$(printf '%s' "$CONF" | grep -q '"width":6' && printf '%s' "$CONF" | grep -q '"height":13' && echo yes || echo no)"
		else
			echo "FAIL: no view.configured for a below-minimum layout" >&2
			failures=$((failures + 1))
		fi
		if msg -t view.close --id "$FOOT_VID" >/dev/null 2>&1; then
			echo "ok: closing the foot window was accepted"
		else
			echo "FAIL: view.close for foot was refused" >&2
			failures=$((failures + 1))
		fi
		sleep 0.5
		kill "$FOOT_PID" 2>/dev/null
		FOOT_PID=
	else
		echo "SKIP: foot never mapped, so the view.configured clamp is unchecked"
	fi
else
	echo "SKIP: no foot for the view.configured clamp check"
fi

# --- input.key / input.scroll (synthetic input on the focused window) -------
# Plain `g` (34) has no binding on it, and a scroll is the common case, so
# neither touches a keybinding; the point is that both go through the same
# path a hand takes and the compositor stays up and responsive. Focus the
# paint window again first so the keys land on a client that ignores them
# rather than on the shell page.
msg -t view.focus --id "$VID" >/dev/null 2>&1
sleep 0.3
if msg -t input.key --keycode 34 --pressed true >/dev/null 2>&1 &&
	msg -t input.key --keycode 34 --pressed false >/dev/null 2>&1; then
	echo "ok: input.key press/release was accepted"
else
	echo "FAIL: input.key press/release was refused" >&2
	failures=$((failures + 1))
fi
if msg -t input.scroll --dy 1 >/dev/null 2>&1; then
	echo "ok: input.scroll was accepted"
else
	echo "FAIL: input.scroll was refused" >&2
	failures=$((failures + 1))
fi
# A scroll that names neither axis is a no-op (zero notches), not a refusal.
if msg -t input.scroll >/dev/null 2>&1; then
	echo "ok: an empty input.scroll is a no-op, not a refusal"
else
	echo "FAIL: an empty input.scroll was refused" >&2
	failures=$((failures + 1))
fi
if msg -t output.query >/dev/null 2>&1; then
	echo "ok: and the compositor is still answering after synthetic input"
else
	echo "FAIL: the compositor stopped answering after synthetic input" >&2
	failures=$((failures + 1))
fi

# --- nonexistent ids: silently ignored, by design -------------------------
# A race (a click landing as an application exits, a layout a frame stale)
# is not a mistake the user can do anything about, so every view message
# naming a window that does not exist is accepted and does nothing, rather
# than refused. What is checked is that acceptance plus liveness.
for args in "view.focus --id 999999" "view.close --id 999999" \
	"view.visible --id 999999 --visible false" \
	"view.fullscreen --id 999999 --fullscreen true" \
	"view.maximized --id 999999 --maximized true" \
	"view.minimized --id 999999 --minimized true" \
	"view.opacity --id 999999 --opacity 0.5" \
	"view.capture --id 999999 --capture false" \
	"view.layout --id 999999 --x 0 --y 0 --width 100 --height 100"; do
	# shellcheck disable=SC2086 # word splitting is the point
	if msg -t $args >/dev/null 2>&1; then
		echo "ok: $args was silently ignored (no such window)"
	else
		echo "FAIL: $args was refused for a nonexistent window" >&2
		failures=$((failures + 1))
	fi
done
check "and the real window is still listed" "$VID" \
	"$(view_id_for "viewport-view-ipc")"

# --- error paths: refused at the prompt, nothing sent ----------------------
# Parse-level failures are refused by `viewport msg` itself (exit 2, usage on
# stderr) before anything is sent. Handler-level refusals are exit 1. Both
# are checked by what the client reports, which is what a person at a prompt
# sees.
err() {
	msg "$@" >/dev/null 2>"$WORK/err.txt"
	local code=$?
	echo "$code $(cat "$WORK/err.txt" | tr '\n' ' ')"
}
check "a missing id is a usage error" yes \
	"$(err -t view.focus | grep -q "^2 .*missing field" && echo yes || echo no)"
check "an id of the wrong type names the field" yes \
	"$(err -t view.focus --id seven | grep -q "^2 .*view.focus: invalid type" && echo yes || echo no)"
check "a negative id is outside the wire type" yes \
	"$(err -t view.focus --id=-1 | grep -q "^2 .*outside" && echo yes || echo no)"
check "an id past u32 is outside the wire type" yes \
	"$(err -t view.focus --id 4294967297 | grep -q "^2 .*outside" && echo yes || echo no)"
check "a field the message does not have is refused, not dropped" yes \
	"$(err -t view.focus --bogus 1 | grep -q "^2 .*has no field" && echo yes || echo no)"
check "an unknown type over -t is a usage error" yes \
	"$(err -t view.teleport --id 1 | grep -q "^2 .*unknown message type" && echo yes || echo no)"
check "an unknown type over --raw is checked before it is sent" yes \
	"$(msg --raw '{"type":"view.teleport"}' >/dev/null 2>&1; [ $? -eq 2 ] && echo yes || echo no)"

# --- view.close ------------------------------------------------------------
# Last of this window's messages: after this there is no view to drive.
kill "$WATCH_PID" 2>/dev/null
WATCH_PID=
if msg -t view.close --id "$VID" >/dev/null 2>&1; then
	echo "ok: view.close was accepted"
else
	echo "FAIL: view.close was refused" >&2
	failures=$((failures + 1))
fi
if wait_for_both "$SUB_OUT" '"type":"view.removed"' "\"id\":$VID" 50; then
	echo "ok: and it arrived as a view.removed for that window"
else
	echo "FAIL: no view.removed after view.close" >&2
	failures=$((failures + 1))
fi
for _ in $(seq 1 50); do
	kill -0 "$PAINT_PID" 2>/dev/null || break
	sleep 0.1
done
check "and the client closed" no \
	"$(kill -0 "$PAINT_PID" 2>/dev/null && echo yes || echo no)"
kill "$SUB_PID" 2>/dev/null
wait "$SUB_PID" 2>/dev/null
SUB_PID=

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed; the compositor's log:" >&2
	tail -30 "$LOG" >&2
	exit 1
fi
echo "all view-ipc checks passed"
