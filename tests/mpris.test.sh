#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# The bar's media widget, end to end, without a display or a music player.
#
# What is tested is the half no unit test reaches: a real player on a real bus,
# the compositor picking it up and reporting what is playing, and a button
# press travelling back. And the gating, which matters as much — a desktop with
# no media widget must not be following every player on the session.
#
# On a private session bus, because the compositor claims names at startup and
# a test that joined the live one would be watching whatever the user is
# actually listening to.
#
#   tests/mpris.test.sh target/debug/viewport target/debug/examples/mpris-player
set -u

viewport=${1:-target/debug/viewport}
player=${2:-target/debug/examples/mpris-player}

for binary in "$viewport" "$player"; do
	if [ ! -x "$binary" ]; then
		echo "missing $binary — build first" >&2
		exit 2
	fi
done
viewport=$(realpath "$viewport")
player=$(realpath "$player")

if ! command -v dbus-run-session >/dev/null; then
	echo "SKIP: no dbus-run-session to hold the private bus"
	exit 77
fi
if [ ! -f /etc/dbus-1/session.conf ]; then
	echo "SKIP: no /etc/dbus-1/session.conf for the private bus to start from"
	exit 77
fi

if [ "${VIEWPORT_MPRIS_TEST_BUS:-}" != yes ]; then
	export VIEWPORT_MPRIS_TEST_BUS=yes
	exec dbus-run-session -- "$0" "$viewport" "$player"
fi

workdir=$(mktemp -d)
viewport_pid=
player_pid=

# By PID, and named ones: pattern matching on "viewport" has killed a live
# session more than once and this runs on the same machine as one.
cleanup() {
	[ -n "$player_pid" ] && kill "$player_pid" 2>/dev/null
	[ -n "$viewport_pid" ] && kill "$viewport_pid" 2>/dev/null
	[ -n "$player_pid" ] && wait "$player_pid" 2>/dev/null
	[ -n "$viewport_pid" ] && wait "$viewport_pid" 2>/dev/null
	rm -rf "$workdir"
}
trap cleanup EXIT

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

start_viewport() {
	local config=$1 log=$2
	"$viewport" --headless --config "$config" >"$log" 2>&1 &
	viewport_pid=$!
	for _ in $(seq 1 100); do
		grep -q "control socket at" "$log" && return 0
		kill -0 "$viewport_pid" 2>/dev/null || break
		sleep 0.1
	done
	echo "the compositor never opened its socket; its log:" >&2
	tail -20 "$log" >&2
	exit 2
}

socket_of() {
	grep -o "control socket at .*" "$1" | tail -1 | cut -d' ' -f4-
}

# The player runs for the whole test: what changes between the two halves is
# whether the bar has a widget that asks about it.
"$player" Rhubarb >"$workdir/player.log" 2>&1 &
player_pid=$!
for _ in $(seq 1 100); do
	grep -q "^registered " "$workdir/player.log" && break
	kill -0 "$player_pid" 2>/dev/null || break
	sleep 0.1
done
check "the player took an MPRIS name" yes \
	"$(grep -q "^registered org.mpris.MediaPlayer2." "$workdir/player.log" && echo yes || echo no)"

# ---------------------------------------------------------------------------
# With a media widget on the bar.
# ---------------------------------------------------------------------------
echo '{ "bar_widgets": [ { "type": "mpris" } ], "tray": false }' >"$workdir/with.json"
start_viewport "$workdir/with.json" "$workdir/with.log"
socket=$(socket_of "$workdir/with.log")

"$viewport" msg --socket "$socket" --timeout 10 -t subscribe mpris.update \
	>"$workdir/events.json" 2>&1 &
subscribe_pid=$!
# A message the compositor sends on its own would be missed by a subscriber
# that started late, so the press below is what makes it send another one.
sleep 0.5
"$viewport" msg --socket "$socket" -t mpris.control --action play-pause \
	>/dev/null 2>&1
# Two updates arrive: the one the compositor sends when the widget appears,
# and the one the press causes. The second is the one this is about, so the
# wait is for the state to have moved rather than for any message at all.
for _ in $(seq 1 100); do
	grep -q '"paused"' "$workdir/events.json" && break
	sleep 0.1
done
kill "$subscribe_pid" 2>/dev/null
wait "$subscribe_pid" 2>/dev/null
update=$(grep '"mpris.update"' "$workdir/events.json" | tail -1)

check "a button press reaches the player over the bus" yes \
	"$(grep -q "^play-pause" "$workdir/player.log" && echo yes || echo no)"
check "and what is playing reaches the shell" yes \
	"$([ -n "$update" ] && echo yes || echo no)"
check "with the track's title" yes \
	"$(case "$update" in *'"Rhubarb"'*) echo yes ;; *) echo no ;; esac)"
check "and its artist, which arrives as a list and leaves as a line" yes \
	"$(case "$update" in *'"Aphex Twin"'*) echo yes ;; *) echo no ;; esac)"
check "and the state it is now in, which the press changed" yes \
	"$(case "$update" in *'"paused"'*) echo yes ;; *) echo no ;; esac)"
# Not decoration: the shell hides the buttons a player says it will not
# honour, and this player answers false for one of them on purpose.
check "and which buttons the player will honour" yes \
	"$(case "$update" in *'"can_go_previous":false'*) case "$update" in
		*'"can_go_next":true'*) echo yes ;; *) echo no ;; esac ;;
	*) echo no ;; esac)"

"$viewport" msg --socket "$socket" -t mpris.control --action next >/dev/null 2>&1
for _ in $(seq 1 100); do
	grep -q "^next" "$workdir/player.log" && break
	sleep 0.1
done
check "skipping reaches the player too" yes \
	"$(grep -q "^next" "$workdir/player.log" && echo yes || echo no)"

# A word the interface does not have is refused rather than passed through:
# this is a string from a page, and MPRIS has methods a bar has no business
# calling.
before=$(wc -l <"$workdir/player.log")
"$viewport" msg --socket "$socket" -t mpris.control --action openuri >/dev/null 2>&1
sleep 0.3
check "an action that is not one of the four does nothing" "$before" \
	"$(wc -l <"$workdir/player.log")"

kill "$viewport_pid" 2>/dev/null
wait "$viewport_pid" 2>/dev/null
viewport_pid=

# ---------------------------------------------------------------------------
# And without one. A desktop with no media widget should not be following the
# session's players at all — no connection, no thread, no match rules.
# ---------------------------------------------------------------------------
echo '{ "tray": false }' >"$workdir/without.json"
start_viewport "$workdir/without.json" "$workdir/without.log"
socket=$(socket_of "$workdir/without.log")

# Watched for a while and then stopped, rather than waited on: what is being
# checked is that nothing arrives, and nothing arriving is not an event to wait
# for.
"$viewport" msg --socket "$socket" --timeout 0 -t subscribe mpris.update \
	>"$workdir/quiet.json" 2>&1 &
subscribe_pid=$!
sleep 2
kill "$subscribe_pid" 2>/dev/null
wait "$subscribe_pid" 2>/dev/null
check "a bar with no media widget is told nothing about players" no \
	"$(grep -q '"mpris.update"' "$workdir/quiet.json" && echo yes || echo no)"

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed; the compositor's log:" >&2
	tail -30 "$workdir/with.log" >&2
	exit 1
fi
echo "all media checks passed"
