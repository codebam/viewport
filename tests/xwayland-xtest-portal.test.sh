#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# An X client's emulated input reaches this compositor's consent portal.
#
# XTEST used to die inside Xwayland. A synthetic event was delivered to X
# clients and to nothing else, because a rootless Xwayland has no path back to
# the compositor — which is why a controller could not move the pointer, why
# `xdotool` did nothing outside X11, and why the Steam Deck needs a uinput
# bridge at all. Since Xwayland 23.2 the server can translate XTEST into libei
# events instead, negotiated through `org.freedesktop.portal.RemoteDesktop`,
# which this compositor implements. Nothing about the X client changes; the
# whole feature lives in three things this test can see:
#
#   1. the compositor starts Xwayland with `-enable-ei-portal` (the switch that
#      turns the translation on) and with `DBUS_SESSION_BUS_ADDRESS` in its
#      environment (smithay clears everything but PATH and XDG_RUNTIME_DIR, so
#      without it the handshake has no bus to run on — and it fails on
#      Xwayland's side of a /dev/null stderr, silently);
#   2. a real XTEST request arrives at the RemoteDesktop implementation as a
#      session wanting the keyboard and the mouse;
#   3. the offer is narrowed to the mouse alone, and — with no page on this
#      headless desk that can draw a chooser — the request is refused rather
#      than granted without one.
#
# What is *not* asserted here is the grant half: Enter, the EIS socket, and the
# cursor moving. The chooser is only raised when a desktop page has committed a
# DMA-BUF, and a headless compositor has no device to hand a page; that half is
# covered by the acceptance run on a real seat (docs/protocols.md, "XTEST
# through the portal"). Everything above it runs on an ordinary runner.
#
#   tests/xwayland-xtest-portal.test.sh target/debug/viewport
set -u

viewport=${1:-build/viewport}

if [ ! -x "$viewport" ]; then
	echo "missing $viewport — build first" >&2
	exit 2
fi

if ! command -v dbus-run-session >/dev/null; then
	echo "SKIP: no dbus-run-session to hold the private bus"
	exit 77
fi

if ! command -v Xwayland >/dev/null; then
	echo "SKIP: no Xwayland for the compositor to spawn"
	exit 77
fi

# The switch this whole feature hangs on. An Xwayland without it refuses to
# start when handed the flag, so the compositor only offers it when the server
# advertises it — and then there is nothing to test.
if ! Xwayland -help 2>&1 | grep -q -- '-enable-ei-portal'; then
	echo "SKIP: this Xwayland does not advertise -enable-ei-portal"
	exit 77
fi

# XTest.h includes XInput.h, which is a header of its own package.
if ! pkg-config --exists xtst xi; then
	echo "SKIP: no libXtst and libXi to build the X client against"
	exit 77
fi

# Everything below runs on a session bus of its own: the compositor claims
# `org.freedesktop.impl.portal.desktop.viewport` at startup, and zbus fails the
# whole connection when the name is taken, so a run that joined the live bus
# would either test nothing or break the session it was started from.
if [ "${VIEWPORT_XTEST_TEST_BUS:-}" != yes ]; then
	export VIEWPORT_XTEST_TEST_BUS=yes
	exec dbus-run-session -- "$0" "$@"
fi

root=$(cd "$(dirname "$0")/.." && pwd)
workdir=$(mktemp -d)
viewport_pid=

cleanup() {
	if [ -n "$viewport_pid" ] && kill -0 "$viewport_pid" 2>/dev/null; then
		kill "$viewport_pid" 2>/dev/null
		wait "$viewport_pid" 2>/dev/null
	fi
	rm -rf "$workdir"
}
trap cleanup EXIT

# A portal routing configuration of this test's own, and the .portal file that
# names this compositor's backend. The alternative — using whatever the machine
# running the suite has installed — would make the test pass or fail on
# configuration it does not control.
mkdir -p "$workdir/config/xdg-desktop-portal"
cat >"$workdir/config/xdg-desktop-portal/portals.conf" <<'CONF'
[preferred]
default=viewport;gtk
org.freedesktop.impl.portal.RemoteDesktop=viewport;wlr
CONF
export XDG_CONFIG_DIRS="$workdir/config:${XDG_CONFIG_DIRS:-/etc/xdg}"
export XDG_DATA_DIRS="$root/data/portal-share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export XDG_CURRENT_DESKTOP=viewport

if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
	XDG_RUNTIME_DIR="$workdir/runtime"
	mkdir -p "$XDG_RUNTIME_DIR"
	chmod 700 "$XDG_RUNTIME_DIR"
	export XDG_RUNTIME_DIR
fi

