#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# `xwayland.scale` reaches the X server and the X clients on it.
#
# Runs the compositor headless at a known logical size with the key set to 2,
# and has an X client check the two halves the setting is made of: an X screen
# twice the logical desk, and XSETTINGS telling the toolkits to spend the
# extra pixels. See tests/xwayland-scale-client.c for why either half alone is
# a bug rather than half a feature.
#
# No HiDPI monitor is involved and none is needed — that is the point of the
# key taking a number as well as "auto". What this cannot check is what the
# applications then do with it, which is toolkit by toolkit and is written
# down in docs/protocols.md rather than tested here.
#
#   tests/xwayland-scale.test.sh build/viewport
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

cleanup() {
	if [ -n "$viewport_pid" ] && kill -0 "$viewport_pid" 2>/dev/null; then
		kill "$viewport_pid" 2>/dev/null
		wait "$viewport_pid" 2>/dev/null
	fi
	rm -rf "$workdir"
}
trap cleanup EXIT

client=$workdir/scale-client
# shellcheck disable=SC2046 # pkg-config output is a word list on purpose
if ! cc "$root/tests/xwayland-scale-client.c" -o "$client" $(pkg-config --cflags --libs x11); then
	echo "SKIP: the X client would not compile"
	exit 77
fi

width=1600
height=900

# Runs the compositor once and checks the X server it brought up. `config` is
# a file to pass, or empty for none — a session with no config file at all is
# the case that has to be unchanged.
check_scale() {
	local scale=$1 config=$2 log status display

	log=$workdir/viewport-$scale.log
	if [ -n "$config" ]; then
		"$viewport" --headless --width "$width" --height "$height" --config "$config" \
			>"$log" 2>&1 &
	else
		"$viewport" --headless --width "$width" --height "$height" >"$log" 2>&1 &
	fi
	viewport_pid=$!

	display=
	for _ in $(seq 1 100); do
		if ! kill -0 "$viewport_pid" 2>/dev/null; then
			echo "the compositor exited before Xwayland was up" >&2
			cat "$log" >&2
			return 1
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
		return 1
	fi

	# The X settings are published on the same turn of the loop that starts
	# the window manager, but the selection owner has to reach the server
	# before a client can read it back.
	status=1
	for _ in $(seq 1 25); do
		if DISPLAY=$display "$client" "$scale" "$width" "$height"; then
			status=0
			break
		fi
		sleep 0.2
	done

	kill "$viewport_pid" 2>/dev/null
	wait "$viewport_pid" 2>/dev/null
	viewport_pid=

	if [ "$status" -ne 0 ]; then
		cat "$log" >&2
	fi
	return "$status"
}

# A config file that says nothing about X11 leaves the X server exactly as it
# was before the key existed. This half of the test is the one that would
# catch the key defaulting itself on. An empty file rather than none at all,
# so that whatever is in the config directory of the machine running the
# tests cannot decide the answer.
empty=$workdir/empty.json
echo '{}' >"$empty"
if ! check_scale 1 "$empty"; then
	echo "FAIL: a config file that says nothing about X11 did not leave it at 1x" >&2
	exit 1
fi

config=$workdir/config.json
cat >"$config" <<'JSON'
{ "xwayland": { "scale": 2 } }
JSON

if ! check_scale 2 "$config"; then
	echo "FAIL: xwayland.scale did not reach the X server" >&2
	echo "      The screen size comes from the client scale on the Xwayland" >&2
	echo "      connection; the settings come from X11Wm::set_xsettings." >&2
	exit 1
fi

echo "PASS: X11 is 1x unasked, and both halves of the scale arrive when asked"
exit 0
