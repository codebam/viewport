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
# It runs on a session bus of its own. The compositor claims
# org.freedesktop.impl.portal.desktop.viewport, and zbus fails the whole
# connection when the name is taken — so on a machine already running viewport,
# a test that joined the live bus would test nothing and a test that replaced
# the name would break the session it was running in.
#
#   tests/screencast-restore.test.sh target/debug/viewport \
#     target/debug/examples/portal-frontend
set -u

viewport=${1:-target/debug/viewport}
frontend=${2:-target/debug/examples/portal-frontend}

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
	exec dbus-run-session -- "$0" "$viewport" "$frontend"
fi

workdir=$(mktemp -d)
viewport_pid=

# By PID only. Pattern matching on "viewport" has killed a live session more
# than once, and this script runs on the same machine as one.
stop_viewport() {
	[ -n "$viewport_pid" ] && kill "$viewport_pid" 2>/dev/null
	wait "$viewport_pid" 2>/dev/null
	viewport_pid=
}
cleanup() {
	stop_viewport
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

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed" >&2
	exit 1
fi
echo "all checks passed"
