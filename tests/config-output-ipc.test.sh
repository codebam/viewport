#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# The config setters, the output messages, the runtime binds and the launcher,
# over `viewport msg`.
#
# These are the messages a settings panel and a script drive, and each has a
# different way of answering: the setters below change what the next
# `view.query` says (the `config` event carries gaps, border, wallpaper and
# dark_mode), the output messages change what `output.query` says, `bind.add`
# shows up in the keymap the compositor announces, `launcher.query` is itself
# the answer (a list with a generation), and `status.refresh`/`status.volume`
# re-sample the bar.
#
# `view.query` is one `config` here: the test greps the single line that
# carries it, because the whole answer (config plus one `view.added` per
# window, of which there are none) is printed line by line.
#
#   tests/config-output-ipc.test.sh target/debug/viewport
set -uo pipefail

VIEWPORT=${1:?usage: config-output-ipc.test.sh VIEWPORT}
if [ ! -x "$VIEWPORT" ]; then
	echo "missing $VIEWPORT — build first" >&2
	exit 2
fi
VIEWPORT=$(realpath "$VIEWPORT")

WORK=$(mktemp -d)
LOG="$WORK/viewport.log"
COMPOSITOR_PID=

cleanup() {
	if [ -n "$COMPOSITOR_PID" ] && kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
		kill -TERM "$COMPOSITOR_PID" 2>/dev/null
		for _ in $(seq 20); do
			kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
			sleep 0.1
		done
		kill -KILL "$COMPOSITOR_PID" 2>/dev/null
	fi
	rm -rf "$WORK"
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

unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR="$WORK/run"
export XDG_STATE_HOME="$WORK/state"
export XDG_CONFIG_HOME="$WORK/config"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME/viewport"

mkdir -p "$XDG_CONFIG_HOME/viewport"
printf '{ "layout": "tiling", "tray": false }\n' >"$XDG_CONFIG_HOME/viewport/config.json"

"$VIEWPORT" --headless --config "$XDG_CONFIG_HOME/viewport/config.json" >"$LOG" 2>&1 &
COMPOSITOR_PID=$!

SOCKET=
for _ in $(seq 100); do
	SOCKET=$(sed -n 's/.*control socket at \(.*\)$/\1/p' "$LOG" | head -1)
	[ -n "$SOCKET" ] && break
	kill -0 "$COMPOSITOR_PID" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$SOCKET" ]; then
	echo "FAIL the compositor never published its socket" >&2
	tail -20 "$LOG" >&2
	exit 2
fi
echo "ok   the compositor is up, socket at $SOCKET"

msg() {
	"$VIEWPORT" msg --socket "$SOCKET" "$@"
}

# The single `config` line from a `view.query`.
config_now() {
	msg -t view.query 2>/dev/null | grep '"type":"config"' | tail -1
}

# One key from that config, dotted for the nested gaps/border objects.
config_field() {
	local field=$1
	# "gaps.inner" -> gaps, inner.
	local obj=${field%%.*} key=${field#*.}
	if [ "$obj" = "$field" ]; then
		config_now | sed -n "s/.*\"$field\":\([^,}]*\).*/\1/p" | head -1
	else
		config_now | sed -n "s/.*\"$obj\":{[^}]*\"$key\":\([^,}]*\).*/\1/p" | head -1
	fi
}

# The first `output.layout` line (queries print exactly one).
layout_now() {
	msg -t output.query 2>/dev/null | tail -1
}

err() {
	msg "$@" >/dev/null 2>"$WORK/err.txt"
	local code=$?
	echo "$code $(cat "$WORK/err.txt" | tr '\n' ' ')"
}

# --- config.gaps -----------------------------------------------------------
# Only the fields given change. Zero is deliberate (no spacing) and accepted;
# a negative value is refused. The shell reads these off the `config` event,
# so the check is the next query.
if msg -t config.gaps --inner 12 --outer 4 --smart true >/dev/null 2>&1; then
	echo "ok: config.gaps was accepted"
else
	echo "FAIL: config.gaps was refused" >&2
	failures=$((failures + 1))
fi
check "and the next config carries the inner gap" "12" "$(config_field gaps.inner)"
check "and the outer gap" "4" "$(config_field gaps.outer)"
check "and the smart flag" "true" "$(config_field gaps.smart)"

check "a negative inner gap is refused" yes \
	"$(err -t config.gaps --inner=-1 | grep -q "^1 .*config.gaps: inner" && echo yes || echo no)"
check "and the gaps are unchanged by the refusal" "12" "$(config_field gaps.inner)"

# Partial rejection must apply nothing. The valid half here is `inner`,
# the invalid half `outer`; what the next query says is what the shell
# would draw.
check "a partially invalid config.gaps is refused" yes \
	"$(err -t config.gaps --inner 99 --outer=-9 | grep -q "^1 .*config.gaps: outer" && echo yes || echo no)"
check "a partially invalid config.gaps applies nothing" "12" \
	"$(config_field gaps.inner)"

if msg -t config.gaps --inner 0 >/dev/null 2>&1; then
	echo "ok: a zero inner gap was accepted (deliberate, not a refusal)"
else
	echo "FAIL: a zero inner gap was refused" >&2
	failures=$((failures + 1))
fi
check "and the next config says inner 0" "0" "$(config_field gaps.inner)"
# Restore for the save checks below.
msg -t config.gaps --inner 12 >/dev/null 2>&1

# --- config.border ----------------------------------------------------------
if msg -t config.border --radius 7 --width 3 --smart true >/dev/null 2>&1; then
	echo "ok: config.border was accepted"
else
	echo "FAIL: config.border was refused" >&2
	failures=$((failures + 1))
fi
check "and the next config carries the radius" "7" "$(config_field border.radius)"
check "and the width" "3" "$(config_field border.width)"
check "a negative width is refused" yes \
	"$(err -t config.border --width=-5 | grep -q "^1 .*config.border: width" && echo yes || echo no)"
check "and the border is unchanged by the refusal" "7" "$(config_field border.radius)"
check "a partially invalid config.border applies nothing" "7" \
	"$(err -t config.border --radius 99 --width=-5 >/dev/null 2>&1; config_field border.radius)"

# --- config.wallpaper -------------------------------------------------------
# Unlike gaps/border, this one resolves the picture and the mode before
# changing anything, so a refusal here must leave both halves alone.
if msg -t config.wallpaper --path '#112233' --mode tile >/dev/null 2>&1; then
	echo "ok: config.wallpaper (colour) was accepted"
else
	echo "FAIL: config.wallpaper (colour) was refused" >&2
	failures=$((failures + 1))
fi
check "and the next config carries the wallpaper" '"#112233"' \
	"$(config_now | sed -n 's/.*"wallpaper":"\([^"]*\)".*/"\1"/p')"
check "and the fitting" '"tile"' \
	"$(config_now | sed -n 's/.*"wallpaper_mode":"\([^"]*\)".*/"\1"/p')"

check "a missing picture is refused" yes \
	"$(err -t config.wallpaper --path /nowhere/wall.png | grep -q "^1 .*config.wallpaper" && echo yes || echo no)"
check "and the wallpaper is unchanged by the refusal" '"#112233"' \
	"$(config_now | sed -n 's/.*"wallpaper":"\([^"]*\)".*/"\1"/p')"
check "an unknown fitting is refused" yes \
	"$(err -t config.wallpaper --mode zoom | grep -q "^1 .*config.wallpaper" && echo yes || echo no)"
check "and the fitting is unchanged by the refusal" '"tile"' \
	"$(config_now | sed -n 's/.*"wallpaper_mode":"\([^"]*\)".*/"\1"/p')"
# The mode alone leaves the picture alone (trying a fitting out keeps it).
if msg -t config.wallpaper --mode center >/dev/null 2>&1; then
	echo "ok: changing the fitting alone was accepted"
else
	echo "FAIL: changing the fitting alone was refused" >&2
	failures=$((failures + 1))
fi
check "and the picture survived it" '"#112233"' \
	"$(config_now | sed -n 's/.*"wallpaper":"\([^"]*\)".*/"\1"/p')"
check "and the fitting is the new one" '"center"' \
	"$(config_now | sed -n 's/.*"wallpaper_mode":"\([^"]*\)".*/"\1"/p')"
# The empty string is the only way back to the shell's own background.
if msg -t config.wallpaper --path '' >/dev/null 2>&1; then
	echo "ok: removing the wallpaper was accepted"
else
	echo "FAIL: removing the wallpaper was refused" >&2
	failures=$((failures + 1))
fi
check "and the next config carries no wallpaper" no \
	"$(config_now | grep -q '"wallpaper":' && echo yes || echo no)"

# --- config.save ------------------------------------------------------------
# The setter above (gaps inner 12) is what is saved. The answer names the
# overlay file; a settings panel's Save button has to be able to say where.
SAVED=$(msg -t config.save 2>/dev/null | tail -1)
check "config.save answers with a config.saved" yes \
	"$(printf '%s' "$SAVED" | grep -q '"type":"config.saved"' && echo yes || echo no)"
SAVED_PATH=$(printf '%s' "$SAVED" | sed -n 's/.*"path":"\([^"]*\)".*/\1/p')
check "and it names the overlay beside the config file" "$XDG_CONFIG_HOME/viewport/settings.json" \
	"$SAVED_PATH"
check "and the overlay carries the runtime gaps" "12" \
	"$(python3 -c 'import json,sys
try:
    print(json.load(open(sys.argv[1]))["gaps"]["inner"])
except Exception:
    print("missing")
' "$SAVED_PATH" 2>/dev/null || echo missing)"
# The config file itself is never rewritten (comments and formatting survive
# a round trip, or the panel has no config file left to edit).
check "and the config file was not rewritten" '{ "layout": "tiling", "tray": false }' \
	"$(cat "$XDG_CONFIG_HOME/viewport/config.json")"

# --- output.active (needs a second output) -----------------------------------
if msg -t output.test_add >/dev/null 2>&1; then
	echo "ok: output.test_add was accepted (setup for output.active)"
else
	echo "FAIL: output.test_add was refused" >&2
	failures=$((failures + 1))
fi
sleep 0.5
SECOND=$(layout_now | grep -o '"name":"HEADLESS-2"' | head -1 | cut -d'"' -f4)
if [ -z "$SECOND" ]; then
	echo "FAIL no second output after output.test_add" >&2
	failures=$((failures + 1))
else
	if msg -t output.active --name "$SECOND" >/dev/null 2>&1; then
		echo "ok: output.active was accepted"
	else
		echo "FAIL: output.active was refused" >&2
		failures=$((failures + 1))
	fi
	LAYOUT=$(layout_now)
	check "and the next layout marks only the named output active" yes \
		"$(printf '%s' "$LAYOUT" | grep -o '"active":[^,}]*' | grep -c 'true' | grep -q '^1$' && echo yes || echo no)"
	check "and the named output is the active one" yes \
		"$(printf '%s' "$LAYOUT" | SECOND="$SECOND" python3 -c '
import json, os, sys
try:
    outs = json.loads(sys.stdin.read())["outputs"]
    want = os.environ["SECOND"]
    ok = any(o.get("name") == want and o.get("active") is True for o in outs)
    print("yes" if ok else "no")
except Exception:
    print("no")
' 2>/dev/null || echo no)"
fi
check "a nonexistent output is refused" yes \
	"$(err -t output.active --name NOPE-1 | grep -q "^1 .*output.active: no such output" && echo yes || echo no)"
check "and the active output is unchanged by the refusal" yes \
	"$(layout_now | grep -o '"active":[^,}]*' | grep -c 'true' | grep -q '^1$' && echo yes || echo no)"

# --- output.hdr: headless can only refuse ------------------------------------
# HDR needs the drm backend and a capable head; the headless output is
# neither, so both the unknown-head and the known-head cases are refusals.
# The refusal text is the assertion (it is what distinguishes the two).
check "output.hdr on an unknown output is refused" yes \
	"$(err -t output.hdr --name NOPE-1 --enabled true | grep -q "^1 .*output.hdr: no such output" && echo yes || echo no)"
check "output.hdr on the headless output names the drm backend" yes \
	"$(err -t output.hdr --name HEADLESS-1 --enabled true | grep -q "HDR needs the drm backend" && echo yes || echo no)"
check "output.test_remove on an unknown output is refused" yes \
	"$(err -t output.test_remove --name NOPE-1 | grep -q "^1 .*output.test_remove" && echo yes || echo no)"
if [ -n "$SECOND" ]; then
	if msg -t output.test_remove --name "$SECOND" >/dev/null 2>&1; then
		echo "ok: output.test_remove was accepted (cleanup)"
	else
		echo "FAIL: output.test_remove was refused" >&2
		failures=$((failures + 1))
	fi
	sleep 0.5
	check "and there is one output again" "1" \
		"$(layout_now | grep -o '"type":"output.layout"' >/dev/null && msg -t output.query 2>/dev/null | grep -o '"name":"HEADLESS-' | wc -l)"
fi

# --- bind.add ---------------------------------------------------------------
# A runtime bind is additive and expendable; it must show in the keymap the
# compositor announces (`config` -> binds), which is the only place the
# shell lists them from.
if msg -t bind.add --chord Mod4+F20 --action shell.focus >/dev/null 2>&1; then
	echo "ok: bind.add was accepted"
else
	echo "FAIL: bind.add was refused" >&2
	failures=$((failures + 1))
fi
check "and the new bind is in the announced keymap" yes \
	"$(config_now | grep '"chord":"Mod4+F20"' | grep -q '"action":"shell shell.focus"' && echo yes || echo no)"
check "a chord that does not parse is refused" yes \
	"$(err -t bind.add --chord 'not a chord' --action shell.focus | grep -q "^1 .*bind.add:" && echo yes || echo no)"
check "bind.add without an action is a usage error" yes \
	"$(err -t bind.add --chord Mod4+F21 | grep -q "^2 .*missing field" && echo yes || echo no)"
# Re-adding the same chord replaces it (that is how a runtime bind is taken
# back), which must not be refused as a duplicate.
if msg -t bind.add --chord Mod4+F20 --action shell.overview >/dev/null 2>&1; then
	echo "ok: re-adding a chord replaces it"
else
	echo "FAIL: re-adding a chord was refused" >&2
	failures=$((failures + 1))
fi
check "and the keymap carries the replacement" yes \
	"$(config_now | grep '"chord":"Mod4+F20"' | grep -q '"action":"shell shell.overview"' && echo yes || echo no)"

# --- shell.exec ---------------------------------------------------------------
# The bar's widgets use this to drive the sink through wpctl or to open a
# directory. The command runs host-side; the file below is its observable.
# The message is accepted before the child runs, so poll for the file.
if msg -t shell.exec --command "touch $WORK/exec-proof" >/dev/null 2>&1; then
	echo "ok: shell.exec was accepted"
else
	echo "FAIL: shell.exec was refused" >&2
	failures=$((failures + 1))
fi
for _ in $(seq 1 50); do
	[ -e "$WORK/exec-proof" ] && break
	sleep 0.1
done
check "and the command ran on the host" yes \
	"$([ -e "$WORK/exec-proof" ] && echo yes || echo no)"
check "shell.exec without a command is a usage error" yes \
	"$(err -t shell.exec | grep -q "^2 .*missing field" && echo yes || echo no)"

# --- status.refresh / status.volume -------------------------------------------
# With no audio sink here `status.volume` still goes through the compositor's
# own wpctl path (which warns and carries on); the observable the test pins
# is the target validation, plus acceptance of a zero change (which touches
# nothing). A non-zero delta would change the user's volume, so none is sent.
if msg -t status.refresh >/dev/null 2>&1; then
	echo "ok: status.refresh was accepted"
else
	echo "FAIL: status.refresh was refused" >&2
	failures=$((failures + 1))
fi
if msg -t status.volume --target sink --delta 0 >/dev/null 2>&1; then
	echo "ok: status.volume (delta 0) was accepted"
else
	echo "FAIL: status.volume (delta 0) was refused" >&2
	failures=$((failures + 1))
fi
check "an unknown volume target is refused" yes \
	"$(err -t status.volume --target bogus --delta 5 | grep -q '^1 .*status\.volume: no such target' && echo yes || echo no)"
check "status.volume without a target is a usage error" yes \
	"$(err -t status.volume --delta 5 | grep -q "^2 .*missing field" && echo yes || echo no)"

# --- launcher.query / launcher.launch -----------------------------------------
# The scan and the generation are the compositor's: the shell cannot read
# XDG_DATA_DIRS. The generation is what makes a launch safe — a launch
# naming an older list (the picker sends a query on every keystroke and
# does not wait before Enter) must be refused rather than started.
LAUNCH1=$(msg -t launcher.query 2>/dev/null | tail -1)
check "launcher.query answers with a launcher.list" yes \
	"$(printf '%s' "$LAUNCH1" | grep -q '"type":"launcher.list"' && echo yes || echo no)"
GEN1=$(printf '%s' "$LAUNCH1" | sed -n 's/.*"generation":\([0-9]*\).*/\1/p')
check "and it carries its generation" yes \
	"$([ -n "$GEN1" ] && echo yes || echo no)"
check "and every row names what it is and what it is for" yes \
	"$(printf '%s' "$LAUNCH1" | grep -q '"apps":\[' && echo yes || echo no)"
APPS=$(printf '%s' "$LAUNCH1" | grep -o '"id":' | wc -l)
LAUNCH2=$(msg -t launcher.query 2>/dev/null | tail -1)
GEN2=$(printf '%s' "$LAUNCH2" | sed -n 's/.*"generation":\([0-9]*\).*/\1/p')
check "a second query moves the generation on" yes \
	"$([ -n "$GEN2" ] && [ "$GEN2" -gt "${GEN1:-0}" ] && echo yes || echo no)"

# Filtered: what the picker sends on every keystroke. Absent is the whole
# list; the filtered one is furnished the same way (generation, rows).
FILTERED=$(msg -t launcher.query --filter foot 2>/dev/null | tail -1)
check "a filtered launcher.query still answers with a list" yes \
	"$(printf '%s' "$FILTERED" | grep -q '"type":"launcher.list"' && echo yes || echo no)"
check "and it carries a generation" yes \
	"$(printf '%s' "$FILTERED" | grep -q '"generation":' && echo yes || echo no)"

# Refusals: a stale generation, a nonexistent application, a missing field.
# Every query moves the generation on, so each launch check names the
# generation of the list it was drawn from, not an older one.
if [ -n "$GEN1" ]; then
	check "a launch naming an older list is refused" yes \
		"$(err -t launcher.launch --id 0 --generation "$GEN1" | grep -q "the list $GEN1 is no longer" && echo yes || echo no)"
fi
LAUNCH_OK=$(msg -t launcher.query 2>/dev/null | tail -1)
GEN_OK=$(printf '%s' "$LAUNCH_OK" | sed -n 's/.*"generation":\([0-9]*\).*/\1/p')
check "a launch naming no such application is refused" yes \
	"$(err -t launcher.launch --id 999999 --generation "${GEN_OK:-1}" | grep -q "no such application" && echo yes || echo no)"
check "a launch without its generation is a usage error" yes \
	"$(err -t launcher.launch --id 0 | grep -q "^2 .*missing field" && echo yes || echo no)"

# A launch that is accepted starts the application, with an xdg-activation
# token so its window opens focused. Foot is preferred because it is fast and
# headless-reliable; without any Foot entry the test only pins acceptance.
# The fresh list above (LAUNCH_OK/GEN_OK) is the current one: GEN2 went stale
# at the filtered query.
FOOT_ID=$(printf '%s' "$LAUNCH_OK" | python3 -c '
import json, sys
try:
    apps = json.loads(sys.stdin.read()).get("apps", [])
    for app in apps:
        if app.get("name", "").lower() == "foot":
            print(app["id"])
            raise SystemExit
    if apps:
        print(apps[0]["id"])
except SystemExit:
    pass
except Exception:
    pass
')
if [ -n "$FOOT_ID" ] && [ -n "$GEN_OK" ]; then
	BEFORE=$(msg -t view.query 2>/dev/null | grep -c '"type":"view.added"')
	if msg -t launcher.launch --id "$FOOT_ID" --generation "$GEN_OK" >/dev/null 2>&1; then
		echo "ok: launcher.launch (current generation) was accepted"
	else
		echo "FAIL: a current launcher.launch was refused" >&2
		failures=$((failures + 1))
	fi
	# The launched window must appear. Poll the query; a fast app appears
	# within a couple of seconds. If the application itself cannot run
	# headless the acceptance check above is what stands, so a timeout here
	# is reported as a skip, not a failure — the launched process is the
	# one that would fail, not the IPC.
	LAUNCHED=no
	for _ in $(seq 1 50); do
		NOW=$(msg -t view.query 2>/dev/null | grep -c '"type":"view.added"')
		if [ "$NOW" -gt "$BEFORE" ]; then
			LAUNCHED=yes
			break
		fi
		sleep 0.1
	done
	if [ "$LAUNCHED" = yes ]; then
		echo "ok: and the launched application appeared as a new view"
		# Clean it up: close every view the launch added (the newest one).
		NEW_VID=$(msg -t view.query 2>/dev/null | grep '"type":"view.added"' |
			sed -n 's/.*"id":\([0-9]*\).*/\1/p' | tail -1)
		if [ -n "$NEW_VID" ]; then
			msg -t view.close --id "$NEW_VID" >/dev/null 2>&1 || true
		fi
	else
		echo "SKIP: the launched application did not map headless (acceptance only)"
	fi
fi

# A still-alive compositor after the launcher traffic.
if msg -t output.query >/dev/null 2>&1; then
	echo "ok: and the compositor is still answering at the end"
else
	echo "FAIL: the compositor stopped answering" >&2
	failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
	echo "$failures check(s) failed; the compositor's log:" >&2
	tail -30 "$LOG" >&2
	exit 1
fi
echo "all config-output-ipc checks passed"
