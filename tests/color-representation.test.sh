#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# wp-color-representation-v1, from registry advertisement to the commit-time
# format check. Each mode of the client asserts one behaviour and exits 0
# when the compositor behaved; a mode whose correct behaviour is a fatal
# protocol error passes when the connection dies, and fails when it does not.
set -u

viewport=${1:-build/viewport}
representation_client=${2:-build/viewport-test-color-representation-client}

for binary in "$viewport" "$representation_client"; do
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
for mode in advertise declare bad-combination bad-siting rgb-mismatch; do
	if ! "$representation_client" "$mode" >"$workdir/$mode.log" 2>&1; then
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
