#!/usr/bin/env bash
# A monitor configuration client can see the heads and apply a configuration.
#
# wlr-output-management-v1 is what wlr-randr and kanshi speak. The compositor
# advertises every head and its modes, stamps the set with a serial, and a
# client builds a configuration against that serial. This is the whole
# conversation, checked end to end over the real socket:
#
#   - the manager must publish a head and finish with done(serial);
#   - create_configuration(serial) -> enable_head(head) -> apply must answer
#     succeeded (not failed, not cancelled);
#   - the same again with test must also succeed — a test applies nothing but
#     still exercises the same request path.
set -uo pipefail

VIEWPORT=${1:?usage: output-management.test.sh VIEWPORT OM_CLIENT}
OM_CLIENT=${2:?}

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

# libwayland picks the socket name and it is printed at startup; there is no
# way to ask for a particular one, so it is read back out of the log.
WAYLAND_DISPLAY=
for _ in $(seq 100); do
  WAYLAND_DISPLAY=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$LOG" |
    head -1 | cut -d= -f2)
  [ -n "$WAYLAND_DISPLAY" ] && break
  kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
  sleep 0.1
done

if [ -z "$WAYLAND_DISPLAY" ]; then
  echo "FAIL the compositor never published a wayland socket"
  tail -20 "$LOG"
  exit 2
fi
export WAYLAND_DISPLAY
echo "ok   the compositor is up on $WAYLAND_DISPLAY"

if "$OM_CLIENT" >"$WORK/om.log" 2>&1; then
  echo "ok   the output-management client configured the head"
  cat "$WORK/om.log" | sed 's/^/     /'
  status=0
else
  echo "FAIL the output-management client"
  cat "$WORK/om.log" | sed 's/^/     /'
  tail -20 "$LOG"
  status=1
fi

exit $status
