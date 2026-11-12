#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# The bus-shaped messages, end to end on a private bus.
#
# The tray, MPRIS and the notification centre already have their own files;
# what is left is `notification.dismiss` (its difference from an expire and
# from an action is the point: an expired popup stays in the history, an
# acted-on one leaves it, and a dismissed one stays too — dismissing is
# closing, not forgetting), the two radios (network and bluetooth, whose
# updates are snapshots like the tray's), the power messages, the on-screen
# keyboard's keys, and `ai.login`.
#
# On a private session bus, because the compositor claims the Notifications
# name at startup and a test that joined the live bus would fight whatever is
# already holding it.
#
# Safety in this file, which is why so much of it tests refusals rather than
# successes:
#   * `power.action` suspend/reboot/poweroff go to logind on the system bus. A
#     private session bus does not isolate that, so no valid action is ever
#     sent; only an invalid one (refused by `apply` before the worker runs).
#   * `network.disconnect`, `network.radio` off and `bluetooth.power` off
#     would leave, switch off or power down the machine's own radios. Only
#     the stop-scanning half of each radio is sent (a picker closing sends it),
#     plus error paths. Valid connect/disconnect on a real device are likewise
#     left unsent; the bogus-device check pins that an unknown action does
#     nothing rather than reaching BlueZ.
#   * `power.profile` with a standard name would switch the host's profile, so
#     only a name the daemon never advertised is sent (the worker drops it).
#   * `ai.login` with `openai` would start an OAuth flow with network and a
#     browser. Only an unknown provider is sent (refused as an `error`).
# `network.update`/`bluetooth.update` carry whatever NetworkManager/BlueZ
# answer on the system bus; the assertions are on the wire fields, never on
# which networks exist.
#
#   tests/services-ipc.test.sh target/debug/viewport
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
# gdbus sends both Notify and CloseNotification (see the withdrawal check).

if [ "${VIEWPORT_SERVICES_TEST_BUS:-}" != yes ]; then
	export VIEWPORT_SERVICES_TEST_BUS=yes
	exec dbus-run-session -- "$0" "$viewport"
fi

workdir=$(mktemp -d)
viewport_pid=

# By PID, and only the one this test started.
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

socket_of() {
	grep -o "control socket at .*" "$1" | tail -1 | cut -d' ' -f4-
}

echo '{ "tray": false }' >"$workdir/on.json"
start_viewport "$workdir/on.json" "$workdir/on.log"
socket=$(socket_of "$workdir/on.log")
if [ -z "$socket" ]; then
	echo "the compositor did not name its control socket" >&2
	exit 2
fi

# The compositor must hold the notification name for Notify to land here.
for _ in $(seq 1 100); do
	grep -q "claimed org.freedesktop.Notifications" "$workdir/on.log" && break
	sleep 0.1
done

id_of() { echo "$1" | sed -n 's/.*uint32 \([0-9]\{1,\}\).*/\1/p'; }

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

# One subscriber for the add/close and radio/power snapshots.
"$viewport" msg --socket "$socket" --timeout 0 -t subscribe \
	notification.add notification.close \
	network.update bluetooth.update power.update \
	>"$workdir/events.json" 2>&1 &
EVENTS_PID=$!
sleep 0.5

notify() {
	gdbus call --session --dest org.freedesktop.Notifications \
		--object-path /org/freedesktop/Notifications \
		--method org.freedesktop.Notifications.Notify \
		"test-app" 0 "" "$1" "$2" "[]" "{}" 5000 2>&1
}

first=$(notify "to be dismissed" "dismiss keeps what expire keeps")
first_id=$(id_of "$first")
check "the bus answered with an id" yes \
	"$([ -n "$first_id" ] && echo yes || echo no)"

second=$(notify "to be expired" "expire keeps the history too")
second_id=$(id_of "$second")

# The add is what the shell draws; the close is what the sender is told when
# the popup goes. Both arrive on the same subscriber.
for _ in $(seq 1 50); do
	grep -q '"type":"notification.add"' "$workdir/events.json" 2>/dev/null && break
	sleep 0.1
done
check "a Notify arrived as a notification.add" yes \
	"$(grep -q '"type":"notification.add"' "$workdir/events.json" && echo yes || echo no)"
ADD=$(grep '"type":"notification.add"' "$workdir/events.json" | head -1)
check "and it carries the summary the application sent" yes \
	"$(printf '%s' "$ADD" | grep -q 'to be dismissed' && echo yes || echo no)"
check "and the body" yes \
	"$(printf '%s' "$ADD" | grep -q 'dismiss keeps' && echo yes || echo no)"

# Dismiss: the popup goes and the sender is told over D-Bus
# (NotificationClosed), and the history keeps it. The socket event
# `notification.close` is only for a sender withdrawal (below), so no
# close line is expected for this id.
if "$viewport" msg --socket "$socket" -t notification.dismiss --id "$first_id" >/dev/null 2>&1; then
	echo "ok: notification.dismiss was accepted"
