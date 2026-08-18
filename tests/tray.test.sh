#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# The system tray, end to end, without a display.
#
# What is tested is the half that cannot be tested in Rust: a real application
# registering a real item on a real bus, the compositor fetching what it looks
# like, and a click travelling back the other way. The unit tests cover the
# shapes — a key, a tooltip, a PNG — and none of them proves that anything ever
# reached the bus.
#
# It runs on a session bus of its own. The compositor claims
# org.kde.StatusNotifierWatcher, and on a machine already running a desktop
# that name is taken: a test that joined the live bus would test nothing, and
# one that took the name would break the session it was running in.
#
#   tests/tray.test.sh target/debug/viewport target/debug/examples/tray-item
set -u

viewport=${1:-target/debug/viewport}
item=${2:-target/debug/examples/tray-item}

for binary in "$viewport" "$item"; do
	if [ ! -x "$binary" ]; then
		echo "missing $binary — build first" >&2
		exit 2
	fi
done
viewport=$(realpath "$viewport")
item=$(realpath "$item")

if ! command -v dbus-run-session >/dev/null; then
	echo "SKIP: no dbus-run-session to hold the private bus"
	exit 77
fi
if [ ! -f /etc/dbus-1/session.conf ]; then
	echo "SKIP: no /etc/dbus-1/session.conf for the private bus to start from"
	exit 77
fi

# Everything below runs inside the private bus: the compositor claims its names
# at startup and there is no way to hand it a bus afterwards.
if [ "${VIEWPORT_TRAY_TEST_BUS:-}" != yes ]; then
	export VIEWPORT_TRAY_TEST_BUS=yes
	exec dbus-run-session -- "$0" "$viewport" "$item"
fi

workdir=$(mktemp -d)
viewport_pid=
item_pid=

