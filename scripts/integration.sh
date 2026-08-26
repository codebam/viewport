#!/usr/bin/env bash
# Run the Wayland integration tests against a compositor binary.
#
#   scripts/integration.sh target/debug/viewport
#   scripts/integration.sh result/bin/viewport
#
# These are the tests that define parity between the two compositors — see
# "Parity, and when the C tree goes" in docs/RUST-REWRITE.md. They take the
# binary as an argument and do not care which language wrote it: they start it
# headless, drive it with real clients over a real socket, and look at what
# comes back.
#
# These used to be meson targets, which is why this script compiles the clients
# by hand: meson.build declared wlroots and wpe-webkit at the top level, so
# `meson setup` failed on a machine that had neither, even for targets that
# used neither. meson is gone with the C compositor and the reasoning outlived
# it — the clients link wayland-client and the generated marshalling code and
# nothing else, which is what keeps this suite on a runner with no compositor
# dependencies at all.
set -euo pipefail

VIEWPORT=${1:?usage: integration.sh PATH-TO-VIEWPORT}
if [ ! -x "$VIEWPORT" ]; then
	echo "not an executable: $VIEWPORT" >&2
	exit 2
fi
VIEWPORT=$(realpath "$VIEWPORT")

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

protocols=$(pkg-config --variable=pkgdatadir wayland-protocols)

# The same list meson.build builds these clients from. Client side, so both
# halves: the header to compile against and the marshalling code to link.
generate() {
	local name=$1 xml=$2
	wayland-scanner client-header "$xml" "$work/$name-client-protocol.h"
	wayland-scanner private-code "$xml" "$work/$name-protocol.c"
	echo "$work/$name-protocol.c"
}

sources=()
sources+=("$(generate xdg-shell "$protocols/stable/xdg-shell/xdg-shell.xml")")
sources+=("$(generate ext-foreign-toplevel-list-v1 \
	"$protocols/staging/ext-foreign-toplevel-list/ext-foreign-toplevel-list-v1.xml")")
sources+=("$(generate ext-image-capture-source-v1 \
	"$protocols/staging/ext-image-capture-source/ext-image-capture-source-v1.xml")")
sources+=("$(generate ext-image-copy-capture-v1 \
	"$protocols/staging/ext-image-copy-capture/ext-image-copy-capture-v1.xml")")
sources+=("$(generate ext-session-lock-v1 \
	"$protocols/staging/ext-session-lock/ext-session-lock-v1.xml")")
# Vendored, as on the server side: the frame probe has to stay visible on a
# chosen output even while a game is fullscreen on another.
sources+=("$(generate wlr-layer-shell-unstable-v1 \
	"$root/protocols/wlr-layer-shell-unstable-v1.xml")")
# The two wlr protocols the server dispatch is hand-written for, vendored here
# at the version the server binds; and the staging workspace protocol, which
# like the other staging protocols comes from wayland-protocols.
sources+=("$(generate wlr-foreign-toplevel-management-unstable-v1 \
	"$root/protocols/wlr-foreign-toplevel-management-unstable-v1.xml")")
sources+=("$(generate wlr-output-management-unstable-v1 \
	"$root/protocols/wlr-output-management-unstable-v1.xml")")
sources+=("$(generate ext-workspace-v1 \
	"$protocols/staging/ext-workspace/ext-workspace-v1.xml")")

for client in paint capture lock foreign-toplevel output-management workspace; do
	# shellcheck disable=SC2046 # pkg-config output is a word list on purpose
	cc -std=c11 -Wall -Wextra -Wno-unused-parameter \
		-I"$work" \
		-o "$work/$client-client" \
		"$root/tests/$client-client.c" "${sources[@]}" \
		$(pkg-config --cflags --libs wayland-client) -lm
done

# The spec's 0700 directory. Without it these fall back to a world-writable
# /tmp shared with every other process on the machine, socket and lock file
# included.
if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
	XDG_RUNTIME_DIR="$work/runtime"
	mkdir -p "$XDG_RUNTIME_DIR"
	chmod 700 "$XDG_RUNTIME_DIR"
	export XDG_RUNTIME_DIR
fi

failed=0
run() {
	local name=$1
	shift
	echo "=== $name"
	local status=0
	"$@" || status=$?
	# 77 is the automake convention a test uses to say it could not run at
	# all, which is not the same as a failure and must not read as one.
	if [ "$status" -eq 0 ]; then
		echo "=== $name: pass"
	elif [ "$status" -eq 77 ]; then
		echo "=== $name: skipped"
	else
		echo "=== $name: FAIL" >&2
		failed=1
	fi
}

# Every test, then the verdict — a run that stops at the first failure tells
# you less than one that says which of them are broken.
run capture-tiling "$root/tests/capture.test.sh" \
	"$VIEWPORT" "$work/paint-client" "$work/capture-client" tiling
run capture-scrolling "$root/tests/capture.test.sh" \
	"$VIEWPORT" "$work/paint-client" "$work/capture-client" scrolling
