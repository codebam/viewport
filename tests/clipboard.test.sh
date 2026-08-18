#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# The clipboard history, end to end.
#
# What is tested is the thing a clipboard manager exists for: copy something,
# close the program that copied it, and paste it anyway. A Wayland selection
# lives only as long as the client offering it, so that last step is the whole
# feature — and it cannot be checked in Rust, because it needs a real client
# offering a real selection and another one asking for it.
#
# wl-copy and wl-paste are those clients. The compositor is headless, so this
# needs no display.
#
#   tests/clipboard.test.sh target/debug/viewport
set -u

viewport=${1:-target/debug/viewport}
if [ ! -x "$viewport" ]; then
	echo "missing $viewport — build first" >&2
	exit 2
fi
viewport=$(realpath "$viewport")

for tool in wl-copy wl-paste; do
	if ! command -v "$tool" >/dev/null; then
		echo "SKIP: no $tool to offer a selection with"
		exit 77
	fi
done

workdir=$(mktemp -d)
viewport_pid=

# By PID, and a named one: pattern matching on "viewport" has killed a live
# session more than once and this runs on the same machine as one.
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

# The history, as the shell would be handed it.
#
# Watched rather than replied to: the compositor answers `clipboard.query` by
# sending the history to everything on the socket, which is what a shell wants
# and what a subscriber sees.
history_json() {
	"$viewport" msg --socket "$socket" --timeout 0 -t subscribe clipboard.history \
		>"$workdir/history.json" 2>&1 &
	local watcher=$!
	sleep 0.3
	"$viewport" msg --socket "$socket" -t clipboard.query >/dev/null 2>&1
	sleep 0.4
	kill "$watcher" 2>/dev/null
	wait "$watcher" 2>/dev/null
	grep '"clipboard.history"' "$workdir/history.json" | tail -1
}

# The tray is off in both runs: this test is about selections, and a watcher
# name claimed on a private bus is one more thing that can go wrong in it.
echo '{ "tray": false, "clipboard_history": 5 }' >"$workdir/on.json"
start_viewport "$workdir/on.json" "$workdir/on.log"
socket=$(grep -o "control socket at .*" "$workdir/on.log" | tail -1 | cut -d' ' -f4-)
display=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$workdir/on.log" | head -1 | cut -d= -f2)
if [ -z "$socket" ] || [ -z "$display" ]; then
	echo "the compositor did not name its socket or its display" >&2
	exit 2
fi
export WAYLAND_DISPLAY="$display"

# wl-copy stays alive holding the selection, which is exactly what a real
# application does — and what makes the last check below mean something.
printf 'the first thing' | wl-copy
printf 'the second thing' | wl-copy
for _ in $(seq 1 50); do
	case "$(history_json)" in *'the second thing'*) break ;; esac
	sleep 0.1
done
entries=$(history_json)

check "what was copied reaches the history" yes \
	"$(case "$entries" in *'the first thing'*) echo yes ;; *) echo no ;; esac)"
check "and so does the next thing" yes \
	"$(case "$entries" in *'the second thing'*) echo yes ;; *) echo no ;; esac)"
# Newest first, which is the order a picker draws them in: the second copy has
# to appear before the first in the list.
check "newest first" yes \
	"$(case "$entries" in *'the second thing'*'the first thing'*) echo yes ;; *) echo no ;; esac)"

# The point of the whole thing: the clients that offered these selections are
# gone, and the older entry can still be pasted.
pkill -x wl-copy 2>/dev/null
sleep 0.3
id=$(printf '%s' "$entries" | tr '{' '\n' | grep 'the first thing' |
	grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
check "the older entry is still named by an id" yes \
	"$([ -n "$id" ] && echo yes || echo no)"
"$viewport" msg --socket "$socket" -t clipboard.paste --id "${id:-0}" >/dev/null 2>&1
pasted=$(timeout 5 wl-paste --no-newline 2>/dev/null)
check "and pasting it works with the application that copied it gone" \
	"the first thing" "$pasted"

# And it becomes the newest thing, as it does in every clipboard manager: the
# next paste finds what was just chosen.
entries=$(history_json)
check "the entry that was pasted is now the newest" yes \
	"$(case "$entries" in *'the first thing'*'the second thing'*) echo yes ;; *) echo no ;; esac)"

# Forgetting everything, which is what somebody asks for after copying a
# password.
"$viewport" msg --socket "$socket" -t clipboard.forget >/dev/null 2>&1
entries=$(history_json)
check "forgetting empties the history" no \
	"$(case "$entries" in *'the second thing'*) echo yes ;; *) echo no ;; esac)"

kill "$viewport_pid" 2>/dev/null
wait "$viewport_pid" 2>/dev/null
viewport_pid=

# ---------------------------------------------------------------------------
# And turned off, which is a session that would rather run cliphist — or one
# that does not want a copy of every password that passes through the
# clipboard.
# ---------------------------------------------------------------------------
echo '{ "tray": false, "clipboard_history": 0 }' >"$workdir/off.json"
start_viewport "$workdir/off.json" "$workdir/off.log"
socket=$(grep -o "control socket at .*" "$workdir/off.log" | tail -1 | cut -d' ' -f4-)
display=$(grep -o 'WAYLAND_DISPLAY=[A-Za-z0-9_-]*' "$workdir/off.log" | head -1 | cut -d= -f2)
export WAYLAND_DISPLAY="$display"

printf 'not remembered' | wl-copy
sleep 0.5
check "a history of zero keeps nothing" no \
	"$(case "$(history_json)" in *'not remembered'*) echo yes ;; *) echo no ;; esac)"
# And the clipboard itself still works, because none of this touches it.
check "but the clipboard itself is untouched" "not remembered" \
	"$(timeout 5 wl-paste --no-newline 2>/dev/null)"
pkill -x wl-copy 2>/dev/null

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed; the compositor's log:" >&2
	tail -30 "$workdir/on.log" >&2
	exit 1
fi
echo "all clipboard checks passed"
