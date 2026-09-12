#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# A fixed-size X11 window is announced as floating; a resizable one is not.
#
# Steam's updater is the window that made this concrete. It advertises no
# dialog type and no parent, so the only thing it says about itself is that it
# is one size and will not be resized — and the compositor, reading xdg-only
# size state and only reaching the check below an xdg-only early return,
# answered "no opinion" and tiled it. See views.rs, `wants_floating`.
#
# The two windows are the same in every other respect, so what the test measures
# is the size hint and nothing else. Both are X11 clients with `WM_CLASS`
# `steam`/`Steam` and `_NET_WM_WINDOW_TYPE_NORMAL`.
#
# The client paints once, at map, and then only serves events — deliberately. A
# window that keeps painting announces itself on a later commit whatever the
# compositor did with the first one; this shape is the one that appears only if
# pairing the X window with its `wl_surface` announces what the early commit
# could not (see `surface_associated`). Run it on a loaded machine: the ordering
# it turns on is a race, and an idle one hides it.
#
# Skips rather than fails where there is no libX11 or no Xwayland, exactly as
# tests/xwayland-focus.test.sh does: the suite is deliberately buildable
# without X11 and this is not the test to make it a requirement.
#
#   tests/xwayland-float.test.sh build/viewport
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

client=$workdir/float-client
# shellcheck disable=SC2046 # pkg-config output is a word list on purpose
if ! cc "$root/tests/xwayland-float-client.c" -o "$client" \
	$(pkg-config --cflags --libs x11); then
	echo "SKIP: the X client would not compile"
	exit 77
fi

log=$workdir/viewport.log
socket=$workdir/control.sock
events=$workdir/events.json
"$viewport" --headless --socket "$socket" >"$log" 2>&1 &
viewport_pid=$!

# Xwayland is started by the compositor and says so when it is ready; the
# display number it picked is in that line and is not predictable.
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

# Subscribed before the first client maps, because an announcement that has
# already gone out is not replayed to a subscriber that arrived late.
"$viewport" msg --socket "$socket" --timeout 0 -t subscribe view.added \
	>"$events" 2>&1 &
subscriber_pid=$!
sleep 0.5

status=0

# The value of one key on the first `view.added` line naming a title. Key order
# on the wire is not part of the contract, so this reads by name rather than by
# position.
field() {
	grep -F "\"title\":\"$1\"" "$events" | head -1 \
		| grep -o "\"$2\":[^,}]*" | head -1 | cut -d: -f2
}

# Waits for the announcement of one title and reports whether it arrived.
await() {
	for _ in $(seq 1 100); do
		grep -qF "\"title\":\"$1\"" "$events" && return 0
		kill -0 "$viewport_pid" 2>/dev/null || return 1
		sleep 0.2
	done
	return 1
}

check() {
	local what=$1 expected=$2 got=$3
	if [ "$expected" = "$got" ]; then
		echo "ok: $what"
	else
		echo "FAIL: $what — wanted $expected, got $got" >&2
		status=1
	fi
}

# --- the fixed-size window: the updater's shape ---------------------------
DISPLAY=$display "$client" fixed >"$workdir/fixed.log" 2>&1 &
client_pid=$!
for _ in $(seq 1 100); do
	grep -q '^mapped ' "$workdir/fixed.log" 2>/dev/null && break
	kill -0 "$client_pid" 2>/dev/null || break
	sleep 0.1
done

if ! await "Steam - Self Updater"; then
	echo "FAIL: the fixed-size window was never announced" >&2
	echo "--- events ---" >&2
	cat "$events" >&2
	echo "--- client ---" >&2
	cat "$workdir/fixed.log" >&2
	echo "--- compositor (last 30) ---" >&2
	tail -30 "$log" >&2
	exit 1
fi

check "a fixed-size X11 window floats" true \
	"$(field 'Steam - Self Updater' floating)"
check "and is opened at its own width" 320 \
	"$(field 'Steam - Self Updater' width)"
check "and at its own height" 140 \
	"$(field 'Steam - Self Updater' height)"

# The minimum is what stops the shell shrinking it below the one size it
# accepts, so a floating rectangle is not all that has to be right.
check "and reports its minimum width" 320 \
	"$(field 'Steam - Self Updater' min_width)"
check "and reports its minimum height" 140 \
	"$(field 'Steam - Self Updater' min_height)"

kill "$client_pid" 2>/dev/null
wait "$client_pid" 2>/dev/null
client_pid=

# --- the same window, resizable -------------------------------------------
# The control. Nothing about this window differs but the maximum size, so a
# compositor that floated every X11 window would pass the first half and fail
# here.
DISPLAY=$display "$client" resizable >"$workdir/resizable.log" 2>&1 &
client_pid=$!
for _ in $(seq 1 100); do
	grep -q '^mapped ' "$workdir/resizable.log" 2>/dev/null && break
	kill -0 "$client_pid" 2>/dev/null || break
	sleep 0.1
done

if ! await "Steam - Resizable"; then
	echo "FAIL: the resizable window was never announced" >&2
	echo "--- events ---" >&2
	cat "$events" >&2
	echo "--- compositor (last 30) ---" >&2
	tail -30 "$log" >&2
	exit 1
fi

check "a resizable X11 window is still tiled" false \
	"$(field 'Steam - Resizable' floating)"

if [ "$status" -ne 0 ]; then
	echo "--- events ---" >&2
	cat "$events" >&2
fi

exit "$status"
