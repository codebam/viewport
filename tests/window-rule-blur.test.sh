#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# The window-rule blur, end to end, without a display.
#
# A rule that says `blur: true` has to blur a window that never asked for it:
# the shell resolves the rule when the window opens and sends `view.blur`, and
# the compositor draws the same framebuffer effect a client's
# `ext-background-effect-v1` request would. That is the whole chain — the
# config file, the shell's rule resolution, the IPC message, the render path —
# so this drives the real compositor headless with the real shell, which is
# the only configuration where every half of it is present.
#
# The window under test is a fully transparent one floating over a second
# window that paints a hard red/blue edge down its own middle (the paint
# client's `split` mode — the boundary half of its `popup` mode without the
# popup, so nothing of the client's own asks for an effect). Without blur, the
# glass shows the sharp edge through itself; with the rule's blur, the edge is
# mixed. Sharp pixels either side of the boundary are the controls: a blurred
# red is still red, so they pin the geometry as well as the effect.
#
# Run it directly, or through scripts/integration.sh which compiles the
# clients first.
set -u

viewport=$(realpath "${1:?usage: window-rule-blur.test.sh VIEWPORT PAINT-CLIENT CAPTURE-CLIENT}")
paint_client=$(realpath "${2:?}")
capture_client=$(realpath "${3:?}")

for binary in "$viewport" "$paint_client" "$capture_client"; do
	if [ ! -x "$binary" ]; then
		echo "missing $binary — build first" >&2
		exit 2
	fi
done

# This is the one integration test that needs the real shell process, and the
# shell needs what a hosted runner cannot give it: WebKitGTK's own network
# service dies there on the sandbox's missing loopback (`bwrap: loopback:
# Failed RTM_NEWADDR: Operation not permitted`, then `failed to receive
# credentials` and a SIGABRT), so the shell never places anything and the
# compositor falls back to the built-in layout where no rule is ever
# resolved. Rather than pretend that answers anything about the blur, the test
# skips wherever the shell cannot run — the same deal the X11 tests get — and
# says why. A machine that can run the shell runs this for real.
if [ -n "${VIEWPORT_RULE_TEST_SKIP_SHELL:-}" ]; then
	echo "SKIP: VIEWPORT_RULE_TEST_SKIP_SHELL is set — the shell this test drives will not be started"
	exit 77
fi

# The two windows. The backdrop paints red left of its own middle, blue right
# (`split`); the glass is transparent everywhere (margin 0, alpha 0 body) so
# whatever the compositor draws behind it is what the screen shows.
backdrop_id=viewport-blur-backdrop
glass_id=viewport-blur-glass
width=600
height=440

workdir=$(mktemp -d)
viewport_pid=
paint_pids=()

cleanup() {
	# By PID only. Pattern matching on "viewport" has killed a live session
	# more than once, and this script runs on the same machine as one.
	for pid in "${paint_pids[@]+"${paint_pids[@]}"}"; do
		kill "$pid" 2>/dev/null
	done
	[ -n "$viewport_pid" ] && kill "$viewport_pid" 2>/dev/null
	wait 2>/dev/null
	rm -rf "$workdir"
}
trap cleanup EXIT INT TERM

# The rule under test: float the glass over the whole output and blur it. The
# rect reaches every edge of the output, so wherever the backdrop's boundary
# lands, the glass is over it — the sample point cannot miss for a reason the
# test does not control.
printf '{ "layout": "tiling", "rules": [ { "app_id": "%s", "blur": true, "floating": true, "x": 0, "y": 0, "width": 640, "height": 480 } ] }\n' \
	"$glass_id" >"$workdir/config.json"

# Off whatever session is already running: this test starts its own compositor
# and must not join, or be joined to, the one the developer is sitting in.
# The state home is isolated for the same reason — a restored layout would
# tile the backdrop beside windows that are not there.
unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export XDG_STATE_HOME="$workdir/state"

# The placement lines are debug-level; raising the filter lets the waits below
# read where the windows really are rather than guessing at how long a layout
# takes. A value from the caller wins, as everywhere else.
export VIEWPORT_LOG="${VIEWPORT_LOG:-viewport=debug}"

