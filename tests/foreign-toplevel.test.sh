#!/usr/bin/env bash
# The window list can be seen, and acted on, from outside.
#
# wlr-foreign-toplevel-management-v1 is the protocol a taskbar or a window
# switcher uses: it lists the toplevels and lets a client focus, close or
# fullscreen them. The listing alone does not prove much — ext-foreign-
# toplevel-list already publishes that — so this drives the requests and
# checks the compositor carries them out over the real socket:
#
#   - a paint client puts a window on screen, and the manager must list it
#     (the `toplevel` handle the test client waits for);
#   - set_fullscreen/unset_fullscreen/activate must be accepted (the
#     compositor forwards them to the shell, which does not echo fullscreen
#     back, so nothing is asserted about a state event);
#   - close() must actually close the window, and the handle must then say
#     `closed` — the part only this protocol can be checked on.
set -uo pipefail

VIEWPORT=${1:?usage: foreign-toplevel.test.sh VIEWPORT FT_CLIENT PAINT_CLIENT}
FT_CLIENT=${2:?}
PAINT_CLIENT=${3:?}

WORK=$(mktemp -d)
LOG="$WORK/viewport.log"
COMPOSITOR_PID=
PAINT_PID=

cleanup() {
  [ -n "$PAINT_PID" ] && kill "$PAINT_PID" 2>/dev/null
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

# One window for the list to contain. Nothing else must be running, or the
# client could not tell which handle is the window being closed.
"$PAINT_CLIENT" foreign-test 800 600 0 ffff0000 ffff0000 \
  >"$WORK/paint.log" 2>&1 &
PAINT_PID=$!

# Let the window map; the compositor publishes the toplevel when it does,
# and says so in the `view <id>` line it logs at the first buffer. Wait for
# that line rather than guessing at a sleep.
mapped=
for _ in $(seq 100); do
  if grep -qE 'view [0-9]+: ' "$LOG"; then
    mapped=yes
    break
  fi
  kill -0 "$PAINT_PID" 2>/dev/null || break
  sleep 0.1
done

if [ -z "$mapped" ]; then
  echo "FAIL the window never mapped, so there is no toplevel to manage"
  tail -20 "$WORK/paint.log"
  sed -n '1,60p' "$LOG"
  exit 2
fi

if ! kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
  echo "FAIL the compositor died"
  sed -n '1,60p' "$LOG"
  exit 1
fi

if "$FT_CLIENT" >"$WORK/ft.log" 2>&1; then
  echo "ok   the foreign-toplevel client saw the window and closed it"
  cat "$WORK/ft.log" | sed 's/^/     /'
  status=0
else
  echo "FAIL the foreign-toplevel client"
  cat "$WORK/ft.log" | sed 's/^/     /'
  tail -20 "$LOG"
  status=1
fi

exit $status
