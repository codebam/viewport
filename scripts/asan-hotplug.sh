#!/usr/bin/env bash
# Headless ASan run that churns outputs the way a DisplayPort monitor powering
# off and back on does: destroy the wlr_output, create a fresh one, repeat, with
# real clients mapped the whole time. Then exit through the IPC "quit" path so
# the backend teardown that noticed the corrupt heap in production also runs.
set -u
root=$(cd "$(dirname "$0")/.." && pwd)
workdir=$(mktemp -d)
echo "workdir=$workdir"

unset WAYLAND_DISPLAY
unset DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export WLR_BACKENDS=headless
export WLR_RENDERER=${VP_RENDERER:-vulkan}
export ASAN_OPTIONS="detect_leaks=0:abort_on_error=0:halt_on_error=0:detect_odr_violation=0:log_path=$workdir/asan:print_stacktrace=1:detect_stack_use_after_return=0"
export G_SLICE=always-malloc
export MALLOC_CHECK_=0

cycles=${VP_CYCLES:-12}
printf '{ "layout": "%s" }\n' "${VP_LAYOUT:-scrolling}" >"$workdir/config.json"

pids=()
cleanup() {
	for p in "${pids[@]+"${pids[@]}"}"; do kill "$p" 2>/dev/null; done
}
trap cleanup EXIT INT TERM

"$root/build-asan/viewport" --headless -c "$workdir/config.json" \
	--url "file://$root/data/fallback.html" \
	>"$workdir/viewport.log" 2>&1 &
vp=$!
echo "viewport_pid=$vp"

display=
for _ in $(seq 1 300); do
	display=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$workdir/viewport.log" 2>/dev/null | head -1 | cut -d= -f2)
	[ -n "$display" ] && break
	kill -0 "$vp" 2>/dev/null || break
	sleep 0.1
done
echo "display=${display:-NONE}"
[ -z "$display" ] && { tail -20 "$workdir/viewport.log"; exit 2; }
export WAYLAND_DISPLAY="$display"

sock=$(grep -o 'control socket at [^ ]*' "$workdir/viewport.log" | head -1 | awk '{print $4}')
echo "sock=$sock"

# Clients stay mapped across every hotplug, so anything holding a per-output
# pointer on their behalf has something to write through afterwards.
"$root/build-asan/viewport-test-paint-client" pulse 700 400 16 ff202020 ff202020 pulse >"$workdir/c1.log" 2>&1 &
pids+=($!)
"$root/build-asan/viewport-test-paint-client" static-a 320 240 24 ffff0000 ff0000ff >"$workdir/c2.log" 2>&1 &
pids+=($!)
"$root/build-asan/viewport-test-paint-client" static-b 500 300 8 ff00ff00 ff00ffff >"$workdir/c3.log" 2>&1 &
pids+=($!)

sleep "${VP_SETTLE:-10}"

# Optionally leave the session locked for the whole churn. The lock backdrop is
# resized from viewport_session_lock_outputs_changed() on every output that
# appears or disappears, and the locker being gone is the case where the
# compositor owns that tree by itself.
if [ "${VP_LOCK:-0}" = 1 ]; then
	"$root/build-asan/viewport-test-lock-client" crash >"$workdir/lock.log" 2>&1
	echo "lock: $(tail -1 "$workdir/lock.log")"
fi

# The socket has to stay open past the send: handle_client_event() closes the
# client on WL_EVENT_HANGUP before reading, so send-then-close drops the command.
python3 - "$sock" "$cycles" <<'PY'
import json, socket, sys, time

sock, cycles = sys.argv[1], int(sys.argv[2])
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock)


def send(msg):
    s.sendall((json.dumps(msg) + "\n").encode())
    time.sleep(0.35)


for i in range(cycles):
    # Two monitors on, then both dropped: a second output exists while the
    # first goes away, which is the arrangement that reorders the layout.
    send({"type": "output.test_add"})
    send({"type": "output.test_add"})
    send({"type": "output.query"})

    # Blank them the way the idle binding does, which is what the session
    # actually did before the connectors dropped. output.configure also arms
    # the revert timer, keyed by output name — and the monitor that comes back
    # carries the same name as the one that went away.
    names = [f"HEADLESS-{n}" for n in range(1, 40)]
    for name in names[:4]:
        send({"type": "output.configure", "name": name, "enabled": False})
    for name in names[:4]:
        send({"type": "output.configure", "name": name, "enabled": True})

    # Drop the connectors while the blank is still in flight.
    send({"type": "output.test_remove"})
    send({"type": "output.test_remove"})
    send({"type": "output.query"})
    print(f"cycle {i + 1}/{cycles}", flush=True)

send({"type": "quit"})
time.sleep(60)
PY

for _ in $(seq 1 300); do
	kill -0 "$vp" 2>/dev/null || break
	sleep 0.1
done
if kill -0 "$vp" 2>/dev/null; then
	echo "still alive after 30s; backtracing $vp"
	gdb -p "$vp" -batch -ex "thread apply all bt" 2>&1 | tail -60
	kill -KILL "$vp"
fi
wait "$vp" 2>/dev/null
echo "exit_status=$?"
echo "=== asan reports ==="
if ls "$workdir"/asan.* >/dev/null 2>&1; then
	head -60 "$workdir"/asan.*
else
	echo "NONE"
fi
echo "=== compositor log tail ==="
tail -8 "$workdir/viewport.log"
