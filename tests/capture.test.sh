#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Window capture, end to end, without a display.
#
# Starts the compositor on the headless backend, puts a window in front of it
# that is painted to be recognisable, and asks the capture client whether what
# comes back is that window. See tests/capture-client.c for what is actually
# being asserted and why those two things are the assertions.
#
# Run it directly, or through scripts/integration.sh which compiles the
# clients first:
#
#   tests/capture.test.sh build/viewport \
#     build/viewport-test-paint-client build/viewport-test-capture-client
set -u

viewport=${1:-build/viewport}
paint_client=${2:-build/viewport-test-paint-client}
capture_client=${3:-build/viewport-test-capture-client}
# "tiling" or "scrolling". The scrolling layout clips and rescales every window
# on every frame, which is the harder case for a capture to survive: the size
# churn that came out of that is what froze the sharing client.
layout=${4:-tiling}
privacy=${5:-public}
content=${6:-plain}

for binary in "$viewport" "$paint_client" "$capture_client"; do
	if [ ! -x "$binary" ]; then
		echo "missing $binary — build first" >&2
		exit 2
	fi
done

# The window under test. The margin is what a client with its own shadows
# paints outside its window; 0000ff there and ff0000 inside means a capture
# that includes the decoration says so in blue.
app_id=viewport-capture-test
width=320
height=240
margin=24
body=ffff0000
edge=ff0000ff
expected=$body

workdir=$(mktemp -d)
viewport_pid=
paint_pid=
filler_pids=()

cleanup() {
	# By PID only. Pattern matching on "viewport" has killed a live session
	# more than once, and this script runs on the same machine as one.
	for pid in "${filler_pids[@]+"${filler_pids[@]}"}"; do
		kill "$pid" 2>/dev/null
	done
	[ -n "$paint_pid" ] && kill "$paint_pid" 2>/dev/null
	[ -n "$viewport_pid" ] && kill "$viewport_pid" 2>/dev/null
	wait 2>/dev/null
	rm -rf "$workdir"
}
trap cleanup EXIT INT TERM

if [ "$privacy" = private ]; then
	printf '{ "layout": "%s", "rules": [{ "app_id": "%s", "capture": false }] }\n' \
		"$layout" "$app_id" >"$workdir/config.json"
	expected=ff000000
else
	printf '{ "layout": "%s" }\n' "$layout" >"$workdir/config.json"
fi

# Off whatever session is already running: this test starts its own compositor
# and must not join, or be joined to, the one the developer is sitting in.
unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"

# The trace that logs placement is debug-level; raise the filter so the waits
# below can watch for the window really being there instead of guessing at
# how long a layout takes. A value from the caller wins, as everywhere else.
export VIEWPORT_LOG="${VIEWPORT_LOG:-viewport=debug}"

"$viewport" --headless --config "$workdir/config.json" \
	>"$workdir/viewport.log" 2>&1 &
viewport_pid=$!

# The socket name is chosen by libwayland and printed at startup; there is no
# way to ask for a particular one, so it has to be read back out of the log.
display=
for _ in $(seq 1 100); do
	display=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$workdir/viewport.log" \
		| head -1 | cut -d= -f2)
	[ -n "$display" ] && break
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done

if [ -z "$display" ]; then
	echo "the compositor did not come up; its log:" >&2
	tail -20 "$workdir/viewport.log" >&2
	exit 2
fi

export WAYLAND_DISPLAY="$display"

paint_args=("$app_id" "$width" "$height" "$margin" "$body" "$edge")
[ "$content" = popup ] && paint_args+=(popup)
"$paint_client" "${paint_args[@]}" >"$workdir/paint.log" 2>&1 &
paint_pid=$!

