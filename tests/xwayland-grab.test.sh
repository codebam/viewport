#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# A keyboard grab is the focused X11 window's, and nobody else's.
#
# Two X clients, one compositor (see tests/xwayland-grab-client.c for the two
# shapes). The background window grabs while holding nothing: a grant without
# the focused-window rule used to answer it — Smithay's grab ignores
# compositor focus changes while it lives, so the seat belonged to a window
# the user never meant to have it until the client relented. The target
# window is focused first and grabs: its grant must be the X11-shaped focus
# the seat already holds, because the wrong shape made the first key fire a
# FocusOut on the window that had just asked to hold every key.
#
# The key is synthetic (`input.key` on the focused window) — exactly the path
# a person's keyboard takes once the compositor has it.
#
# Skips rather than fails where there is no libX11 or no Xwayland, exactly as
# tests/xwayland-float.test.sh does: the suite is deliberately buildable
# without X11 and this is not the test to make it a requirement.
#
#   tests/xwayland-grab.test.sh build/viewport
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
target_pid=
background_pid=
subscriber_pid=

cleanup() {
	for pid in "$target_pid" "$background_pid" "$subscriber_pid" "$viewport_pid"; do
		[ -n "$pid" ] && kill "$pid" 2>/dev/null
	done
	wait 2>/dev/null
	rm -rf "$workdir"
}
trap cleanup EXIT

# A private runtime dir: this starts its own compositor and must not join,
# or be joined to, the one the developer is sitting in — and a private config
# dir, so machine-specific settings (an idle lock_command, rules) never
# change what the test measures.
export XDG_RUNTIME_DIR="$workdir/run"
export XDG_STATE_HOME="$workdir/state"
export XDG_CONFIG_HOME="$workdir/config"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
unset WAYLAND_DISPLAY

client=$workdir/grab-client
# shellcheck disable=SC2046 # pkg-config output is a word list on purpose
if ! cc "$root/tests/xwayland-grab-client.c" -o "$client" \
	$(pkg-config --cflags --libs x11); then
	echo "SKIP: the X client would not compile"
	exit 77
fi

log=$workdir/viewport.log
socket=$workdir/control.sock
config=$workdir/config.json
focused=$workdir/focused.jsonl
printf '{ "layout": "tiling", "tray": false }\n' >"$config"
"$viewport" --headless --socket "$socket" --config "$config" >"$log" 2>&1 &
viewport_pid=$!

display=
for _ in $(seq 1 100); do
	if ! kill -0 "$viewport_pid" 2>/dev/null; then
		echo "the compositor exited before Xwayland was up" >&2
		cat "$log" >&2
		exit 1
	fi
	display=$(sed -n 's/.*Xwayland ready on \(:[0-9]*\).*/\1/p' "$log" | head -1)
	if [ -n "$display" ]; then
		break
	fi
	sleep 0.2
done

if [ -z "$display" ]; then
	echo "Xwayland never came up" >&2
	cat "$log" >&2
	exit 1
fi

# The control socket the compositor logs once it is listening; the display
# number above is Xwayland's, not this one's.
for _ in $(seq 1 100); do
	socket=$(sed -n 's/.*control socket at \(.*\)$/\1/p' "$log" | head -1)
	[ -n "$socket" ] && break
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$socket" ]; then
	echo "the compositor never published a control socket" >&2
	cat "$log" >&2
	exit 1
fi

msg() {
	"$viewport" msg --socket "$socket" "$@"
}

status=0
check() {
	local what=$1 ok=$2
	if [ "$ok" = "yes" ]; then
		echo "ok: $what"
	else
		echo "FAIL: $what" >&2
		status=1
	fi
}

# Waits for a line in a file without a fixed wait length on any one of them.
await_line() {
	local file=$1 needle=$2 timeout=${3:-100}
	for _ in $(seq 1 "$timeout"); do
		grep -q -- "$needle" "$file" 2>/dev/null && return 0
		sleep 0.1
	done
	return 1
}

# The view id of one title, by name rather than by position.
view_id() {
	msg -t view.query 2>/dev/null |
		grep -F "\"title\":\"$1\"" |
		sed -n 's/.*"id":\([0-9]*\).*/\1/p' | head -1
}