else
	echo "FAIL: notification.dismiss was refused" >&2
	failures=$((failures + 1))
fi
sleep 0.5
check "and no socket close event names it (dismiss is not a withdrawal)" no \
	"$(grep '"type":"notification.close"' "$workdir/events.json" | grep -q "\"id\":$first_id" && echo yes || echo no)"
HISTORY=$(history_now)
check "and a dismissed notification stays in the history" yes \
	"$(printf '%s' "$HISTORY" | grep -q 'to be dismissed' && echo yes || echo no)"

# Expire: the same, and it stays too (the centre exists for popups that
# went on their own).
if "$viewport" msg --socket "$socket" -t notification.expire --id "$second_id" >/dev/null 2>&1; then
	echo "ok: notification.expire was accepted"
else
	echo "FAIL: notification.expire was refused" >&2
	failures=$((failures + 1))
fi
sleep 0.5
check "and no socket close event names it either" no \
	"$(grep '"type":"notification.close"' "$workdir/events.json" | grep -q "\"id\":$second_id" && echo yes || echo no)"
HISTORY=$(history_now)
check "and an expired one stays as well" yes \
	"$(printf '%s' "$HISTORY" | grep -q 'to be expired' && echo yes || echo no)"

# Withdrawal: the sender closes its own notification. Unlike a dismiss or
# an expire, this *does* arrive as `notification.close` and it *does*
# leave the history.
if [ -n "$second_id" ]; then
	if gdbus call --session --dest org.freedesktop.Notifications \
		--object-path /org/freedesktop/Notifications \
		--method org.freedesktop.Notifications.CloseNotification \
		"uint32 $second_id" >/dev/null 2>&1; then
		echo "ok: CloseNotification was sent (setup for notification.close)"
	else
		echo "FAIL: CloseNotification could not be sent" >&2
		failures=$((failures + 1))
	fi
	for _ in $(seq 1 50); do
		grep '"type":"notification.close"' "$workdir/events.json" 2>/dev/null |
			grep -q "\"id\":$second_id" && break
		sleep 0.1
	done
	check "and it arrived as a notification.close for that id" yes \
		"$(grep '"type":"notification.close"' "$workdir/events.json" | grep -q "\"id\":$second_id" && echo yes || echo no)"
	HISTORY=$(history_now)
	check "and a withdrawn notification leaves the history (unlike an expired one)" yes \
		"$(printf '%s' "$HISTORY" | grep -q 'to be expired' && echo no || echo yes)"
fi

# A dismiss naming no notification is silently ignored (a stale id is a race,
# not a mistake), like every other view/notification id here.
if "$viewport" msg --socket "$socket" -t notification.dismiss --id 999999 >/dev/null 2>&1; then
	echo "ok: notification.dismiss for a nonexistent id was silently ignored"
else
	echo "FAIL: notification.dismiss for a nonexistent id was refused" >&2
	failures=$((failures + 1))
fi

# --- the radios -------------------------------------------------------------
# A picker opening sends scan-true (absent means yes); closing sends
# false, which stops both reading and transmitting.
if "$viewport" msg --socket "$socket" -t network.scan >/dev/null 2>&1; then
	echo "ok: network.scan (to open) was accepted"
else
	echo "FAIL: network.scan was refused" >&2
	failures=$((failures + 1))
fi
for _ in $(seq 1 100); do
	grep -q '"type":"network.update"' "$workdir/events.json" 2>/dev/null && break
	sleep 0.1
done
check "and a network.update arrived in answer" yes \
	"$(grep -q '"type":"network.update"' "$workdir/events.json" && echo yes || echo no)"
NET=$(grep '"type":"network.update"' "$workdir/events.json" | tail -1)
check "and it carries whether NetworkManager answered" yes \
	"$(printf '%s' "$NET" | grep -q '"available":' && echo yes || echo no)"
check "and the radio and the machine state" yes \
	"$(printf '%s' "$NET" | grep -q '"wireless":' && printf '%s' "$NET" | grep -q '"state":' && echo yes || echo no)"

if "$viewport" msg --socket "$socket" -t bluetooth.scan >/dev/null 2>&1; then
	echo "ok: bluetooth.scan (to open) was accepted"
else
	echo "FAIL: bluetooth.scan was refused" >&2
	failures=$((failures + 1))
fi
for _ in $(seq 1 100); do
	grep -q '"type":"bluetooth.update"' "$workdir/events.json" 2>/dev/null && break
	sleep 0.1
done
check "and a bluetooth.update arrived in answer" yes \
	"$(grep -q '"type":"bluetooth.update"' "$workdir/events.json" && echo yes || echo no)"