# In the scrolling layout, crowd the window under test.
#
# It opens first and the strip then grows to the right of it, so it ends up at
# the edge, clipped, with a clip that moves as the strip is laid out again —
# and one of the fillers never stops resizing, so it is laid out again on every
# frame. A window in that state is what the capture has to survive: it is the
# state the window was in when sharing it froze the client.
if [ "$layout" = scrolling ]; then
	# The strip grows to the right of whatever opened first, so the window
	# under test has to be on screen before the fillers are: wait for the
	# compositor to announce it (the `view <id>` line it logs at first
	# buffer) rather than guessing at a sleep.
	mapped=
	for _ in $(seq 1 100); do
		if grep -qE 'view [0-9]+: ' "$workdir/viewport.log" 2>/dev/null; then
			mapped=yes
			break
		fi
		kill -0 "$paint_pid" 2>/dev/null || break
		sleep 0.1
	done
	if [ -z "$mapped" ]; then
		echo "FAIL the window under test never opened, so there is nothing" >&2
		echo "     to crowd; the fillers would be laid out in its place" >&2
		tail -20 "$workdir/viewport.log" >&2
		tail -10 "$workdir/paint.log" >&2
		exit 2
	fi
	for i in 1 2 3; do
		pulse=
		[ "$i" = 1 ] && pulse=pulse
		"$paint_client" "filler-$i" 700 400 16 ff202020 ff202020 $pulse \
			>"$workdir/filler-$i.log" 2>&1 &
		filler_pids+=($!)
	done
fi

# Wait for the shell to have placed it rather than guessing at a sleep: until
# the layout has run, the window has no size and the capture would be of
# whatever it was before. The placement line is what screencast-restore waits
# on too; with the filter raised above it is really there to be seen.
placed=
for _ in $(seq 1 100); do
	if grep -qE 'view .* (boxed|placed at)' "$workdir/viewport.log" 2>/dev/null; then
		placed=yes
		break
	fi
	kill -0 "$paint_pid" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$placed" ]; then
	echo "FAIL the window was never placed, so a capture now would not be" >&2
	echo "     of it; the compositor's log:" >&2
	tail -20 "$workdir/viewport.log" >&2
	tail -10 "$workdir/paint.log" >&2
	exit 2
fi

if [ "$content" = popup ]; then
	popup_ready=
	for _ in $(seq 1 100); do
		if grep -q '^popup-ready$' "$workdir/paint.log" 2>/dev/null; then
			popup_ready=yes
			break
		fi
		kill -0 "$paint_pid" 2>/dev/null || break
		sleep 0.1
	done
	if [ -z "$popup_ready" ]; then
		echo "FAIL the blur popup never painted" >&2
		status=2
	else
		status=0
		"$capture_client" --window-pixel "$app_id" 32 32 "$body" \
			"$width" "$height" || status=1
		"$capture_client" --window-pixel-mixed "$app_id" 160 120 \
			"$width" "$height" || status=1
	fi
else
	"$capture_client" "$app_id" "$expected" "$width" "$height" 2000
	status=$?
fi

if [ "$status" -eq 0 ] && [ "$privacy" = private ]; then
	"$capture_client" --output-not "$body"
	status=$?
	# Prove absence of red came from policy rather than from a window that was
	# never in output capture. This also exercises the runtime allow direction.
	view_id=$(grep -oE 'view [0-9]+:' "$workdir/viewport.log" | head -1 \
		| cut -d' ' -f2 | tr -d ':')
	if [ "$status" -eq 0 ] && [ -n "$view_id" ] && "$viewport" msg \
			--socket "$XDG_RUNTIME_DIR/viewport-$display.sock" \
			-t view.capture --id "$view_id" --capture true; then
		"$capture_client" --output-has "$body"
		status=$?
	else
		echo "FAIL could not make the private test window capturable" >&2
		status=2
	fi
fi

if [ "$status" -ne 0 ]; then
	echo
	echo "--- capture policy log ---" >&2
	grep -E 'capture policy' "$workdir/viewport.log" >&2 || true
	echo "--- compositor log (last 30 lines) ---" >&2
	tail -30 "$workdir/viewport.log" >&2
	echo "--- window log ---" >&2
	tail -10 "$workdir/paint.log" >&2
fi

exit "$status"
