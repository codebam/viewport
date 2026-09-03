#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# ext-background-effect-v1, from registry advertisement to captured GLES pixels.
set -u

viewport=${1:-build/viewport}
effect_client=${2:-build/viewport-test-background-effect-client}
capture_client=${3:-build/viewport-test-capture-client}

for binary in "$viewport" "$effect_client" "$capture_client"; do
	if [ ! -x "$binary" ]; then
		echo "missing $binary - build first" >&2
		exit 2
	fi
done

workdir=$(mktemp -d)
viewport_pid=
effect_pid=
cleanup() {
	[ -n "$effect_pid" ] && kill "$effect_pid" 2>/dev/null
	[ -n "$viewport_pid" ] && kill "$viewport_pid" 2>/dev/null
	wait 2>/dev/null
	rm -rf "$workdir"
}
trap cleanup EXIT INT TERM

mkdir "$workdir/runtime"
chmod 700 "$workdir/runtime"
export XDG_RUNTIME_DIR="$workdir/runtime"
unset WAYLAND_DISPLAY

printf '{ "layout": "tiling" }\n' >"$workdir/config.json"
"$viewport" --headless --width 640 --height 480 \
	--config "$workdir/config.json" >"$workdir/viewport.log" 2>&1 &
viewport_pid=$!

display=
for _ in $(seq 1 100); do
	display=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' \
		"$workdir/viewport.log" | head -1 | cut -d= -f2)
	[ -n "$display" ] && break
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$display" ]; then
	echo "compositor did not start" >&2
	tail -30 "$workdir/viewport.log" >&2
	exit 2
fi
export WAYLAND_DISPLAY="$display"

"$effect_client" >"$workdir/effect.log" 2>&1 &
effect_pid=$!
ready=
for _ in $(seq 1 100); do
	if grep -q '^ready$' "$workdir/effect.log" 2>/dev/null; then
		ready=yes
		break
	fi
	kill -0 "$effect_pid" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$ready" ]; then
	echo "background-effect client did not become ready" >&2
	tail -20 "$workdir/effect.log" >&2
	tail -30 "$workdir/viewport.log" >&2
	exit 2
fi

status=0
# Sharp red outside the requested region.
"$capture_client" --output-pixel 32 128 ffff0000 || status=1
# The hard red/blue edge is mixed only where blur was requested.
"$capture_client" --output-pixel-mixed 188 128 || status=1
# The subtracted hole remains the sharp blue backdrop.
"$capture_client" --output-pixel 320 192 ff0000ff || status=1

if [ "$status" -ne 0 ]; then
	echo "--- background-effect client ---" >&2
	tail -20 "$workdir/effect.log" >&2
	echo "--- compositor ---" >&2
	tail -40 "$workdir/viewport.log" >&2
fi
exit "$status"