# By PID, and named ones at that. Pattern matching on "viewport" has killed a
# live session more than once, and this script runs on the same machine as one.
cleanup() {
	[ -n "$item_pid" ] && kill "$item_pid" 2>/dev/null
	[ -n "$viewport_pid" ] && kill "$viewport_pid" 2>/dev/null
	[ -n "$item_pid" ] && wait "$item_pid" 2>/dev/null
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
	grep -q "claimed org.kde.StatusNotifierWatcher" "$log" && break
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done
if ! grep -q "claimed org.kde.StatusNotifierWatcher" "$log"; then
	echo "the compositor never claimed the watcher name; its log:" >&2
	tail -20 "$log" >&2
	exit 2
fi

socket=$(grep -o "control socket at .*" "$log" | tail -1 | cut -d' ' -f4-)
if [ -z "$socket" ]; then
	echo "the compositor did not name its control socket" >&2
	exit 2
fi

# Watching before registering, so the update cannot arrive between the two.
"$viewport" msg --socket "$socket" --timeout 10 -t subscribe tray.update \
	>"$workdir/events.json" 2>&1 &
subscribe_pid=$!
sleep 0.5

"$item" Nextcloud >"$workdir/item.log" 2>&1 &
item_pid=$!
for _ in $(seq 1 100); do
	grep -q "^registered " "$workdir/item.log" && break
	kill -0 "$item_pid" 2>/dev/null || break
	sleep 0.1
done
service=$(grep -o "^registered .*" "$workdir/item.log" | cut -d' ' -f2)
check "the item registered itself" yes "$([ -n "$service" ] && echo yes || echo no)"

# The update the shell would have drawn.
for _ in $(seq 1 100); do
	grep -q '"tray.update"' "$workdir/events.json" && break
	sleep 0.1
done
kill "$subscribe_pid" 2>/dev/null
wait "$subscribe_pid" 2>/dev/null

update=$(grep '"tray.update"' "$workdir/events.json" | tail -1)
check "the compositor forwarded a tray" yes \
	"$([ -n "$update" ] && echo yes || echo no)"
check "with the item's own title on it" yes \
	"$(case "$update" in *'"Nextcloud"'*) echo yes ;; *) echo no ;; esac)"
check "and its tooltip, both halves" yes \
	"$(case "$update" in *'Test item'*'with a body'*) echo yes ;; *) echo no ;; esac)"
# The pixmap the item published, encoded here rather than passed through: an
# icon name would depend on which themes are installed, and a file:// path is
# not something the shell can draw.
check "and its pixmap as a PNG data URL" yes \
	"$(case "$update" in *'data:image/png;base64,'*) echo yes ;; *) echo no ;; esac)"

id="$service/StatusNotifierItem"
check "the item is named by its owner and its path" yes \
	"$(case "$update" in *"$id"*) echo yes ;; *) echo no ;; esac)"

# And back the other way: what a click on the bar does.
"$viewport" msg --socket "$socket" -t tray.activate --id "$id" \
	--button primary --x 40 --y 30 >/dev/null 2>&1
"$viewport" msg --socket "$socket" -t tray.scroll --id "$id" --delta 1 \
	--orientation vertical >/dev/null 2>&1
for _ in $(seq 1 100); do
	grep -q "^scroll " "$workdir/item.log" && break
	sleep 0.1
done

check "a click reaches the application, with where the icon was" "activate 40 30" \
	"$(grep -o '^activate .*' "$workdir/item.log" | tail -1)"
check "and the wheel is a step in an axis" "scroll 1 vertical" \
	"$(grep -o '^scroll .*' "$workdir/item.log" | tail -1)"

# ---------------------------------------------------------------------------
# The menu. An application publishes it as a com.canonical.dbusmenu object and
# the compositor reads it — the layout, the properties on each row, and the
# events going back the other way. This is the half no unit test can reach.
# ---------------------------------------------------------------------------
check "the item says it has a menu this compositor can draw" yes \
	"$(case "$update" in *'"has_menu":true'*) echo yes ;; *) echo no ;; esac)"

"$viewport" msg --socket "$socket" --timeout 10 -t subscribe tray.menu \
	>"$workdir/menu.json" 2>&1 &
subscribe_pid=$!
sleep 0.5
"$viewport" msg --socket "$socket" -t tray.activate --id "$id" \
	--button menu --x 40 --y 30 >/dev/null 2>&1
for _ in $(seq 1 100); do
	grep -q '"tray.menu"' "$workdir/menu.json" && break
	sleep 0.1
done
kill "$subscribe_pid" 2>/dev/null
wait "$subscribe_pid" 2>/dev/null
menu=$(grep '"tray.menu"' "$workdir/menu.json" | tail -1)

check "asking for the menu fetches a layout" yes \
	"$([ -n "$menu" ] && echo yes || echo no)"
check "the application was asked first, as the specification wants" yes \
	"$(grep -q "^menu about to show" "$workdir/item.log" && echo yes || echo no)"
check "a label arrives without the mnemonic the toolkit would have drawn" yes \
	"$(case "$menu" in *'"Open"'*) echo yes ;; *) echo no ;; esac)"
check "a separator comes through as one" yes \
	"$(case "$menu" in *'"separator"'*) echo yes ;; *) echo no ;; esac)"
check "a disabled row is marked rather than dropped" yes \
	"$(case "$menu" in *'"enabled":false'*) echo yes ;; *) echo no ;; esac)"
# Two substrings rather than one pattern: the fields are written in whatever
# order the serialiser puts them, and a test that depends on that order is a
# test of the serialiser.
check "a ticked row carries both its kind and its state" yes \
	"$(case "$menu" in *'"toggle":"checkmark"'*) case "$menu" in
		*'"checked":true'*) echo yes ;; *) echo no ;; esac ;;
	*) echo no ;; esac)"
check "a submenu comes with it, rather than in a second round trip" yes \
	"$(case "$menu" in *'"notes.md"'*) echo yes ;; *) echo no ;; esac)"