client=$workdir/xtest-client
# shellcheck disable=SC2046 # pkg-config output is a word list on purpose
if ! cc "$root/tests/xwayland-xtest-client.c" -o "$client" $(pkg-config --cflags --libs x11 xtst xi); then
	echo "SKIP: the X client would not compile"
	exit 77
fi

log=$workdir/viewport.log
VIEWPORT_LOG='info,viewport::screencast=debug' \
	"$viewport" --headless --width 1280 --height 720 >"$log" 2>&1 &
viewport_pid=$!

display=
for _ in $(seq 1 100); do
	if ! kill -0 "$viewport_pid" 2>/dev/null; then
		echo "the compositor exited before Xwayland was up" >&2
		cat "$log" >&2
		exit 1
	fi
	display=$(sed -n 's/.*Xwayland ready on \(:[0-9]*\).*/\1/p' "$log" | head -1)
	[ -n "$display" ] && break
	sleep 0.2
done

if [ -z "$display" ]; then
	echo "Xwayland never came up" >&2
	cat "$log" >&2
	exit 1
fi

# The frontend is what the request is made to; without one on this bus the
# handshake cannot happen and there is nothing to assert. It is activated by
# the compositor watching for the name, so this is also the check that the bus
# this test built actually has a portal on it.
if ! grep -q "the portal frontend is now" "$log"; then
	for _ in $(seq 1 50); do
		grep -q "the portal frontend is now" "$log" && break
		sleep 0.2
	done
fi
if ! grep -q "the portal frontend is now" "$log"; then
	echo "SKIP: no xdg-desktop-portal frontend on this bus"
	exit 77
fi

fail=0
check() {
	local what=$1 expected=$2 actual=$3
	if [ "$expected" = "$actual" ]; then
		echo "ok: $what"
	else
		echo "FAIL: $what — expected [$expected], got [$actual]" >&2
		fail=1
	fi
}

# The Xwayland the compositor started, found by its display number rather than
# by pattern-matching a process name (this suite runs beside a live session).
xwayland_pid=
for candidate in /proc/[0-9]*; do
	pid=${candidate#/proc/}
	[ "$pid" = "$$" ] && continue
	exe=$(readlink "$candidate/exe" 2>/dev/null) || continue
	[ "${exe##*/}" = "Xwayland" ] || continue
	if tr '\0' ' ' <"$candidate/cmdline" 2>/dev/null | grep -q -- " $display "; then
		xwayland_pid=$pid
		break
	fi
done

check "the compositor started an Xwayland for $display" yes \
	"$([ -n "$xwayland_pid" ] && echo yes || echo no)"
if [ -z "$xwayland_pid" ]; then
	cat "$log" >&2
	exit 1
fi

check "it was given -enable-ei-portal" yes \
	"$(tr '\0' '\n' <"/proc/$xwayland_pid/cmdline" | grep -cx -- '-enable-ei-portal' | sed 's/^1$/yes/; s/^0$/no/')"

# The one that is invisible when it is missing: with no bus address Xwayland's
# liboeffis cannot reach the portal, and the failure is a debug line on a
# stderr this compositor discards.
check "it was given the session bus" yes \
	"$(tr '\0' '\n' <"/proc/$xwayland_pid/environ" | grep -c '^DBUS_SESSION_BUS_ADDRESS=' | sed 's/^1$/yes/; s/^0$/no/')"

DISPLAY=$display "$client"
status=$?
if [ "$status" -eq 2 ]; then
	echo "SKIP: this Xwayland has no XTEST extension after all"
	exit 77
fi
check "the XTEST client ran" 0 "$status"

# The handshake is asynchronous: Xwayland answers the XTEST request first and
# starts the portal conversation behind it, so the lines below appear a moment
# later, on a session that is still being set up.
for _ in $(seq 1 100); do
	grep -q "no desktop page is drawing" "$log" && break
	sleep 0.2
done

check "the request reached the RemoteDesktop implementation" 1 \
	"$(grep -c 'remote desktop: select devices 3' "$log")"

# What the person would be asked about: the request wants the keyboard too,
# and the offer is the mouse alone, because an X client's keys already reach
# this seat by another path and a chord that arrives twice fires twice.
check "the offer was narrowed to the mouse" 1 \
	"$(grep -c 'asked for the keyboard, mouse and is being offered the mouse' "$log")"

# Headless, nothing can draw a chooser, and a grant without consent is the one
# answer this compositor must never give.
check "with nothing to ask with, the request was refused" 1 \
	"$(grep -c 'no desktop page is drawing, so refusing' "$log")"
check "and nothing was granted" 0 \
	"$(grep -c 'may drive' "$log")"

if [ "$fail" -ne 0 ]; then
	echo "--- compositor log ---" >&2
	cat "$log" >&2
fi

exit "$fail"
