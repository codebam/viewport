#!/usr/bin/env bash
# Monitors must enumerate in the order they were plugged in.
#
# server->outputs is walked by everything that answers "which monitors are
# there, and in what order": the output.layout array the shell draws its display
# panel from, the wlr-output-management head list wlr-randr prints, and three
# fallbacks that take outputs.next and treat it as the leftmost output. The
# geometry beside it comes from wlr_output_layout_add_auto(), which arranges
# monitors left to right in the order they arrive.
#
# Those two have to agree. wl_list_insert(&list, elem) inserts after the
# sentinel — at the head — so the obvious spelling builds the list backwards and
# the shell drew the monitors mirrored while the pointer moved between them the
# other way. A DisplayPort monitor powering off drops its connector, so every
# blank re-runs this path and the desktop can come back reversed.
#
# So: plug in two more outputs, ask for the layout, and require that the array
# is ordered by x. Reversal is the exact failure this catches.
set -uo pipefail

VIEWPORT=${1:?usage: output-order.test.sh VIEWPORT}

WORK=$(mktemp -d)
LOG="$WORK/viewport.log"
COMPOSITOR_PID=

cleanup() {
	# By pid, never by name: this runs on developer machines where the session
	# itself is very often another viewport.
	if [ -n "$COMPOSITOR_PID" ] && kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
		kill -TERM "$COMPOSITOR_PID" 2>/dev/null
		for _ in $(seq 20); do
			kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
			sleep 0.1
		done
		kill -KILL "$COMPOSITOR_PID" 2>/dev/null
	fi
	rm -rf "$WORK"
}
trap cleanup EXIT

unset WAYLAND_DISPLAY
unset DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export WLR_BACKENDS=headless
export WLR_RENDERER=${VP_RENDERER:-vulkan}

printf '{ "layout": "tiling" }\n' >"$WORK/config.json"

"$VIEWPORT" --headless -c "$WORK/config.json" >"$LOG" 2>&1 &
COMPOSITOR_PID=$!

for _ in $(seq 300); do
	grep -q 'control socket at' "$LOG" && break
	kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
	sleep 0.1
done

SOCK=$(grep -o 'control socket at [^ ]*' "$LOG" | head -1 | awk '{print $4}')
if [ -z "$SOCK" ]; then
	echo "the compositor never opened its control socket" >&2
	tail -20 "$LOG" >&2
	exit 2
fi

# The socket has to stay open past the send: handle_client_event() closes the
# client on WL_EVENT_HANGUP before reading, so send-then-close drops the command.
timeout 60 python3 - "$SOCK" <<'PY'
import json, socket, sys, time

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])


def send(message):
    s.sendall((json.dumps(message) + "\n").encode())
    time.sleep(0.5)


# The headless backend starts with one output; these two arrive through the
# same new_output signal a real hotplug uses.
send({"type": "output.test_add"})
send({"type": "output.test_add"})
send({"type": "output.query"})

# Bounded by the clock, not by a quiet socket: the compositor keeps publishing
# while the shell paints, so a read loop waiting for silence never returns.
s.settimeout(1.0)
deadline = time.monotonic() + 5.0
buf = b""
while time.monotonic() < deadline:
    try:
        chunk = s.recv(65536)
    except Exception:
        break
    if not chunk:
        break
    buf += chunk

layout = None
for line in buf.decode(errors="replace").splitlines():
    try:
        message = json.loads(line)
    except Exception:
        continue
    if message.get("type") == "output.layout":
        layout = message

if layout is None:
    sys.exit("no output.layout event arrived")

outputs = layout["outputs"]
order = [(o["name"], o.get("x")) for o in outputs]
print("output.layout order:", order)

if len(outputs) < 3:
    sys.exit(f"expected at least 3 outputs after two hotplugs, got {len(outputs)}")

xs = [o.get("x") for o in outputs]
if any(x is None for x in xs):
    sys.exit(f"an output has no x position: {order}")

if xs != sorted(xs):
    sys.exit(
        "output.layout is not ordered left to right — server->outputs is "
        f"walking backwards: {order}"
    )
PY
status=$?

if [ "$status" -ne 0 ]; then
	echo "FAIL: output enumeration order" >&2
	exit 1
fi

echo "PASS: outputs enumerate in connection order, left to right"