BT=$(grep '"type":"bluetooth.update"' "$workdir/events.json" | tail -1)
check "and it carries the adapter state" yes \
	"$(printf '%s' "$BT" | grep -q '"powered":' && printf '%s' "$BT" | grep -q '"available":' && echo yes || echo no)"

# Stop both (what a picker closing sends). Leave the machine as it was found.
"$viewport" msg --socket "$socket" -t network.scan --enabled false >/dev/null 2>&1
"$viewport" msg --socket "$socket" -t bluetooth.scan --enabled false >/dev/null 2>&1

# Error paths for the radios: missing/invalid refused at the prompt; an
# unknown bluetooth action does nothing (the worker drops it).
err() {
	"$viewport" msg --socket "$socket" "$@" >/dev/null 2>"$workdir/err.txt"
	local code=$?
	echo "$code $(cat "$workdir/err.txt" | tr '\n' ' ')"
}
check "network.connect without an ssid is a usage error" yes \
	"$(err -t network.connect | grep -q "^2 .*missing field" && echo yes || echo no)"
check "bluetooth.device without an action is a usage error" yes \
	"$(err -t bluetooth.device --address AA:BB:CC:DD:EE:FF | grep -q "^2 .*missing field" && echo yes || echo no)"

# A connect to a network that does not exist is handed to NetworkManager
# (nothing is joined); what is pinned is acceptance, not success.
if "$viewport" msg --socket "$socket" -t network.connect --ssid probe-ssid-no-such-net >/dev/null 2>&1; then
	echo "ok: network.connect to an unknown ssid was accepted (nothing joined)"
else
	echo "FAIL: network.connect was refused" >&2
	failures=$((failures + 1))
fi
# An unknown bluetooth action does nothing: send it, wait, and the device
# list must not have errored or changed identity (no device names an error).
if "$viewport" msg --socket "$socket" -t bluetooth.device --address AA:BB:CC:DD:EE:FF --action bogus >/dev/null 2>&1; then
	echo "ok: bluetooth.device with an unknown action was accepted (and ignored)"
else
	echo "FAIL: bluetooth.device with an unknown action was refused" >&2
	failures=$((failures + 1))
fi
sleep 0.5
check "and the compositor is still answering" yes \
	"$("$viewport" msg --socket "$socket" -t output.query >/dev/null 2>&1 && echo yes || echo no)"

# --- power, osk, ai -----------------------------------------------------------
# Only names that cannot mutate the host are sent (see the header). Each is
# accepted (the worker drops what it has no daemon for); the refusals are
# what distinguish invalid verbs from valid ones.
if "$viewport" msg --socket "$socket" -t power.profile --profile no-such-profile >/dev/null 2>&1; then
	echo "ok: power.profile with an unadvertised name was accepted (dropped)"
else
	echo "FAIL: power.profile was refused" >&2
	failures=$((failures + 1))
fi
check "power.profile without a profile is a usage error" yes \
	"$(err -t power.profile | grep -q "^2 .*missing field" && echo yes || echo no)"
check "an invalid power.action is refused before any logind call" yes \
	"$(err -t power.action --action bogus | grep -q '^1 .*power\.action: no such action' && echo yes || echo no)"
check "power.action without an action is a usage error" yes \
	"$(err -t power.action | grep -q "^2 .*missing field" && echo yes || echo no)"

# The on-screen keyboard: a tap is pressed true then false, with nothing
# focused that shows a keyboard here, so the observable is acceptance.
if "$viewport" msg --socket "$socket" -t osk.key --keysym 65288 --pressed true >/dev/null 2>&1 &&
	"$viewport" msg --socket "$socket" -t osk.key --keysym 65288 --pressed false >/dev/null 2>&1; then
	echo "ok: osk.key down/up was accepted"
else
	echo "FAIL: osk.key was refused" >&2
	failures=$((failures + 1))
fi
check "osk.key without a keysym is a usage error" yes \
	"$(err -t osk.key --pressed true | grep -q "^2 .*missing field" && echo yes || echo no)"

# ai.login: unknown provider is refused as an error naming the request.
# (The openai flow would reach the network, so it is not started here.)
check "ai.login with an unknown provider is refused" yes \
	"$(err --raw '{"type":"ai.login","provider":"bogus"}' | grep -q '^1 .*ai\.login: no OAuth login' && echo yes || echo no)"
check "ai.login is offerable with -t" "0" \
	"$(err -t ai.login --provider bogus | grep -q "ai\.login: no OAuth login" && echo 0 || echo nonzero)"

kill "$EVENTS_PID" 2>/dev/null
wait "$EVENTS_PID" 2>/dev/null
EVENTS_PID=

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed; the compositor's log:" >&2
	tail -30 "$workdir/on.log" >&2
	exit 1
fi
echo "all services-ipc checks passed"
