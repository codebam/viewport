#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# An X11 title change reaches the shell as a `view.props`.
#
# The float test proves a window is announced with the name it had at map;
# this one proves the announcement is not the last word. `property_notify`
# used to look at `_NET_WM_STATE` alone, so a title an X client changed after
# mapping stopped at the X server: titlebars and taskbars froze at the map
# title. The fix routes `Title` (and `Class`) through the same `notify_props`
# an xdg-shell client's change takes, and the client here writes both
# spellings — `WM_NAME` and `_NET_WM_NAME` — because a compositor reading
# only one of them is the same frozen titlebar by another door.
#
# The subscriber is attached before the retitle and the client announces its
# first title a second before changing it, so the event this test waits for
# cannot be a replay of the announcement: `view.props` arrives with "After
# Retitle" or the routing is broken, whatever the map title said.
#
# Skips rather than fails where there is no libX11 or no Xwayland, exactly as
# tests/xwayland-float.test.sh does: the suite is deliberately buildable
# without X11 and this is not the test to make it a requirement.
#
#   tests/xwayland-props.test.sh build/viewport
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
subscriber_pid=

cleanup() {
	for pid in "$client_pid" "$subscriber_pid" "$viewport_pid"; do
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

client=$workdir/props-client
# shellcheck disable=SC2046 # pkg-config output is a word list on purpose
if ! cc "$root/tests/xwayland-props-client.c" -o "$client" \
	$(pkg-config --cflags --libs x11); then
	echo "SKIP: the X client would not compile"
	exit 77
fi

log=$workdir/viewport.log
socket=$workdir/control.sock
sub=$workdir/sub.jsonl
"$viewport" --headless --socket "$socket" >"$log" 2>&1 &
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

# The window, announced with the title it had at map.
DISPLAY=$display "$client" >"$workdir/client.log" 2>&1 &
client_pid=$!
for _ in $(seq 1 100); do
	grep -q '^mapped' "$workdir/client.log" 2>/dev/null && break
	kill -0 "$client_pid" 2>/dev/null || break
	sleep 0.1
done

# The view id carrying the map title. Key order on the wire is not part of
# the contract, so this reads by name rather than by position, as
# tests/shell-session-ipc.test.sh does.
id=
for _ in $(seq 1 100); do
	id=$(msg -t view.query 2>/dev/null |
		grep -F '"title":"Before Retitle"' |
		sed -n 's/.*"id":\([0-9]*\).*/\1/p' | head -1)
	[ -n "$id" ] && break
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$id" ]; then
	echo "FAIL: no window was announced as \"Before Retitle\"" >&2
	echo "--- client ---" >&2
	cat "$workdir/client.log" >&2
	echo "--- compositor (last 30) ---" >&2
	tail -30 "$log" >&2
	exit 1
fi
echo "ok: the window is up as view $id, \"Before Retitle\""

# Subscribed before the retitle: an event that has already gone out is not
# replayed to a subscriber that arrived late, so what lands here can only be
# the change itself.
"$viewport" msg --socket "$socket" --timeout 0 -t subscribe view.props \
	>"$sub" 2>&1 &
subscriber_pid=$!
sleep 0.3

# The client's own cue — the test waits for the retitle to have been written
# before it starts waiting on the compositor, so a slow compositor and a fast
# client do not trade places.
for _ in $(seq 1 150); do
	grep -q '^retitled' "$workdir/client.log" 2>/dev/null && break
	kill -0 "$client_pid" 2>/dev/null || break
	sleep 0.1
done

# The change, on the wire, within 10s of the client writing it.
# Key order on the wire is not part of the contract, so this greps the two
# facts of the same line rather than one line-shaped pattern.
got=
for _ in $(seq 1 100); do
	if grep '"After Retitle"' "$sub" 2>/dev/null | grep -q '"type":"view.props"'; then
		got=yes
		break
	fi
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done
check "a retitle arrives as view.props with the new title" "${got:-no}"

# The same line must name the window: a props event about some other view
# would satisfy a grep for the title alone.
line=$(grep '"After Retitle"' "$sub" 2>/dev/null | grep '"type":"view.props"' | head -1)
check "and it names the window it belongs to" \
	"$(printf '%s' "$line" | grep -q "\"id\":$id" && echo yes || echo no)"

if [ "$status" -ne 0 ]; then
	echo "--- subscriber ---" >&2
	cat "$sub" >&2
	echo "--- client ---" >&2
	cat "$workdir/client.log" >&2
	echo "--- compositor (last 30) ---" >&2
	tail -30 "$log" >&2
fi

kill "$client_pid" 2>/dev/null

exit "$status"
