#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Restoring a screen share, end to end, without a display.
#
# The thing being tested is what OBS does: share a screen once, keep the token,
# and get the same screen back on the next launch — including a launch on the
# other side of a compositor restart, which is the case a view id or an Output
# handle cannot survive. tests/portal-frontend does the frontend's half, which
# is storing the restore data and handing it back.
#
# It runs on a session bus and a PipeWire daemon of its own. The compositor
# claims org.freedesktop.impl.portal.desktop.viewport, and zbus fails the whole
# connection when the name is taken — so on a machine already running viewport,
# a test that joined the live bus would test nothing and a test that replaced
# the name would break the session it was running in. The daemon is the same
# argument plus one: a runner has none at all, and a share is carried on it.
#
#   tests/screencast-restore.test.sh target/debug/viewport \
#     target/debug/examples/portal-frontend
set -u

viewport=${1:-target/debug/viewport}
frontend=${2:-target/debug/examples/portal-frontend}
# Optional: without it the window cases are skipped, because a window to share
# has to be a real client and this suite compiles those elsewhere.
paint_client=${3:-}

for binary in "$viewport" "$frontend"; do
	if [ ! -x "$binary" ]; then
		echo "missing $binary — build first" >&2
		exit 2
	fi
done
viewport=$(realpath "$viewport")
frontend=$(realpath "$frontend")

if ! command -v dbus-run-session >/dev/null; then
	echo "dbus-run-session is not installed" >&2
	exit 2
fi

# Everything below runs inside the private bus, because the compositor claims
# its name at startup and there is no way to hand it one afterwards.
if [ "${VIEWPORT_RESTORE_TEST_BUS:-}" != yes ]; then
	export VIEWPORT_RESTORE_TEST_BUS=yes
	if [ -n "$paint_client" ]; then
		paint_client=$(realpath "$paint_client")
	fi
	exec dbus-run-session -- "$0" "$viewport" "$frontend" "$paint_client"
fi

workdir=$(mktemp -d)
viewport_pid=
client_pid=
pipewire_pid=

# By PID only, and named pids at that. Pattern matching on "viewport" has
# killed a live session more than once, and this script runs on the same
# machine as one; a bare `wait` would take the PipeWire daemon below with it,
# which is a background job of this shell too and the one thing here that is
# meant to outlive every compositor.
stop_viewport() {
	[ -n "$client_pid" ] && kill "$client_pid" 2>/dev/null
	[ -n "$viewport_pid" ] && kill "$viewport_pid" 2>/dev/null
	[ -n "$client_pid" ] && wait "$client_pid" 2>/dev/null
	[ -n "$viewport_pid" ] && wait "$viewport_pid" 2>/dev/null
	client_pid=
	viewport_pid=
}
cleanup() {
	stop_viewport
	[ -n "$pipewire_pid" ] && kill "$pipewire_pid" 2>/dev/null
	pipewire_pid=
	rm -rf "$workdir"
}
trap cleanup EXIT INT TERM

# Off whatever session is already running: this test starts its own compositor
# and must not join, or be joined to, the one the developer is sitting in.
unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"

# Debug, because refusing a token says so at that level: what the compositor
# did with a piece of restore data it would not read is the whole assertion,
# and at info it does it silently.
export VIEWPORT_LOG="${VIEWPORT_LOG:-viewport=debug}"

# PipeWire, which is what a screen share is carried on: the portal's answer is
# a node id, and a compositor that cannot reach a daemon fails Start outright.
# Its own daemon in its own runtime dir, for the same reason as the bus above —
# a hosted runner has no daemon at all, which is what made every check here
# fail there while passing on a desk, and a developer's machine has one that a
# test has no business putting nodes into.
start_pipewire() {
	if ! command -v pipewire >/dev/null; then
		echo "pipewire is not installed" >&2
		exit 2
	fi
	export PIPEWIRE_RUNTIME_DIR="$workdir/pipewire"
	mkdir -p "$PIPEWIRE_RUNTIME_DIR"
	pipewire >"$workdir/pipewire.log" 2>&1 &
	pipewire_pid=$!
	for _ in $(seq 1 100); do
		[ -S "$PIPEWIRE_RUNTIME_DIR/pipewire-0" ] && return 0
		kill -0 "$pipewire_pid" 2>/dev/null || break
		sleep 0.1
	done
	echo "the pipewire daemon did not come up; its log:" >&2
	tail -20 "$workdir/pipewire.log" >&2
	exit 2
}
start_pipewire