"$viewport" --headless --width 640 --height 480 --watch-config \
	--config "$workdir/config.json" --socket "$workdir/control.sock" \
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

# The backdrop first, tiled, alone on its workspace so it fills the work area
# whatever the gaps are. Its own painting puts the boundary at its middle.
"$paint_client" "$backdrop_id" "$width" "$height" 0 ffff0000 ff0000ff split \
	>"$workdir/backdrop.log" 2>&1 &
paint_pids+=($!)

# The glass floats over it by the rule, so both can start together: the rule
# is resolved on the window's arrival, not on anything the test does.
"$paint_client" "$glass_id" 640 480 0 00000000 00000000 \
	>"$workdir/glass.log" 2>&1 &
paint_pids+=($!)

status=0

# Which view is which. The placement lines name an id, not a window, and two
# windows mapping in the same dispatch can map in either order — so the ids
# are taken from the painted lines instead, where the buffer size says who:
# 600x440 is the backdrop, 640x480 the glass. First paint is before first
# placement, so both ids exist by the time either is looked up below.
backdrop_id_num=
glass_id_num=
for _ in $(seq 1 150); do
	backdrop_id_num=$(grep -Eo "view [0-9]+: painted ${width}x${height}" \
		"$workdir/viewport.log" | tail -1 | grep -Eo '[0-9]+' | head -1)
	glass_id_num=$(grep -Eo "view [0-9]+: painted 640x480" \
		"$workdir/viewport.log" | tail -1 | grep -Eo '[0-9]+' | head -1)
	[ -n "$backdrop_id_num" ] && [ -n "$glass_id_num" ] && break
	sleep 0.1
done
if [ -z "$backdrop_id_num" ] || [ -z "$glass_id_num" ]; then
	echo "FAIL a window never painted; the compositor's log:" >&2
	tail -20 "$workdir/viewport.log" >&2
	exit 2
fi
echo "ok   backdrop is view $backdrop_id_num, glass is view $glass_id_num"

# The backdrop's placed rectangle, as the shell laid it out. The client paints
# its own 600x440 buffer whatever it is configured with, and a buffer is drawn
# unscaled at the hole's origin (margin 0, no decorations), so the colour
# boundary is at the origin plus half the *buffer's* width — not half the
# hole's, which is why the sizes are not assumed equal here.
placed=
for _ in $(seq 1 150); do
	placed=$(grep -E "view ${backdrop_id_num}: placed at [0-9]+x[0-9]+[+-][0-9]+[+-][0-9]+" \
		"$workdir/viewport.log" | tail -1)
	[ -n "$placed" ] && break
	sleep 0.1
done
if [ -z "$placed" ]; then
	echo "FAIL the backdrop was never placed; the compositor's log:" >&2
	tail -20 "$workdir/viewport.log" >&2
	exit 2
fi
if [[ "$placed" =~ view\ [0-9]+:\ placed\ at\ [0-9]+x[0-9]+([+-][0-9]+)([+-][0-9]+) ]]; then
	bx=${BASH_REMATCH[1]}
	by=${BASH_REMATCH[2]}
else
	echo "FAIL could not parse the placement line: $placed" >&2
	exit 2
fi
# Where the backdrop's own boundary is: the middle of its buffer, which is
# where its `split` painting changes colour. The y is the middle of the buffer
# as well, well inside the hole however tall the shell made it.
seam=$((bx + width / 2))
midy=$((by + height / 2))
echo "ok   backdrop placed at ${bx},${by}; its colour boundary is at x=$seam"

# And the glass over it — the floating rule's rect, echoed back by the same
# log. Waited for rather than assumed, because the blur sample is meaningless
# if the glass is not on screen yet.
glass_placed=
for _ in $(seq 1 150); do
	glass_placed=$(grep -Eo "view ${glass_id_num}: placed at [0-9]+x[0-9]+[+-][0-9]+[+-][0-9]+" \
		"$workdir/viewport.log" | tail -1)
	[ -n "$glass_placed" ] && break
	sleep 0.1
