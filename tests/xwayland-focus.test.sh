#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# An X11 window is given the input focus, not left at PointerRoot.
#
# Starts the compositor headless, waits for it to bring up Xwayland, and has an
# X client map a window and ask the server who holds the keyboard. See
# tests/xwayland-focus-client.c for why the answer matters more than it looks.
#
# Skips rather than fails when there is no libX11 to compile the client with:
# the rest of the integration suite deliberately depends on nothing but
# wayland-client, and this one test is not worth making X11 a requirement for
# running any of it.
#
#   tests/xwayland-focus.test.sh build/viewport
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

# The compositor spawns the server; with no binary on PATH it says so and
# carries on serving Wayland clients, which leaves nothing here to test.
if ! command -v Xwayland >/dev/null 2>&1; then
	echo "SKIP: no Xwayland for the compositor to spawn"
	exit 77
fi

root=$(cd "$(dirname "$0")/.." && pwd)
workdir=$(mktemp -d)
viewport_pid=

cleanup() {
	if [ -n "$viewport_pid" ] && kill -0 "$viewport_pid" 2>/dev/null; then
		kill "$viewport_pid" 2>/dev/null
		wait "$viewport_pid" 2>/dev/null
	fi
	rm -rf "$workdir"
}
trap cleanup EXIT

client=$workdir/focus-client
# shellcheck disable=SC2046 # pkg-config output is a word list on purpose
if ! cc "$root/tests/xwayland-focus-client.c" -o "$client" $(pkg-config --cflags --libs x11); then
	echo "SKIP: the X client would not compile"
	exit 77
fi

log=$workdir/viewport.log
"$viewport" --headless --width 1920 --height 1080 >"$log" 2>&1 &
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

if DISPLAY=$display "$client"; then
	echo "PASS: the X11 window holds the input focus"
	exit 0
fi

echo "FAIL: the compositor left the X server's focus alone" >&2
echo "      A window manager has to call SetInputFocus; nothing else will." >&2
exit 1
