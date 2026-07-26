#!/usr/bin/env bash
# Launch viewport from a TTY.
#
# Paths are explicit rather than relying on defaults: nothing is installed to
# $prefix yet, so the built-in fallback URL points at
# /usr/local/share/viewport/fallback.html, which does not exist. A shell that
# failed to load would then fall back to a 404 and leave a blank desktop.
#
# --startup is the safety net. Keybindings have not been exercised on real
# hardware, and if Mod4+Return does not fire you would have no way to open a
# terminal. Launching one unconditionally means the session is usable even if
# every binding is broken.
#
# Escape hatches, in order:
#   Ctrl+Alt+F2      switch VT (checked before config, bindings and the shell,
#                    so it works even when all three are broken)
#   Mod4+Shift+e     exit binding
#   pkill -TERM viewport   from another VT
set -euo pipefail

REPO="$(dirname "$(readlink -f "$0")")"
cd "$REPO"

# stdenv exports SHELL as an absolute path to the non-interactive bash, and
# `nix develop` passes that through to the compositor and on to every terminal
# it spawns. That bash is built --disable-readline --disable-progcomp, so
# ~/.bashrc errors out and prompt escapes render literally. Read the real login
# shell from passwd rather than $SHELL: this script may itself be started from
# inside a nix shell, in which case $SHELL is already wrong.
LOGIN_SHELL="$(getent passwd "$(id -un)" | cut -d: -f7)"
: "${LOGIN_SHELL:=${SHELL:-/bin/sh}}"

LOG="${VIEWPORT_LOG:-$HOME/viewport.log}"

# Dark mode for client applications depends on three things lining up, none of
# which happen automatically when running from the build tree.
#
#   XDG_CURRENT_DESKTOP  xdg-desktop-portal picks a backend by matching this
#                        against each .portal file's UseIn= line.
#   XDG_DATA_DIRS        it only finds our .portal file if the directory
#                        containing it is on the search path. Installed builds
#                        get this for free; running from ./build does not.
#   activation env       the portal is D-Bus activated and inherits its
#                        environment from the session, not from us, so the two
#                        variables above have to be pushed there before it
#                        starts — otherwise it looks for a backend named after
#                        whatever desktop was current at login.
# VIEWPORT_NO_PORTAL=1 disables our settings portal, falling back to whatever
# backend the session already had. Dark mode stops working; the point is to
# isolate the portal when something else is being investigated.
#
# Skipping the setup is not enough on its own: systemctl --user set-environment
# persists across runs, so variables exported by a previous launch would keep
# our backend selected and the flag would appear to do nothing at all.
if [ -n "${VIEWPORT_NO_PORTAL:-}" ]; then
	systemctl --user unset-environment \
		NIX_XDG_DESKTOP_PORTAL_DIR XDG_CURRENT_DESKTOP 2>/dev/null || true
	systemctl --user restart xdg-desktop-portal.service 2>/dev/null || true
else
export XDG_CURRENT_DESKTOP=viewport
export XDG_DATA_DIRS="$PWD/data/portal-share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
# Backend selection moved from the .portal file's UseIn= to a config file in
# xdg-desktop-portal 1.18; 1.20 ignores UseIn entirely. Shipped in-tree rather
# than written into ~/.config so nothing outside the repo is touched.
export XDG_CONFIG_DIRS="$PWD/data/portal-config:${XDG_CONFIG_DIRS:-/etc/xdg}"

if command -v dbus-update-activation-environment >/dev/null 2>&1; then
	dbus-update-activation-environment --systemd \
		XDG_CURRENT_DESKTOP XDG_DATA_DIRS XDG_CONFIG_DIRS \
		NIX_XDG_DESKTOP_PORTAL_DIR 2>/dev/null || true
fi

# On NixOS, xdg-desktop-portal is patched to read portal definitions from a
# single directory named by NIX_XDG_DESKTOP_PORTAL_DIR, and ignores
# XDG_DATA_DIRS entirely — so dropping viewport.portal onto the data path has
# no effect there no matter how it is arranged. Build a directory holding the
# system portals plus ours and point the variable at it.
if [ -n "${NIX_XDG_DESKTOP_PORTAL_DIR:-}" ]; then
	merged="${XDG_RUNTIME_DIR:-/tmp}/viewport-portals"
	rm -rf "$merged"
	mkdir -p "$merged"
	for portal in "$NIX_XDG_DESKTOP_PORTAL_DIR"/*.portal; do
		[ -e "$portal" ] && ln -sf "$portal" "$merged/"
	done
	ln -sf "$PWD/data/portal-share/xdg-desktop-portal/portals/viewport.portal" \
		"$merged/"
	export NIX_XDG_DESKTOP_PORTAL_DIR="$merged"
	systemctl --user set-environment \
		"NIX_XDG_DESKTOP_PORTAL_DIR=$merged" 2>/dev/null || true
fi

# The portal caches its backend list at startup, so one already running from a
# previous session will not see ours until it restarts.
systemctl --user restart xdg-desktop-portal.service 2>/dev/null \
	|| systemctl --user stop xdg-desktop-portal.service 2>/dev/null || true
fi

if [ ! -x build/viewport ]; then
	echo "build/viewport missing — run: nix develop --command ninja -C build" >&2
	exit 1
fi

# Keep the previous run's log: a crash is followed by a restart, and
# truncating here destroys the only record of why it died.
# Written as an `if` deliberately: under `set -e`, `[ -f x ] && mv ...` exits
# the script when the file is absent, which would break the very first run.
if [ -f "$LOG" ]; then
	mv -f "$LOG" "$LOG.1"
fi

echo "logging to $LOG (previous run: $LOG.1)"

# --debug mirrors the shell's console into the log and serves the shell
# uncached, which is what makes a JavaScript error visible at all and an edited
# shell take effect. It is cheap and stays on.
#
# --trace is the expensive tier: one line per window per frame, which during an
# animation is sixty a second and formatted I/O inside the layout path. Pass it
# when chasing a geometry bug:  ./start.sh --trace

# Spawned clients inherit the compositor's cwd, so a terminal opened from the
# shell would otherwise land in the build tree rather than $HOME. Every path
# handed to the compositor is absolute, so the chdir costs nothing.
cd "$HOME"

exec nix develop "$REPO" --command env SHELL="$LOGIN_SHELL" "$REPO/build/viewport" \
	--url      "file://$REPO/data/shell/index.html" \
	--fallback "file://$REPO/data/fallback.html" \
	--terminal rio \
	--menu     wmenu-run \
	--startup  rio \
	--debug "$@" >"$LOG" 2>&1
