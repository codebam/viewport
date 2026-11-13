#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# The see-through window, end to end, without a display.
#
# The reported bug: a window whose frame is drawn and whose client is not, so
# the wallpaper shows through the hole. Three mechanisms can produce it — a
# fade left at zero, a crop with no area, a crop that misses the window — and
# the compositor now heals all three and logs which one it was. This drives the
# real compositor headless, paints a window a known colour, breaks each
# mechanism through the control socket, and asks what is actually on the screen.
#
# It is a regression test rather than a smoke test. Run against 194735ff — the
# commit before the healing landed — a fade left at zero leaves the pixel at the
# background colour with nothing logged. Run against a fixed tree, the fade heals
# and says so, an impossible crop heals and says so, and a crop the shell means
# (empty, a column scrolled out of its strip) hides the window rather than
# painting it over the monitor beside it.
set -u

viewport=$(realpath "${1:?usage: see-through.test.sh VIEWPORT PAINT-CLIENT CAPTURE-CLIENT}")
paint_client=$(realpath "${2:?}")
capture_client=$(realpath "${3:?}")

app_id=viewport-see-through-test
width=320
height=240
margin=24
body=ffff0000
edge=ff0000ff
body_rgb=${body#ff}

workdir=$(mktemp -d)
viewport_pid=
paint_pid=

cleanup() {
	for pid in "$paint_pid" "$viewport_pid"; do
		[ -n "$pid" ] && kill "$pid" 2>/dev/null
	done
	wait 2>/dev/null
	rm -rf "$workdir"
}
trap cleanup EXIT INT TERM

printf '{ "layout": "tiling" }\n' >"$workdir/config.json"

unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export VIEWPORT_LOG="${VIEWPORT_LOG:-viewport=debug}"

"$viewport" --headless --config "$workdir/config.json" \
	--socket "$workdir/control.sock" >"$workdir/viewport.log" 2>&1 &
viewport_pid=$!

display=
for _ in $(seq 1 100); do
	display=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$workdir/viewport.log" \
		| head -1 | cut -d= -f2)
	[ -n "$display" ] && break
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$display" ]; then
	echo "FAIL the compositor did not come up; its log:" >&2
	tail -20 "$workdir/viewport.log" >&2
	exit 2
fi
export WAYLAND_DISPLAY="$display"

"$paint_client" "$app_id" "$width" "$height" "$margin" "$body" "$edge" \
	>"$workdir/paint.log" 2>&1 &
paint_pid=$!

# The window's id and rectangle, as the compositor placed it. In headless there
# is no shell, so the built-in tiler places it and the same debug line reports
# it as any other layout would.
placed=
for _ in $(seq 1 150); do
	placed=$(grep -Eo 'view [0-9]+: placed at [0-9]+x[0-9]+[+-][0-9]+[+-][0-9]+' \
		"$workdir/viewport.log" | tail -1)
	[ -n "$placed" ] && break
	kill -0 "$paint_pid" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$placed" ]; then
	echo "FAIL the window was never placed; the compositor's log:" >&2
	tail -20 "$workdir/viewport.log" >&2
	tail -10 "$workdir/paint.log" >&2
	exit 2
fi

id= pw= ph= px= py=
if [[ "$placed" =~ view\ ([0-9]+):\ placed\ at\ ([0-9]+)x([0-9]+)([+-][0-9]+)([+-][0-9]+) ]]; then
	id=${BASH_REMATCH[1]}
	pw=${BASH_REMATCH[2]}
	ph=${BASH_REMATCH[3]}
	px=${BASH_REMATCH[4]}
	py=${BASH_REMATCH[5]}
else
	echo "FAIL could not parse the placement line: $placed" >&2
	exit 2
fi
# The built-in tiler gives it the whole output; the client paints its own
# 320x240 buffer whatever it is configured with, so place the window at exactly
# that and sample the middle of it. From here the rectangle is ours, which is
# also what makes the crop tests below about a known rectangle.
pw=$width
ph=$height
px=+0
py=+0
cx=$(( width / 2 ))
cy=$(( height / 2 ))

layout() {
	"$viewport" msg --socket "$workdir/control.sock" --raw "$1" \
		>"$workdir/msg.log" 2>&1 || {
		echo "FAIL the layout was refused:" >&2
		cat "$workdir/msg.log" >&2
		status=1
	}
}
place() { # place CLIP-JSON
	local clip=${1:-}
	local json
	if [ -n "$clip" ]; then
		json="{\"type\":\"view.layout\",\"id\":$id,\"x\":0,\"y\":0,\"width\":$width,\"height\":$height,\"clip\":$clip}"
	else
		json="{\"type\":\"view.layout\",\"id\":$id,\"x\":0,\"y\":0,\"width\":$width,\"height\":$height}"
	fi
	layout "$json"
}
place
for _ in $(seq 1 100); do
	grep -q "placed at ${width}x${height}+0+0" "$workdir/viewport.log" && break
	sleep 0.05
done
echo "ok   window $id placed at ${width}x${height}+0+0 by the test; sampling $cx,$cy"

status=0
want() { # want LABEL ARGB
	local label=$1 colour=$2
	if "$capture_client" --output-pixel "$cx" "$cy" "$colour" \
		>"$workdir/capture.log" 2>&1; then
		echo "ok   $label"
	else
		echo "FAIL $label" >&2
		tail -3 "$workdir/capture.log" >&2
		status=1
	fi
}
want_not() { # want_not LABEL ARGB
	local label=$1 colour=$2
	if "$capture_client" --output-pixel-not "$cx" "$cy" "$colour" \
		>"$workdir/capture.log" 2>&1; then
		echo "ok   $label"
	else
		echo "FAIL $label" >&2
		tail -3 "$workdir/capture.log" >&2
		status=1
	fi
}
saw() { # saw PATTERN LABEL — wait for a log line, bounded
	local pattern=$1 label=$2
	for _ in $(seq 1 100); do
		if grep -qE "$pattern" "$workdir/viewport.log" 2>/dev/null; then
			echo "ok   $label"
			return 0
		fi
		sleep 0.05
	done
	echo "FAIL $label (never logged)" >&2
	status=1
}

want "the window is drawn before anything is broken" "$body_rgb"

# 1. A crop with no area. That is the shell hiding a column scrolled out of
#    its strip, so it is honoured: the window is not drawn over whatever is
#    beside it. Drawing it anyway is how a column from one monitor appears on
#    the next one over.
place '{"x":0,"y":0,"width":0,"height":0}'
want_not "a zero-area crop hides the window instead of drawing it" "$body_rgb"
if grep -q "does not meet it" "$workdir/viewport.log"; then
	echo "FAIL a zero-area crop was treated as a disagreement" >&2
	status=1
else
	echo "ok   a zero-area crop is honoured without a warning"
fi
place
want "an ordinary layout brings the window back" "$body_rgb"

# 2. A crop that has area and lands somewhere the window is not. That cannot
#    be the shell's own intersection, so it is dropped and the window drawn.
place '{"x":5000,"y":5000,"width":300,"height":200}'
saw "does not meet it" "a crop that misses the window is refused and logged"
want "the window is still drawn under a crop that misses it" "$body_rgb"

# 3. A fade left at zero. The compositor owes the window a raise, and pays it
#    after the grace period: what matters is what the screen shows afterwards.
"$viewport" msg --socket "$workdir/control.sock" -t view.opacity \
	--id "$id" --opacity 0 >"$workdir/msg.log" 2>&1 ||
	{ echo "FAIL the opacity was refused" >&2; cat "$workdir/msg.log" >&2; status=1; }
saw "drawn see-through" "a fade left at zero is paid out and logged"
want "the window is drawn again after the failed fade" "$body_rgb"

if [ "$status" -eq 0 ]; then
	echo "=== see-through: pass"
else
	echo "=== see-through: FAIL" >&2
	tail -30 "$workdir/viewport.log" >&2
fi
exit "$status"