run output-order "$root/tests/output-order.test.sh" "$VIEWPORT"

# The screencast frontend is Rust — it speaks D-Bus rather than Wayland, so it
# is a cargo example beside the compositor's own crate rather than one more
# client compiled by hand up there. Skipped rather than failed where there is
# no toolchain: this suite also runs against a binary built by nix on a machine
# that has no cargo at all.
frontend="$root/target/debug/examples/portal-frontend"
if [ ! -x "$frontend" ] && command -v cargo >/dev/null; then
	(cd "$root" && cargo build -p viewport --example portal-frontend) || true
fi
if [ -x "$frontend" ]; then
	run screencast-restore "$root/tests/screencast-restore.test.sh" \
		"$VIEWPORT" "$frontend" "$work/paint-client"
else
	echo "=== screencast-restore: skipped, no portal-frontend to run it with"
fi
# The tray item is a cargo example for the same reason the frontend is: it
# speaks D-Bus rather than Wayland. Same skip, for a run against a binary built
# by nix on a machine with no cargo.
tray_item="$root/target/debug/examples/tray-item"
if [ ! -x "$tray_item" ] && command -v cargo >/dev/null; then
	(cd "$root" && cargo build -p viewport --example tray-item) || true
fi
if [ -x "$tray_item" ]; then
	run tray "$root/tests/tray.test.sh" "$VIEWPORT" "$tray_item"
else
	echo "=== tray: skipped, no tray-item to run it with"
fi

# The media player is a cargo example for the same reason again.
mpris_player="$root/target/debug/examples/mpris-player"
if [ ! -x "$mpris_player" ] && command -v cargo >/dev/null; then
	(cd "$root" && cargo build -p viewport --example mpris-player) || true
fi
if [ -x "$mpris_player" ]; then
	run mpris "$root/tests/mpris.test.sh" "$VIEWPORT" "$mpris_player"
else
	echo "=== mpris: skipped, no mpris-player to run it with"
fi

# Needs dbus-monitor and a private bus, and skips (77) without either.
run appearance "$root/tests/appearance.test.sh" "$VIEWPORT"

# Needs gdbus and a private bus, and skips (77) without either. The holder is a
# cargo example for the reason the frontend and the tray item are: it speaks
# D-Bus rather than Wayland. Without it the interface checks still run and the
# two deadline checks — which need a connection that stays open — are skipped.
inhibit_holder="$root/target/debug/examples/inhibit-holder"
if [ ! -x "$inhibit_holder" ] && command -v cargo >/dev/null; then
	(cd "$root" && cargo build -p viewport --example inhibit-holder) || true
fi
run inhibit "$root/tests/inhibit.test.sh" "$VIEWPORT" "$inhibit_holder"

# Needs wl-copy and wl-paste, and skips (77) without them.
run clipboard "$root/tests/clipboard.test.sh" "$VIEWPORT"

# The order the compositor tears itself down in, with a python stub standing in
# for the shell process. Skips (77) without python3.
run shell-stop "$root/tests/shell-stop.test.sh" "$VIEWPORT"

# What is kept after a popup has gone: the notification on the bus, and the
# history read back the way the shell's centre reads it. The helper makes two
# Notify calls on one connection so the second is allowed to replace the first;
# two gdbus processes intentionally have different notification ownership.
notification_sender="$root/target/debug/examples/notification-sender"
if [ ! -x "$notification_sender" ] && command -v cargo >/dev/null; then
	(cd "$root" && cargo build -p viewport --example notification-sender) || true
fi
if [ -x "$notification_sender" ]; then
	run notification-centre "$root/tests/notification-centre.test.sh" \
		"$VIEWPORT" "$notification_sender"
else
	echo "=== notification-centre: skipped, no notification-sender to run it with"
fi

run session-lock-crash "$root/tests/lock.test.sh" \
	"$VIEWPORT" "$work/lock-client" "$work/capture-client" "$work/paint-client"
run session-lock-takeover "$root/tests/lock-takeover.test.sh" \
	"$VIEWPORT" "$work/lock-client"
# The other lock screen: the one this compositor draws itself, and what it does
# when nothing can draw it. No lock client, because there is no second process
# in that story at all.
run session-lock-builtin "$root/tests/lock-builtin.test.sh" \
	"$VIEWPORT" "$work/capture-client" "$work/paint-client"
run foreign-toplevel "$root/tests/foreign-toplevel.test.sh" \
	"$VIEWPORT" "$work/foreign-toplevel-client" "$work/paint-client"
run output-management "$root/tests/output-management.test.sh" \
	"$VIEWPORT" "$work/output-management-client"
run workspace "$root/tests/workspace.test.sh" \
	"$VIEWPORT" "$work/workspace-client"

# Compiles its own client, because it is the one test here that needs X11 and
# the suite is deliberately buildable without it. Skips where there is none.
run xwayland-focus "$root/tests/xwayland-focus.test.sh" "$VIEWPORT"
run xwayland-scale "$root/tests/xwayland-scale.test.sh" "$VIEWPORT"

exit "$failed"