done
if [ -z "$glass_placed" ]; then
	echo "FAIL the glass was never placed; the compositor's log:" >&2
	tail -20 "$workdir/viewport.log" >&2
	tail -10 "$workdir/glass.log" >&2
	exit 2
fi
echo "ok   glass placed by the rule ($(echo "$glass_placed" | grep -Eo '[0-9]+x[0-9]+[+-][0-9]+[+-][0-9]+$'))"

# The rule said blur, so the shell must have said so: the message is logged
# with the filter raised above. Without it the compositor never draws the
# effect, and the boundary sample below would come back sharp for a reason
# this test could not otherwise name.
blurred=
for _ in $(seq 1 150); do
	if grep -q "\"type\":\"view.blur\"" "$workdir/viewport.log"; then
		blurred=yes
		break
	fi
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done
if [ -n "$blurred" ]; then
	echo "ok   the shell sent view.blur"
else
	echo "FAIL the shell never sent view.blur" >&2
	status=1
fi

# What the screen shows. The boundary under the glass is mixed if the rule's
# blur drew; either side of it stays the colour it was, because a blurred red
# is still red — those two are the controls that pin the geometry, so a miss
# here says where rather than just that something did.
if "$capture_client" --output-pixel-mixed "$((seam + 4))" "$midy" \
	>"$workdir/capture-mixed.log" 2>&1; then
	echo "ok   the boundary is mixed under the rule's blur"
else
	echo "FAIL the boundary under the glass is sharp — the rule's blur did not draw" >&2
	tail -3 "$workdir/capture-mixed.log" >&2
	status=1
fi
if "$capture_client" --output-pixel "$((seam - 60))" "$midy" ff0000 \
	>"$workdir/capture-red.log" 2>&1; then
	echo "ok   red either side of the boundary stays red"
else
	echo "FAIL the red control sample is not red" >&2
	tail -3 "$workdir/capture-red.log" >&2
	status=1
fi
if "$capture_client" --output-pixel "$((seam + 60))" "$midy" 0000ff \
	>"$workdir/capture-blue.log" 2>&1; then
	echo "ok   blue either side of the boundary stays blue"
else
	echo "FAIL the blue control sample is not blue" >&2
	tail -3 "$workdir/capture-blue.log" >&2
	status=1
fi

# Take the blur back out of the rule and rewrite the file: the compositor
# reloads it, the shell re-resolves every window against the new rules, and
# the message that arrives for the glass now says blur off. The edge under it
# goes sharp again — the other half of what the feature promises, because a
# rule that stopped matching must not leave its effect behind.
blurs_before=$(grep -c '"type":"view.blur"' "$workdir/viewport.log" || true)
printf '{ "layout": "tiling", "rules": [ { "app_id": "%s", "floating": true, "x": 0, "y": 0, "width": 640, "height": 480 } ] }\n' \
	"$glass_id" >"$workdir/config.json"
# The watcher fires on the file's own change; an mtime-only touch is enough.
sleep 1
touch "$workdir/config.json"
unblurred=
for _ in $(seq 1 150); do
	blurs=$(grep -c '"type":"view.blur"' "$workdir/viewport.log" || true)
	if [ "$blurs" -gt "$blurs_before" ] &&
		grep -q '"blur":false' "$workdir/viewport.log"; then
		unblurred=yes
		break
	fi
	sleep 0.1
done
if [ -n "$unblurred" ]; then
	echo "ok   the rule without blur re-resolved and sent view.blur false"
else
	echo "FAIL the shell never took the blur back off" >&2
	status=1
fi
if "$capture_client" --output-pixel "$((seam + 4))" "$midy" 0000ff \
	>"$workdir/capture-unblurred.log" 2>&1; then
	echo "ok   the boundary is sharp again once the rule stops asking"
else
	echo "FAIL the boundary is still mixed after the rule took its blur back" >&2
	tail -3 "$workdir/capture-unblurred.log" >&2
	status=1
fi
# The mixed form going away is the assertion — the same pixel that mixed
# before is one colour now, and the controls above said which is where.

if [ "$status" -ne 0 ]; then
	echo "--- compositor ---" >&2
	tail -40 "$workdir/viewport.log" >&2
fi
exit "$status"