await_view() {
	for _ in $(seq 1 100); do
		[ -n "$(view_id "$1")" ] && return 0
		kill -0 "$viewport_pid" 2>/dev/null || return 1
		sleep 0.1
	done
	return 1
}

# --- the target window: focused, then grabbing ------------------------------
DISPLAY=$display "$client" target >"$workdir/target.log" 2>&1 &
target_pid=$!
await_line "$workdir/target.log" '^mapped' || {
	echo "FAIL: the target window never mapped" >&2
	cat "$workdir/target.log" >&2
	exit 1
}
id=$(await_view "Grab Target" && view_id "Grab Target")
if [ -z "$id" ]; then
	echo "FAIL: no window was announced as \"Grab Target\"" >&2
	echo "--- compositor (last 30) ---" >&2
	tail -30 "$log" >&2
	exit 1
fi
echo "ok: the target window is up as view $id"

# Subscribed before anything is focused, so the event below can only be the
# focus this test asked for.
"$viewport" msg --socket "$socket" --timeout 0 -t subscribe view.focused \
	>"$focused" 2>&1 &
subscriber_pid=$!
sleep 0.3

msg -t view.focus --id "$id"
if await_line "$focused" "\"id\":$id" 100; then
	echo "ok: the target window holds the focus"
else
	echo "FAIL: the focus never landed on the target window" >&2
	cat "$focused" >&2
	exit 1
fi

# The grab, granted to the window that already holds the keys.
grab=
for _ in $(seq 1 100); do
	grab=$(sed -n 's/^grab=\([0-9]*\)$/\1/p' "$workdir/target.log" | head -1)
	[ -n "$grab" ] && break
	kill -0 "$target_pid" 2>/dev/null || break
	sleep 0.1
done
check "the focused window's grab succeeds" \
	"$( [ "${grab:-1}" = 0 ] && echo yes || echo no)"

# --- the background window: never focused, grabbing anyway ------------------
DISPLAY=$display "$client" background >"$workdir/background.log" 2>&1 &
background_pid=$!
await_line "$workdir/background.log" '^mapped' || {
	echo "FAIL: the background window never mapped" >&2
	cat "$workdir/background.log" >&2
	exit 1
}
bg_id=$(await_view "Background Grabber" && view_id "Background Grabber")
[ -n "$bg_id" ] || {
	echo "FAIL: no window was announced as \"Background Grabber\"" >&2
	exit 1
}
await_line "$workdir/background.log" '^grab=' 100
bg_grab=$(sed -n 's/^grab=\([0-9]*\)$/\1/p' "$workdir/background.log" | head -1)
check "a grab from a window that was never focused is refused" \
	"$( [ -n "$bg_grab" ] && [ "$bg_grab" != 0 ] && echo yes || echo no )"

# Nothing about the refusal may have handed the seat over: no focus event
# for a window the test never focused. One second is longer than the ask
# takes to be answered either way.
sleep 1
check "and no focus event names the background window" \
	"$( grep "\"id\":$bg_id" "$focused" | grep -q '"type":"view.focused"' && echo no || echo yes )"

# --- the keys, still on the focused window ---------------------------------
# One press and release of a key with no binding on it, exactly as
# tests/view-ipc.test.sh sends one.
msg -t input.key --keycode 34 --pressed true
msg -t input.key --keycode 34 --pressed false
keypress=
for _ in $(seq 1 50); do
	if grep -q '^keypress' "$workdir/target.log" 2>/dev/null; then
		keypress=yes
		break
	fi
	kill -0 "$target_pid" 2>/dev/null || break
	sleep 0.1
done
check "the grabbed window receives a synthetic key" "${keypress:-no}"
# The F5 regression: the first key used to fire a leave on the window that
# had just grabbed, so the keypress lines here always follow a focusout.
check "and no focusout preceded the key" \
	"$( grep -q '^focusout' "$workdir/target.log" && echo no || echo yes )"

if [ "$status" -ne 0 ]; then
	echo "--- target ---" >&2
	cat "$workdir/target.log" >&2
	echo "--- background ---" >&2
	cat "$workdir/background.log" >&2
	echo "--- focused ---" >&2
	cat "$focused" >&2
	echo "--- compositor (last 30) ---" >&2
	tail -30 "$log" >&2
fi

kill "$target_pid" "$background_pid" 2>/dev/null

exit "$status"
