#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# What is kept after a notification's popup has gone.
#
# The unit tests cover the store's own rules — newest first, a replacement
# taking the place of what it replaces, the limit. What they cannot show is the
# path a real notification takes: on the bus as `Notify`, through the D-Bus
# thread, into the history, and back out as a `notification.history` to
# whatever is subscribed. Every one of those is a seam between two pieces that
# are tested separately, and the centre is empty if any of them is missed.
#
# So this sends the notification the way an application does, with `gdbus`, and
# reads the history back the way the shell does, with `viewport msg`.
#
# On a private session bus, because the compositor claims the notification name
# at startup and a test that joined the live one would be fighting whatever is
# already holding it — mako, dunst, or the compositor the user is sitting in
# front of.
#
#   tests/notification-centre.test.sh target/debug/viewport
set -u

viewport=${1:-target/debug/viewport}
if [ ! -x "$viewport" ]; then
	echo "missing $viewport — build first" >&2
	exit 2
fi
viewport=$(realpath "$viewport")

if ! command -v dbus-run-session >/dev/null; then
	echo "SKIP: no dbus-run-session to hold the private bus"
	exit 77
fi
if [ ! -f /etc/dbus-1/session.conf ]; then
	echo "SKIP: no /etc/dbus-1/session.conf for the private bus to start from"
	exit 77
fi
if ! command -v gdbus >/dev/null; then
	echo "SKIP: no gdbus to send the notification with"
	exit 77
fi

if [ "${VIEWPORT_NOTIFICATION_TEST_BUS:-}" != yes ]; then
	export VIEWPORT_NOTIFICATION_TEST_BUS=yes
	exec dbus-run-session -- "$0" "$viewport"
fi

workdir=$(mktemp -d)
viewport_pid=

# By PID, and only the one this test started: pattern matching on "viewport"
# has killed a live session more than once and this runs on the same machine
# as one.
cleanup() {
	[ -n "$viewport_pid" ] && kill "$viewport_pid" 2>/dev/null
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

log="$workdir/viewport.log"
"$viewport" --headless >"$log" 2>&1 &
viewport_pid=$!
for _ in $(seq 1 100); do
	grep -q "control socket at " "$log" && break
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done

socket=$(grep -o "control socket at .*" "$log" | tail -1 | cut -d' ' -f4-)
if [ -z "$socket" ]; then
	echo "the compositor did not name its control socket" >&2
	tail -20 "$log" >&2
	exit 2
fi

# The expiry is 5000 rather than the -1 an application usually sends: gdbus
# reads a bare `-1` as one of its own options and prints its usage instead of
# calling anything. What the number is does not matter here — the popup is
# never drawn, because nothing is drawing.
notify() {
	local summary=$1 body=$2 replaces=${3:-0}
	gdbus call --session --dest org.freedesktop.Notifications \
		--object-path /org/freedesktop/Notifications \
		--method org.freedesktop.Notifications.Notify \
		"test-app" "$replaces" "" "$summary" "$body" \
		"[]" "{}" 5000 2>&1
}

# The id the server handed out, which is what every request below names.
#
# Read out of `(uint32 7,)` by naming the type: the first run of digits in that
# string is the 32 in `uint32`, so every request went to a notification that
# did not exist and every check about one still passed by accident.
id_of() { echo "$1" | sed -n 's/.*uint32 \([0-9]\{1,\}\).*/\1/p'; }

# What the shell does when the centre opens: ask, and read the answer.
#
# The subscription is opened first, because the answer is an event broadcast to
# whoever is listening rather than a reply on the asking connection — so a
# subscribe started after the request would miss it. A subscribe does not end
# on its own, so it is killed by pid once the line has landed.
history_now() {
	local out="$workdir/history.json"
	: >"$out"
	"$viewport" msg --socket "$socket" --timeout 0 -t subscribe \
		notification.history >"$out" 2>&1 &
	local sub=$!
	sleep 0.3
	"$viewport" msg --socket "$socket" -t notification.list >/dev/null 2>&1
	for _ in $(seq 1 50); do
		[ -s "$out" ] && break
		sleep 0.1
	done
	kill "$sub" 2>/dev/null
	wait "$sub" 2>/dev/null
	cat "$out"
}

first=$(notify "the first one" "a body")
check "the bus answered with an id" yes \
	"$([ -n "$(id_of "$first")" ] && echo yes || echo no)"
first_id=$(id_of "$first")

second=$(notify "the second one" "another body")
second_id=$(id_of "$second")

history=$(history_now)
check "both notifications are in the history" yes \
	"$(echo "$history" | grep -q "the first one" &&
		echo "$history" | grep -q "the second one" && echo yes || echo no)"

# Newest first, which is the order a centre shows them in. Checked by which
# summary the reply names first rather than by parsing it: the order is the
# claim, and jq is not a dependency of this tree.
check "newest first" yes \
	"$(echo "$history" | grep -o "the first one\|the second one" | head -1 |
		grep -q "the second one" && echo yes || echo no)"

check "and each entry carries the time it arrived" yes \
	"$(echo "$history" | grep -q '"at":[0-9]\{10\}' && echo yes || echo no)"

# A popup that expired stays: that is the case the centre exists for.
"$viewport" msg --socket "$socket" -t notification.expire --id "$first_id" \
	>/dev/null 2>&1
history=$(history_now)
check "an expired notification stays in the history" yes \
	"$(echo "$history" | grep -q "the first one" && echo yes || echo no)"

# One acted on does not: a mail opened is not something to go back to.
"$viewport" msg --socket "$socket" -t notification.action --id "$second_id" \
	--action default >/dev/null 2>&1
history=$(history_now)
check "one that was acted on leaves it" yes \
	"$(echo "$history" | grep -q "the second one" && echo no || echo yes)"

# A sender replacing its own notification replaces the entry rather than
# stacking beside it — the progress-bar case, which fills a centre by itself
# if it is got wrong.
third=$(notify "counting" "1 of 10")
third_id=$(id_of "$third")
notify "counting" "10 of 10" "$third_id" >/dev/null
history=$(history_now)
check "a replacement does not stack a second entry" 1 \
	"$(echo "$history" | grep -o "counting" | wc -l)"
check "and what is kept is the newer text" yes \
	"$(echo "$history" | grep -q "10 of 10" && echo yes || echo no)"

# The centre's own two verbs.
"$viewport" msg --socket "$socket" -t notification.forget --id "$first_id" \
	>/dev/null 2>&1
history=$(history_now)
check "forgetting one takes that one out" yes \
	"$(echo "$history" | grep -q "the first one" && echo no || echo yes)"

"$viewport" msg --socket "$socket" -t notification.forget >/dev/null 2>&1
history=$(history_now)
check "and forgetting everything empties it" yes \
	"$(echo "$history" | grep -q "counting" && echo no || echo yes)"

exit $((failures > 0))
