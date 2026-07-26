#!/usr/bin/env bash
#
# Collect one file describing what happened, for reporting a problem.
#
# A log on its own rarely settles anything: the same line means different things
# on different hardware, with a different renderer, or against a different
# build. This gathers the log together with the facts needed to read it — which
# commit, which wlroots, which GPU, what the config actually said — into a
# single file to hand over.
#
#   ./scripts/collect-report.sh              # newest log it can find
#   ./scripts/collect-report.sh path/to.log  # a particular one
#
# Nothing is uploaded anywhere. The result is a plain file; read it before
# sending it on.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=${VIEWPORT_REPORT:-$HOME/viewport-report-$(date +%Y%m%d-%H%M%S).txt}

# Where a log might be, newest first: the container runs, then start.sh's, then
# the rotated copy start.sh keeps of the run before this one.
find_log() {
	if [ $# -gt 0 ]; then
		printf '%s\n' "$1"
		return
	fi
	local candidates=()
	[ -d "${VIEWPORT_LOGDIR:-$HOME/viewport-logs}" ] &&
		while IFS= read -r line; do candidates+=("$line"); done < <(
			find "${VIEWPORT_LOGDIR:-$HOME/viewport-logs}" -name 'viewport-*.log' \
				-printf '%T@ %p\n' 2>/dev/null | sort -rn | cut -d' ' -f2-
		)
	[ -f "$HOME/viewport.log" ] && candidates+=("$HOME/viewport.log")
	[ -f "$HOME/viewport.log.1" ] && candidates+=("$HOME/viewport.log.1")
	[ ${#candidates[@]} -gt 0 ] && printf '%s\n' "${candidates[0]}"
}

log=$(find_log "$@" || true)

section() { printf '\n===== %s =====\n' "$1"; }

{
	printf 'viewport report, %s\n' "$(date -Is)"

	section "version"
	if git -C "$repo" rev-parse --git-dir >/dev/null 2>&1; then
		printf 'commit:  %s\n' "$(git -C "$repo" describe --always --dirty)"
		printf 'subject: %s\n' "$(git -C "$repo" log -1 --pretty=%s)"
	fi
	# The installed copy may be a different build from the checkout, and which
	# one was running is usually the first thing worth knowing.
	command -v viewport >/dev/null && printf 'installed: %s\n' "$(command -v viewport)"
	[ -x "$repo/build/viewport" ] && printf 'checkout build: %s\n' \
		"$(date -r "$repo/build/viewport" -Is 2>/dev/null)"

	section "system"
	printf 'kernel:  %s\n' "$(uname -sr)"
	[ -r /etc/os-release ] && . /etc/os-release && printf 'distro:  %s\n' "${PRETTY_NAME:-unknown}"
	printf 'session: %s\n' "${XDG_SESSION_TYPE:-none} (WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-unset})"

	section "graphics"
	for card in /sys/class/drm/card*/device/driver; do
		[ -e "$card" ] || continue
		printf 'driver:  %s\n' "$(basename "$(readlink -f "$card")")"
		break
	done
	for connector in /sys/class/drm/*-*/status; do
		[ -r "$connector" ] || continue
		name=$(basename "$(dirname "$connector")")
		printf 'output:  %-24s %s\n' "$name" "$(cat "$connector")"
	done

	section "config"
	config=${XDG_CONFIG_HOME:-$HOME/.config}/viewport/config.json
	if [ -r "$config" ]; then
		printf 'from %s\n' "$config"
		cat "$config"
	else
		printf 'no config file; built-in defaults are in use\n'
	fi

	section "log"
	if [ -n "$log" ] && [ -r "$log" ]; then
		printf 'from %s (%s lines)\n\n' "$log" "$(wc -l < "$log")"
		# The whole thing when it is small, otherwise the ends: a failure shows
		# up either where it started or where everything stopped, and the middle
		# of a long run is mostly frame timing.
		if [ "$(wc -l < "$log")" -le 400 ]; then
			cat "$log"
		else
			head -120 "$log"
			printf '\n... [middle omitted] ...\n\n'
			tail -260 "$log"
		fi
	else
		printf 'no log found. start.sh writes ~/viewport.log; the container\n'
		printf 'script writes into ~/viewport-logs. Pass a path to use another.\n'
	fi
} > "$out" 2>&1

echo "wrote $out"
printf 'log used: %s\n' "${log:-none}"
echo
echo "It is a plain text file — read it before sending it on. It contains your"
echo "config, your output names and the compositor log, and nothing else."
