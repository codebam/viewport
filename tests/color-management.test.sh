#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# wp-color-management-v1, from the advertisement a game picks its colours from
# to the two well-known Windows descriptions and the error that guards them.
#
# The shapes here are Wine's, because Wine is what Proton runs: it binds the
# global at whatever version it is offered, reads `windows_scrgb` as the answer
# to "can this display do HDR at all", and attaches `windows_bt2100` to an
# HDR10 swapchain. A mode whose correct behaviour is a fatal protocol error
# passes when the connection dies, and fails when it does not.
set -u

viewport=${1:-build/viewport}
client=${2:-build/viewport-test-color-management-client}

for binary in "$viewport" "$client"; do
	if [ ! -x "$binary" ]; then
		echo "missing $binary - build first" >&2
		exit 2
	fi
done

workdir=$(mktemp -d)
viewport_pid=
cleanup() {
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

status=0
# advertise first: every later mode depends on what it pins down, so a failure
# there should be readable as the cause rather than as four unrelated ones.
for mode in advertise windows-scrgb windows-bt2100 parametric no-information; do
	if ! "$client" "$mode" >"$workdir/$mode.log" 2>&1; then
		echo "$mode failed" >&2
		tail -20 "$workdir/$mode.log" >&2
		status=1
	fi
done

if [ "$status" -ne 0 ]; then
	echo "--- compositor ---" >&2
	tail -40 "$workdir/viewport.log" >&2
fi

# Every scenario that ended in a fatal protocol error killed only its own
# client; the compositor is still here, and still serving the next one.
kill -0 "$viewport_pid" 2>/dev/null || {
	echo "compositor died along with its client" >&2
	status=1
}

exit "$status"
