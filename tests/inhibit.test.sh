#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Keeping the screen awake from the bus, end to end.
#
# What no unit test reaches: the name actually claimed, the interface answering
# at both paths a browser tries, and — the part that makes granting a hold safe
# — a hold released when the program that took it goes away without saying so.
# That last one is free to test here, because `gdbus call` exits the moment it
# has its answer: every call is a connection that dies immediately, which is
# exactly the case a killed video player produces.
#
# On a private session bus, because the compositor claims names at startup and
# a test that joined the live one would be fighting whatever screensaver the
# machine already has.
#
#   tests/inhibit.test.sh target/debug/viewport target/debug/examples/inhibit-holder
set -u

viewport=${1:-target/debug/viewport}
holder=${2:-target/debug/examples/inhibit-holder}
if [ ! -x "$viewport" ]; then
	echo "missing $viewport — build first" >&2
	exit 2
fi
viewport=$(realpath "$viewport")
[ -x "$holder" ] && holder=$(realpath "$holder")

if ! command -v dbus-run-session >/dev/null; then
	echo "SKIP: no dbus-run-session to hold the private bus"
	exit 77
fi
if [ ! -f /etc/dbus-1/session.conf ]; then
	echo "SKIP: no /etc/dbus-1/session.conf for the private bus to start from"
	exit 77
fi
if ! command -v gdbus >/dev/null; then
	echo "SKIP: no gdbus to call the interface with"
	exit 77
fi

if [ "${VIEWPORT_INHIBIT_TEST_BUS:-}" != yes ]; then
	export VIEWPORT_INHIBIT_TEST_BUS=yes
	exec dbus-run-session -- "$0" "$viewport" "$holder"
fi

workdir=$(mktemp -d)
viewport_pid=
holder_pid=

# By PID, and named ones: pattern matching on "viewport" has killed a live
# session more than once and this runs on the same machine as one.
cleanup() {
	[ -n "$holder_pid" ] && kill "$holder_pid" 2>/dev/null
	[ -n "$viewport_pid" ] && kill "$viewport_pid" 2>/dev/null
	[ -n "$holder_pid" ] && wait "$holder_pid" 2>/dev/null
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
	grep -q "claimed org.freedesktop.ScreenSaver" "$log" && break
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done
check "the compositor claimed the screensaver name" yes \
	"$(grep -q "claimed org.freedesktop.ScreenSaver" "$log" && echo yes || echo no)"

call() {
	local path=$1 method=$2
	shift 2
	gdbus call --session --dest org.freedesktop.ScreenSaver \
		--object-path "$path" \
		--method "org.freedesktop.ScreenSaver.$method" "$@" 2>&1
}

# The specification's path, and GNOME's, because software asks at both.
for path in /org/freedesktop/ScreenSaver /ScreenSaver; do
	cookie=$(call "$path" Inhibit "test-player" "playing video")
	check "Inhibit at $path answers a cookie" yes \
		"$(case "$cookie" in '(uint32 '*) echo yes ;; *) echo no ;; esac)"
	check "and the cookie is never zero at $path" yes \
		"$(case "$cookie" in '(uint32 0,)') echo no ;; *) echo yes ;; esac)"
done

check "the hold is named in the log with who took it" yes \
	"$(grep -q "test-player is holding the screen awake (playing video)" "$log" &&
		echo yes || echo no)"

# gdbus is gone the moment it printed that cookie, and nothing called
# UnInhibit. This is the case a video player killed mid-film produces, and a
# compositor that waited for a release it will never get keeps the screens lit
# for the rest of the session.
for _ in $(seq 1 100); do
	grep -q "went away; releasing its hold" "$log" && break
	sleep 0.1
done
check "a hold dies with the connection that took it" yes \
	"$(grep -q "test-player went away; releasing its hold (playing video)" "$log" &&
		echo yes || echo no)"

# Answered rather than errored, both of them: a client that gets an error back
# sometimes decides the whole interface is missing and stops inhibiting too.
active=$(call /org/freedesktop/ScreenSaver GetActive)
check "GetActive answers, and answers false" "(false,)" "$active"
active_time=$(call /org/freedesktop/ScreenSaver GetActiveTime)
check "GetActiveTime answers zero" "(uint32 0,)" "$active_time"

# A cookie belongs to whoever took it. This one is from a connection that has
# already gone, so releasing it from a new one must do nothing at all — the log
# says so and the call still succeeds, because the state the caller wanted is
# the state it has.
released=$(call /org/freedesktop/ScreenSaver UnInhibit 1)
check "UnInhibit answers" "()" "$released"

# And somebody saying they are there, which has to reach the same place a
# keypress does.
simulated=$(call /org/freedesktop/ScreenSaver SimulateUserActivity)
check "SimulateUserActivity answers" "()" "$simulated"

# The portal interface, on the portal's own name and object.
inhibit_version=$(gdbus call --session \
	--dest org.freedesktop.impl.portal.desktop.viewport \
	--object-path /org/freedesktop/portal/desktop \
	--method org.freedesktop.DBus.Properties.Get \
	org.freedesktop.impl.portal.Inhibit version 2>&1)
check "the inhibit portal is on the bus at version 1" "(<uint32 1>,)" "$inhibit_version"

# And the whole point of the interface: a hold stops the deadline.
#
# A second compositor, with a blank deadline of a second, because the first one
# has none — the policy is off unless the config file asks for it, and a test
# that asserted on a timer nobody set would pass with the registry unplugged.
# The holder is a program rather than a `gdbus call` because a hold hangs on
# the connection that took it, and a command-line call's connection is gone
# before the next line of this script runs.
if [ -x "$holder" ]; then
	kill "$viewport_pid" 2>/dev/null
	wait "$viewport_pid" 2>/dev/null
	viewport_pid=

	echo '{ "idle": { "blank_after": 1 } }' >"$workdir/blank.json"
	blanklog="$workdir/blank.log"
	"$viewport" --headless --config "$workdir/blank.json" >"$blanklog" 2>&1 &
	viewport_pid=$!
	for _ in $(seq 1 100); do
		grep -q "claimed org.freedesktop.ScreenSaver" "$blanklog" && break
		kill -0 "$viewport_pid" 2>/dev/null || break
		sleep 0.1
	done

	"$holder" mpv "playing video" >"$workdir/holder.log" 2>&1 &
	holder_pid=$!
	for _ in $(seq 1 100); do
		grep -q "^held " "$workdir/holder.log" && break
		kill -0 "$holder_pid" 2>/dev/null || break
		sleep 0.1
	done
	check "the holder took a hold" yes \
		"$(grep -q "^held " "$workdir/holder.log" && echo yes || echo no)"

	# Four seconds of a one-second deadline. Without the hold this blanks on
	# the first tick.
	sleep 4
	check "a held screen does not blank" no \
		"$(grep -q "blanking" "$blanklog" && echo yes || echo no)"

	# Killed rather than asked to stop: a player that crashed is the case the
	# whole owner-watching thread exists for, and the deadline has to come
	# back for it.
	kill "$holder_pid" 2>/dev/null
	wait "$holder_pid" 2>/dev/null
	holder_pid=
	for _ in $(seq 1 100); do
		grep -q "blanking" "$blanklog" && break
		sleep 0.1
	done
	check "and blanks once the holder is gone" yes \
		"$(grep -q "blanking" "$blanklog" && echo yes || echo no)"
	log=$blanklog
else
	echo "skipping the deadline checks: no inhibit-holder to run them with"
fi

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed; the compositor's log:" >&2
	tail -30 "$log" >&2
	exit 1
fi
echo "all inhibit checks passed"