# A row the application asked not to show is dropped here rather than sent for
# the shell to hide: the shell draws what it is given.
check "a row marked invisible is not sent at all" no \
	"$(case "$menu" in *'"Hidden"'*) echo yes ;; *) echo no ;; esac)"

"$viewport" msg --socket "$socket" -t tray.menu.click --id "$id" --item 6 \
	>/dev/null 2>&1
"$viewport" msg --socket "$socket" -t tray.menu.closed --id "$id" >/dev/null 2>&1
for _ in $(seq 1 100); do
	grep -q "^menu event 0 closed" "$workdir/item.log" && break
	sleep 0.1
done
check "choosing a row reaches the application as a click on that row" yes \
	"$(grep -q "^menu event 6 clicked" "$workdir/item.log" && echo yes || echo no)"
check "and a menu dismissed without a choice is reported too" yes \
	"$(grep -q "^menu event 0 closed" "$workdir/item.log" && echo yes || echo no)"

# An application exiting is not announced by anything but its bus name going
# away, which is the only notice a crash gives either.
kill "$item_pid" 2>/dev/null
wait "$item_pid" 2>/dev/null
item_pid=
for _ in $(seq 1 100); do
	grep -q "tray: 0 item" "$log" && break
	sleep 0.1
done
check "an item whose application exits leaves the tray" yes \
	"$(grep -q "tray: 0 item" "$log" && echo yes || echo no)"

# ---------------------------------------------------------------------------
# The other half of the desktop: an item that publishes no menu object and
# draws its own window when it is asked for one. Nothing is fetched for it and
# the request goes out as ContextMenu, which is what it is waiting for.
# ---------------------------------------------------------------------------
"$item" --no-menu Steam >"$workdir/plain.log" 2>&1 &
item_pid=$!
for _ in $(seq 1 100); do
	grep -q "^registered " "$workdir/plain.log" && break
	kill -0 "$item_pid" 2>/dev/null || break
	sleep 0.1
done
plain=$(grep -o "^registered .*" "$workdir/plain.log" | cut -d' ' -f2)/StatusNotifierItem
"$viewport" msg --socket "$socket" -t tray.activate --id "$plain" \
	--button menu --x 40 --y 30 >/dev/null 2>&1
for _ in $(seq 1 100); do
	grep -q "^menu " "$workdir/plain.log" && break
	sleep 0.1
done
check "an item with no menu object is asked to draw its own" "menu 40 30" \
	"$(grep -o '^menu .*' "$workdir/plain.log" | tail -1)"
kill "$item_pid" 2>/dev/null
wait "$item_pid" 2>/dev/null
item_pid=

# ---------------------------------------------------------------------------
# And with it turned off. A session that would rather run somebody else's tray
# asks for "tray": false, and what that has to mean is that the names are free
# — an application registering then gets an error rather than an icon nothing
# draws.
# ---------------------------------------------------------------------------
kill "$viewport_pid" 2>/dev/null
wait "$viewport_pid" 2>/dev/null
viewport_pid=

echo '{ "tray": false }' >"$workdir/off.json"
off="$workdir/off.log"
"$viewport" --headless --config "$workdir/off.json" >"$off" 2>&1 &
viewport_pid=$!
for _ in $(seq 1 100); do
	grep -q "control socket at" "$off" && break
	kill -0 "$viewport_pid" 2>/dev/null || break
	sleep 0.1
done

check "the watcher name is left alone" no \
	"$(grep -q "claimed org.kde.StatusNotifierWatcher" "$off" && echo yes || echo no)"

"$item" Nextcloud >"$workdir/off-item.log" 2>&1
check "so an item has nobody to register with" no \
	"$(grep -q "^registered " "$workdir/off-item.log" && echo yes || echo no)"

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed; the compositor's log:" >&2
	tail -30 "$log" >&2
	exit 1
fi
echo "all tray checks passed"
