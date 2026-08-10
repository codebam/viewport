#!/usr/bin/env bash
# The shell's workspaces reach a bar, and the bar's requests come back.
#
# ext-workspace-v1 is the staging protocol a workspace bar binds. Here the
# workspaces belong to the shell — there is no shell in this test, so its
# `workspace.list` is sent over the control socket the way data/shell does —
# and the compositor republishes it: a client binding ext_workspace_manager_v1
# must see a workspace_group and a workspace handle. The requests a bar makes
# are forwarded in the other direction: the client's activate + commit must
# come back out of the control socket as a workspace.request event, which is
# the observable proof that the whole round trip works.
set -uo pipefail

VIEWPORT=${1:?usage: workspace.test.sh VIEWPORT WS_CLIENT}
WS_CLIENT=${2:?}

WORK=$(mktemp -d)
LOG="$WORK/viewport.log"
COMPOSITOR_PID=

cleanup() {
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

# Off whatever session is already running: this starts its own compositor and
# must not join, or be joined to, the one the developer is sitting in.
unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"

"$VIEWPORT" --headless --timeout 1 >"$LOG" 2>&1 &
COMPOSITOR_PID=$!

for _ in $(seq 300); do
  grep -q 'control socket at' "$LOG" && break
  grep -q 'WAYLAND_DISPLAY=' "$LOG" && break
  kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
  sleep 0.1
done

WAYLAND_DISPLAY=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$LOG" |
  head -1 | cut -d= -f2)
SOCK=$(grep -o 'control socket at [^ ]*' "$LOG" | head -1 | awk '{print $4}')

if [ -z "$WAYLAND_DISPLAY" ] || [ -z "$SOCK" ]; then
  echo "FAIL the compositor never published its wayland socket"
  tail -20 "$LOG"
  exit 2
fi
export WAYLAND_DISPLAY
export WS_CLIENT
echo "ok   the compositor is up on $WAYLAND_DISPLAY, socket at $SOCK"

timeout 60 python3 -u - "$SOCK" >"$WORK/py.log" 2>&1 <<'PY'
import json, os, socket, subprocess, sys, time

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])


def send(message):
    s.sendall((json.dumps(message) + "\n").encode())
    time.sleep(0.5)


def read_events(deadline=5.0):
    """Collect JSON events until one matches or the clock runs out."""
    s.settimeout(1.0)
    end = time.monotonic() + deadline
    buf = b""
    while time.monotonic() < end:
        try:
            chunk = s.recv(65536)
        except Exception:
            break
        if not chunk:
            break
        buf += chunk
    events = []
    for line in buf.decode(errors="replace").splitlines():
        try:
            events.append(json.loads(line))
        except Exception:
            continue
    return events


# What is the headless output called? The shell's workspace.list names an
# output, and a group is only made for one that exists.
send({"type": "output.query"})
layout = None
for event in read_events():
    if event.get("type") == "output.layout":
        layout = event
if layout is None or not layout.get("outputs"):
    sys.exit("no output.layout arrived, so no output to put the workspace on")
name = layout["outputs"][0]["name"]
print(f"ok   the headless output is {name}")

# The shell announcing its workspaces. Without this nothing is published:
# a compositor with no shell has no workspaces of its own.
send({
    "type": "workspace.list",
    "workspaces": [
        {"id": "1", "name": "one", "output": name, "active": True},
    ],
})

# A bar binds, sees the group and workspace, and asks to switch to it.
result = subprocess.run(
    [os.environ["WS_CLIENT"]],
    env={**os.environ, "WAYLAND_DISPLAY": os.environ["WAYLAND_DISPLAY"]},
    capture_output=True,
    text=True,
)
sys.stdout.write(result.stdout)
if result.returncode != 0:
    sys.exit(f"the workspace client failed:\n{result.stdout}\n{result.stderr}")

# The request must come back out on the control socket: activate, id "1".
request = None
for event in read_events(5.0):
    if event.get("type") == "workspace.request":
        request = event
if request is None:
    sys.exit("no workspace.request came back on the control socket")
print(f"ok   the compositor forwarded {request}")
if request.get("action") != "activate" or request.get("id") != "1":
    sys.exit(f"the forwarded request was not activate of workspace 1: {request}")
PY
status=$?

if [ "$status" -ne 0 ]; then
  echo "FAIL: workspace list and forward" >&2
  cat "$WORK/py.log"
  tail -20 "$LOG"
  exit 1
fi

cat "$WORK/py.log"
echo "PASS: the shell's workspaces are published and its requests forwarded"
