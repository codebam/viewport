#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# A fullscreen request that is a property write is answered. Wine — and
# FINAL FANTASY XVI under it — sets `_NET_WM_STATE_FULLSCREEN` by writing the
# property after the map, not by sending the EWMH `ClientMessage`; the
# property is the WM's, so nothing read it back and the window stayed in its
# tile. The compositor has to hear the write and publish the window as
# fullscreen the same way it does for the `ClientMessage` shape, which is what
# this watches for: a `wlr-foreign-toplevel-management` client, started before
# anything maps, prints the toplevel's state once per change — see its `watch`
# mode — and `fullscreen=1` after the write is the answer.
#
# The `post` phase is the control: that shape worked before the property was
# ever read back, so a compositor that answers neither fails there. The `prop`
# phase additionally requires the state to have been published as `fullscreen=0`
# first, so a window that simply mapped fullscreen cannot pass it.
#
#   tests/xwayland-fullscreen.test.sh build/viewport path/to/foreign-toplevel-client
set -uo pipefail

viewport=${1:?usage: xwayland-fullscreen.test.sh VIEWPORT FT_CLIENT}
ft_client=${2:?}

if [ ! -x "$viewport" ]; then
	echo "missing $viewport — build first" >&2
	exit 2
fi

if [ ! -x "$ft_client" ]; then
	echo "missing $ft_client — build the test clients first (scripts/integration.sh does)" >&2
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
watcher_pid=
client_pid=

cleanup() {
	for pid in "$client_pid" "$watcher_pid" "$viewport_pid"; do
		[ -n "$pid" ] && kill "$pid" 2>/dev/null
	done
	wait 2>/dev/null
	rm -rf "$workdir"
}
trap cleanup EXIT

client=$workdir/fullscreen-client
# shellcheck disable=SC2046 # pkg-config output is a word list on purpose
if ! cc "$root/tests/xwayland-fullscreen-client.c" -o "$client" \
	$(pkg-config --cflags --libs x11); then
	echo "SKIP: the X client would not compile"
	exit 77
fi

# Off whatever session is already running: this starts its own compositor and
# must not join, or be joined to, the one the developer is sitting in.
unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"

status=0

# One phase: a compositor of its own, the watcher started before anything
# maps, and the X client in $1's shape. The watcher must report
# `fullscreen=1` after the client's request; everything is torn down before
# the next phase starts its own.
run_case() {
	local mode=$1
	local log=$workdir/viewport-$mode.log
	local watch=$workdir/watch-$mode.log
	local clog=$workdir/client-$mode.log
	local display= wd=

	"$viewport" --headless >"$log" 2>&1 &
	viewport_pid=$!
	for _ in $(seq 1 100); do
		kill -0 "$viewport_pid" 2>/dev/null || break
		wd=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$log" | head -1 | cut -d= -f2)
		display=$(sed -n 's/.*Xwayland ready on \(:[0-9]*\).*/\1/p' "$log" | head -1)
		[ -n "$wd" ] && [ -n "$display" ] && break
		sleep 0.2
	done
	if [ -z "$wd" ] || [ -z "$display" ]; then
		echo "FAIL [$mode]: the compositor never came up (wayland=$wd x11=$display)" >&2
		tail -20 "$log" >&2
		return 1
	fi

	WAYLAND_DISPLAY=$wd "$ft_client" watch >"$watch" 2>&1 &
	watcher_pid=$!
	# The watcher prints nothing until a toplevel exists; the bind only needs
	# a moment before the window maps, which is the wait below.
	sleep 0.5

	DISPLAY=$display "$client" "$mode" >"$clog" 2>&1 &
	client_pid=$!
	for _ in $(seq 1 100); do
		grep -q '^mapped ' "$clog" 2>/dev/null && break
		kill -0 "$client_pid" 2>/dev/null || break
		sleep 0.1
	done

	# The prop client writes three seconds after it paints; twelve is
	# generous on a loaded machine, and a compositor that never answers
	# fills the window and fails.
	local saw=0
	for _ in $(seq 1 120); do
		if grep -q 'fullscreen=1' "$watch" 2>/dev/null; then
			saw=1
			break
		fi
		kill -0 "$viewport_pid" 2>/dev/null || break
		sleep 0.1
	done

	local ok=0
	if [ "$saw" != 1 ]; then
		echo "FAIL [$mode]: fullscreen was never published" >&2
		echo "--- watcher ---" >&2
		cat "$watch" >&2
		echo "--- client ---" >&2
		cat "$clog" >&2
		echo "--- compositor (last 30) ---" >&2
		tail -30 "$log" >&2
	elif [ "$mode" = prop ] \
		&& ! awk '/fullscreen=0/{z=NR} /fullscreen=1/{o=NR} END{exit !(z && o && z < o)}' "$watch"; then
		echo "FAIL [$mode]: fullscreen=1 arrived without a fullscreen=0 state before it" >&2
		cat "$watch" >&2
	else
		echo "ok   [$mode]: the request was answered and published"
		sed 's/^/     /' "$watch"
		ok=1
	fi

	kill "$client_pid" "$watcher_pid" "$viewport_pid" 2>/dev/null
	wait 2>/dev/null
	client_pid=
	watcher_pid=
	viewport_pid=
	[ "$ok" = 1 ]
}

run_case prop || status=1
run_case post || status=1

exit "$status"