failures=0
check() {
	local what=$1 expected=$2 got=$3
	if [ "$expected" = "$got" ]; then
		echo "ok: $what"
	else
		echo "FAIL: $what — wanted $expected, got $got" >&2
		failures=$((failures + 1))
	fi
}

# Start the compositor, and wait for the portal rather than for the socket:
# the frontend's first call is on the bus, and a compositor that is up but has
# not claimed its name yet answers nothing.
start_viewport() {
	local log=$1
	"$viewport" --headless >"$log" 2>&1 &
	viewport_pid=$!
	for _ in $(seq 1 100); do
		grep -q "screencast portals up" "$log" && return 0
		kill -0 "$viewport_pid" 2>/dev/null || break
		sleep 0.1
	done
	echo "the compositor did not publish its portal; its log:" >&2
	tail -20 "$log" >&2
	exit 2
}

# A window to share, and a wait for the layout to have placed it: a window with
# no size yet is one the compositor cannot capture, and a share of it would be
# refused for a reason that has nothing to do with restoring.
window_app_id="viewport-restore-test"
start_client() {
	local log=$1 display
	display=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$log" | head -1 | cut -d= -f2)
	if [ -z "$display" ]; then
		echo "the compositor did not name its socket" >&2
		exit 2
	fi
	WAYLAND_DISPLAY="$display" "$paint_client" "$window_app_id" 320 240 0 \
		ffff0000 ffff0000 >"$workdir/client.log" 2>&1 &
	client_pid=$!
	# Either line means it has a size: the shell places it when there is one,
	# and the built-in layout places it when the shell never answers — which
	# is what happens here, two and a half seconds in, because a headless run
	# has no page.
	for _ in $(seq 1 100); do
		grep -qE "view .* (boxed|placed at)" "$log" && return 0
		kill -0 "$client_pid" 2>/dev/null || break
		sleep 0.1
	done
	echo "the window was never placed; the compositor's log:" >&2
	tail -20 "$log" >&2
	exit 2
}

field() {
	grep -o "^$2=.*" "$1" | tail -1 | cut -d= -f2-
}

# Which monitor a run ended up sharing, as the compositor named it. The kind
# alone is not the assertion — "a monitor" is true of the wrong monitor too.
shared_name() {
	grep -o "sharing [^ ]* as pipewire node" "$1" | tail -1 | awk '{print $2}'
}

# ---------------------------------------------------------------------------
# The first share. Nothing is remembered yet, so this is the ordinary path,
# and what it answers with is the token everything after it depends on.
# ---------------------------------------------------------------------------
start_viewport "$workdir/first.log"
"$frontend" --save "$workdir/token.bin" >"$workdir/share.out" 2>&1
check "the first share started" 0 $?
check "it answered with a persist mode" 2 "$(field "$workdir/share.out" persist_mode)"
check "it answered with restore data" yes "$(field "$workdir/share.out" got_restore_data)"
check "it shared a monitor" 1 "$(field "$workdir/share.out" source_type)"
node=$(field "$workdir/share.out" node)
[ -n "$node" ] && echo "ok: it named a pipewire node ($node)"
[ -s "$workdir/token.bin" ] && echo "ok: the token was written down"

# ---------------------------------------------------------------------------
# The restart. A token that only works while the compositor that issued it is
# still running is a token that does nothing for a recorder: OBS is started
# after the desktop, and the ids it was told about last time are gone.
# ---------------------------------------------------------------------------
stop_viewport
start_viewport "$workdir/second.log"

"$frontend" --restore "$workdir/token.bin" >"$workdir/restore.out" 2>&1
check "the restored share started" 0 $?
check "it was restored, not chosen" 1 \
	"$(grep -c "as the application asked" "$workdir/second.log")"
check "nothing was shared without asking" 0 \
	"$(grep -c "without asking" "$workdir/second.log")"
check "it answered with restore data again" yes \
	"$(field "$workdir/restore.out" got_restore_data)"
