#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# A copy made in a Wayland client is visible to X11 applications.
#
# The clipboard is one selection spoken by two protocols: Wayland clients take
# it through `wl_data_device`, X11 clients through the X server, and Xwayland is
# the bridge — but only once the compositor has told it what the Wayland side is
# offering and that it now owns the X11 selection. Nothing else takes the X11
# CLIPBOARD on its behalf. Without that, a copy made in a Wayland client leaves
# the X11 clipboard owned by nobody, and pasting into Steam, a browser or an X
# terminal reads nothing at all.
#
# The X client here is the one that matters: it is the side that used to be
# answered by nobody. `wl-paste` is checked too, because a fix that broke the
# Wayland half while adding the X11 one would pass the first check alone.
#
# Skips rather than fails where there is no libX11 or no Xwayland, exactly as
# tests/xwayland-focus.test.sh does: the suite is deliberately buildable
# without X11 and this is not the test to make it a requirement.
#
#   tests/xwayland-clipboard.test.sh build/viewport
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
if ! command -v wl-copy >/dev/null 2>&1 || ! command -v wl-paste >/dev/null 2>&1; then
	echo "SKIP: no wl-clipboard to offer a selection with"
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

client=$workdir/clipboard-client
# shellcheck disable=SC2046 # pkg-config output is a word list on purpose
if ! cc "$root/tests/xwayland-clipboard-client.c" -o "$client" $(pkg-config --cflags --libs x11); then
	echo "SKIP: the X client would not compile"
	exit 77
fi

export XDG_RUNTIME_DIR="$workdir/runtime"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

log=$workdir/viewport.log
"$viewport" --headless --width 1920 --height 1080 >"$log" 2>&1 &
viewport_pid=$!

display=
wayland_display=
for _ in $(seq 1 100); do
	if ! kill -0 "$viewport_pid" 2>/dev/null; then
		echo "the compositor exited before Xwayland was up" >&2
		cat "$log" >&2
		exit 1
	fi
	display=$(sed -n 's/.*Xwayland ready on \(:[0-9]*\).*/\1/p' "$log" | head -1)
	wayland_display=$(sed -n 's/.*WAYLAND_DISPLAY=\([A-Za-z0-9_-]*\).*/\1/p' "$log" | head -1)
	if [ -n "$display" ] && [ -n "$wayland_display" ]; then
		break
	fi
	sleep 0.2
done
if [ -z "$display" ] || [ -z "$wayland_display" ]; then
	echo "the compositor never named an X display and a Wayland one" >&2
	cat "$log" >&2
	exit 1
fi

export WAYLAND_DISPLAY="$wayland_display"
printf 'copied in a wayland client' | wl-copy

# Watched rather than assumed: `wl-copy` forks and the child is the owner, so
# the selection exists a moment after the command returns.
pasted=
for _ in $(seq 1 50); do
	pasted=$(timeout 5 wl-paste --no-newline 2>/dev/null)
	[ -n "$pasted" ] && break
	sleep 0.1
done

failures=0
check() {
	local what=$1 expected=$2 got=$3
	if [ "$expected" = "$got" ]; then
		echo "ok: $what"
	else
		echo "FAIL: $what — wanted '$expected', got '$got'" >&2
		failures=$((failures + 1))
	fi
}

check "a Wayland client's copy is what Wayland clients paste" \
	"copied in a wayland client" "$pasted"

x_pasted=$(DISPLAY="$display" timeout 30 "$client" 2>"$workdir/client.err")
x_status=$?
if [ "$x_status" -ne 0 ]; then
	echo "FAIL: the X11 client could not paste — $(cat "$workdir/client.err")" >&2
	failures=$((failures + 1))
else
	check "and what X11 clients paste" "copied in a wayland client" "$x_pasted"
fi

if [ "$failures" -eq 0 ]; then
	echo "PASS: the clipboard crosses into X11"
	exit 0
fi
exit 1
