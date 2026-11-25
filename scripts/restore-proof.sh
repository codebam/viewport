#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The exit restore, on the machine that actually has a panel.
#
# Started by viewport-bench@restore-proof.service on tty2 with a logind seat
# session, the same way the benchmark harness is. Install it beside that
# harness and start the unit:
#
#   cp scripts/restore-proof.sh ~/bench/restore-proof.sh
#   systemctl start viewport-bench@restore-proof.service
#
# The unit lives in `steamdeck/configuration/bench.nix` in codebam/nixos. Point
# VIEWPORT_BIN at the build under test, or leave it and put that build at
# `~/bench/viewport-restore`.
#
# What it establishes:
#
#   * the compositor runs the atomic restore on its way out — the fork logs
#     "trying to restore previous state" at debug, which this asks its log
#     filter for. It is called from the compositor's own teardown, not left
#     to the DRM device's `Drop`, which a real exit does not reach;
#   * the restore is accepted by the kernel — no "Failed to restore previous
#     state" line, which the old build was answered `EINVAL` on every exit;
#   * a night-light tint does not survive the session: gammastep is still
#     running when the compositor quits, so the only thing that can put the
#     pre-session gamma LUT back is the restore itself.
#
# The last one is also a thing to look at. When this unit finishes it returns
# the VT to tty1; a console that is its normal colour is the pass, and an
# orange cast is the failure the fork's fifth patch exists for.
set -uo pipefail
cd "$(dirname "$0")"

BIN=${VIEWPORT_BIN:-$PWD/viewport-restore/bin/viewport}
LOG=$PWD/restore-proof.log
RUNTIME=${XDG_RUNTIME_DIR:-/run/user/1000}
CONFIG=$PWD/restore-proof.json
: >"$LOG"
say() { echo "$*" | tee -a "$LOG"; }

if [ ! -x "$BIN" ]; then
	say "FAIL: no compositor at $BIN"
	exit 2
fi
# A minimal config: the proof is about the DRM exit path, and the deck's own
# config carries a desktop worth of startup commands that would only add noise.
printf '{}\n' >"$CONFIG"

VIEWPORT_LOG="info,smithay::backend::drm=debug" \
	"$BIN" --drm --config "$CONFIG" --exit-after 40 >>"$LOG" 2>&1 &
VP=$!

display=
for _ in $(seq 1 400); do
	display=$(sed -n 's/.*WAYLAND_DISPLAY=\([^ ]*\).*/\1/p' "$LOG" | head -1)
	[ -n "$display" ] && break
	kill -0 "$VP" 2>/dev/null || break
	sleep 0.1
done
if [ -z "$display" ]; then
	say "FAIL: the compositor never announced a display"
	tail -20 "$LOG" | sed 's/^/    /'
	kill "$VP" 2>/dev/null
	exit 1
fi
SOCK="$RUNTIME/viewport-$display.sock"
if [ ! -S "$SOCK" ]; then
	say "FAIL: no control socket at $SOCK"
	kill "$VP" 2>/dev/null
	exit 1
fi
say "ok: compositor up on $display"

# A tint held by a client that stays connected until the compositor is gone.
# gammastep speaks wlr-gamma-control, which is how a night light sets the LUT.
WAYLAND_DISPLAY="$display" nix shell nixpkgs#gammastep -c \
	gammastep -m wayland -l 0:0 -t 3000 -T 3000 >>"$LOG" 2>&1 &
GAMMA=$!
sleep 5
if grep -qi "temperature" "$LOG"; then
	say "ok: gammastep applied a 3000K tint"
else
	say "warn: gammastep said nothing about the temperature; the restore check still stands"
fi

# Quit while the tint client is still running: nothing resets the ramp on the
# way out except the compositor's own restore.
"$BIN" msg -t quit >>"$LOG" 2>&1
wait "$VP"
STATUS=$?
kill "$GAMMA" 2>/dev/null
say "ok: compositor exited with status $STATUS"

failed=no
grep -q "Failed to restore previous state" "$LOG" && failed=yes
restored=no
grep -q "trying to restore previous state" "$LOG" && restored=yes

if [ "$failed" = yes ]; then
	say "FAIL: the exit restore was refused again (the EINVAL symptom)"
	grep -n "Failed to restore previous state" "$LOG" | sed 's/^/    /'
	exit 1
fi
if [ "$restored" != yes ]; then
	say "FAIL: the restore never ran (no 'trying to restore previous state' in the log)"
	exit 1
fi
say "PASS: the restore ran and the kernel accepted it"
say "now look at this VT: tty1 must be its normal colour, not orange."