check "it is still a monitor" 1 "$(field "$workdir/restore.out" source_type)"
check "it is the same monitor" "$(shared_name "$workdir/first.log")" \
	"$(shared_name "$workdir/second.log")"

# ---------------------------------------------------------------------------
# A token this compositor did not write. The frontend hands back whatever it
# stored for the application, which may have been written by another desktop,
# and reading somebody else's dictionary as our own is how a share of the
# wrong thing starts without anybody being asked.
# ---------------------------------------------------------------------------
stop_viewport
start_viewport "$workdir/third.log"

"$frontend" --restore "$workdir/token.bin" --forge-vendor gnome \
	>"$workdir/foreign.out" 2>&1
check "a foreign token still starts a share" 0 $?
check "but it was not restored from" 0 \
	"$(grep -c "as the application asked" "$workdir/third.log")"
check "the compositor said whose it was" 1 \
	"$(grep -c "ignoring restore data from gnome" "$workdir/third.log")"

# ---------------------------------------------------------------------------
# A monitor token offered to an application that asked only for a window. The
# token is ours and the monitor is there; what is missing is the application's
# consent to being handed one.
# ---------------------------------------------------------------------------
stop_viewport
start_viewport "$workdir/fourth.log"

"$frontend" --restore "$workdir/token.bin" --types 2 >"$workdir/window.out" 2>&1
check "a window request is not answered with the remembered monitor" 0 \
	"$(grep -c "as the application asked" "$workdir/fourth.log")"

# ---------------------------------------------------------------------------
# An application that does not want to be remembered is not remembered.
# ---------------------------------------------------------------------------
stop_viewport
start_viewport "$workdir/fifth.log"

"$frontend" --persist 0 >"$workdir/transient.out" 2>&1
check "a transient share started" 0 $?
check "and answered with no restore data" no \
	"$(field "$workdir/transient.out" got_restore_data)"

# ---------------------------------------------------------------------------
# A window, which is the case the interface exists for and the one an id
# cannot carry: the window is closed and opened again between the two shares,
# so the view id it had the first time belongs to nothing by the second.
# ---------------------------------------------------------------------------
if [ -n "$paint_client" ]; then
	stop_viewport
	start_viewport "$workdir/window.log"
	start_client "$workdir/window.log"

	"$frontend" --types 2 --save "$workdir/window-token.bin" \
		>"$workdir/window.out" 2>&1
	check "a window share started" 0 $?
	check "it shared a window" 2 "$(field "$workdir/window.out" source_type)"

	stop_viewport
	start_viewport "$workdir/window-again.log"
	start_client "$workdir/window-again.log"

	"$frontend" --types 2 --restore "$workdir/window-token.bin" \
		>"$workdir/window-restore.out" 2>&1
	check "the window share was restored" 1 \
		"$(grep -c "as the application asked" "$workdir/window-again.log")"
	check "and it is the same window" 1 \
		"$(grep -c "sharing Window { app_id: \"$window_app_id\"" \
			"$workdir/window-again.log")"
	check "the restored window share started" 0 \
		"$(field "$workdir/window-restore.out" response)"

	# ---------------------------------------------------------------------
	# And the other half of the same feature: an application holding a token
	# can still ask to be asked. OBS clears its token when the user presses
	# the button that reopens the chooser, so what arrives is a request with
	# no restore data — and answering that from the old token would be a
	# "select a source" button that cannot select one.
	# ---------------------------------------------------------------------
	"$frontend" --types 1 --save "$workdir/new-token.bin" \
		>"$workdir/reselect.out" 2>&1
	# Still the one line from the restore above, and none from this request.
	check "a request with no token is not answered from one" 1 \
		"$(grep -c "as the application asked" "$workdir/window-again.log")"
	check "it went to the chooser instead" 1 \
		"$(grep -c "without asking" "$workdir/window-again.log")"
	check "it shared the monitor it was offered" 1 \
		"$(field "$workdir/reselect.out" source_type)"
	if cmp -s "$workdir/window-token.bin" "$workdir/new-token.bin"; then
		echo "FAIL: the new source did not replace the remembered one" >&2
		failures=$((failures + 1))
	else
		echo "ok: the new source replaced the remembered one"
	fi
else
	echo "skipped: the window cases need the paint client"
fi

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed" >&2
	exit 1
fi
echo "all checks passed"
