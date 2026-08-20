#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Dark mode, end to end, without a toolkit to look at.
#
# The compositor answers `org.freedesktop.impl.portal.Settings` itself, and the
# whole reason it is a service rather than a file is that a running application
# follows a toggle rather than waiting to be restarted. That means the signal,
# and the signal is what no unit test can check: what a client actually hears
# on the bus when somebody presses the chord.
#
# Both namespaces are checked because clients read one or the other and not
# both — libadwaita reads the freedesktop scheme, GTK3 reads the GNOME theme
# name — and a session that announced one of them looked, from the other half
# of the desktop, exactly like a portal that does not work.
#
# On a private session bus, because the compositor claims names at startup and
# this toggles the colour scheme, which on the live bus is somebody's desktop.
#
#   tests/appearance.test.sh target/debug/viewport
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
if ! command -v dbus-monitor >/dev/null; then
	echo "SKIP: no dbus-monitor to listen with"
	exit 77
fi

if [ "${VIEWPORT_APPEARANCE_TEST_BUS:-}" != yes ]; then
	export VIEWPORT_APPEARANCE_TEST_BUS=yes
	exec dbus-run-session -- "$0" "$viewport"
fi

workdir=$(mktemp -d)
viewport_pid=
monitor_pid=

# By PID, and named ones: pattern matching on "viewport" has killed a live
# session more than once and this runs on the same machine as one.
cleanup() {
	[ -n "$monitor_pid" ] && kill "$monitor_pid" 2>/dev/null
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

# Started dark, and switched by rewriting the file it is watching. The chord
# would be the obvious trigger and cannot be used: `input.key` presses a key
# through the compositor's own built-in chord table, which is VT switching and
# the two ungrab chords, and `appearance toggle` is a *configured* binding —
# those are matched inside the real input path and nothing on the control
# socket reaches them. Live config reload ends in the same `set_dark`.
config="$workdir/config.json"
echo '{ "dark_mode": true }' >"$config"

log="$workdir/viewport.log"
"$viewport" --headless --config "$config" --watch-config >"$log" 2>&1 &
viewport_pid=$!
for _ in $(seq 1 100); do
	grep -q "portals up" "$log" && break
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done
check "the portals came up" yes "$(grep -q "portals up" "$log" && echo yes || echo no)"
check "and the session starts dark" yes \
	"$(grep -q "color-scheme=1" "$log" && echo yes || echo no)"

socket=$(grep -o "control socket at .*" "$log" | tail -1 | cut -d' ' -f4-)
if [ -z "$socket" ]; then
	echo "the compositor did not name its control socket" >&2
	exit 2
fi

# Listening before pressing anything, or the signal is emitted into a bus
# nobody is watching and the test races its own trigger.
dbus-monitor --session "interface='org.freedesktop.impl.portal.Settings'" \
	>"$workdir/bus.log" 2>&1 &
monitor_pid=$!
sleep 0.5

echo '{ "dark_mode": false }' >"$config"

for _ in $(seq 1 150); do
	grep -q "color-scheme now light" "$log" && break
	sleep 0.1
done
check "the reload switched the session to light" yes \
	"$(grep -q "color-scheme now light" "$log" && echo yes || echo no)"

sleep 0.5
kill "$monitor_pid" 2>/dev/null
monitor_pid=

announced() {
	# One dbus-monitor record per signal, so the namespace and the key of a
	# single announcement are within a few lines of each other.
	grep -A 4 "member=SettingChanged" "$workdir/bus.log" |
		grep -A 2 "string \"$1\"" | grep -c "string \"$2\""
}

# libadwaita, Firefox and anything else reading the freedesktop namespace.
check "the freedesktop scheme is announced" yes \
	"$([ "$(announced org.freedesktop.appearance color-scheme)" -ge 1 ] && echo yes || echo no)"
# GTK3, which reads the GNOME namespace and never looks at the one above.
check "the GNOME scheme is announced" yes \
	"$([ "$(announced org.gnome.desktop.interface color-scheme)" -ge 1 ] && echo yes || echo no)"
# And the theme name, because GTK3 follows that rather than the scheme.
check "the GTK theme name is announced" yes \
	"$([ "$(announced org.gnome.desktop.interface gtk-theme)" -ge 1 ] && echo yes || echo no)"

# What a client that re-reads on the signal is handed. Announcing one value and
# answering with another is worse than announcing nothing.
scheme=$(dbus-send --session --print-reply --dest=org.freedesktop.impl.portal.desktop.viewport \
	/org/freedesktop/portal/desktop org.freedesktop.impl.portal.Settings.Read \
	string:org.gnome.desktop.interface string:color-scheme 2>/dev/null |
	grep -o '"prefer-[a-z]*"')
check "and a read agrees with what was announced" '"prefer-light"' "$scheme"

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed; the compositor's log:" >&2
	tail -30 "$log" >&2
	echo "the bus:" >&2
	tail -40 "$workdir/bus.log" >&2
	exit 1
fi
echo "all appearance checks passed"
