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

cd "$(dirname "$(readlink -f "$0")")"

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
export XDG_CURRENT_DESKTOP=viewport
export XDG_DATA_DIRS="$PWD/data/portal-share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
# Backend selection moved from the .portal file's UseIn= to a config file in
# xdg-desktop-portal 1.18; 1.20 ignores UseIn entirely. Shipped in-tree rather
# than written into ~/.config so nothing outside the repo is touched.
export XDG_CONFIG_DIRS="$PWD/data/portal-config:${XDG_CONFIG_DIRS:-/etc/xdg}"

if command -v dbus-update-activation-environment >/dev/null 2>&1; then
	dbus-update-activation-environment --systemd \
		XDG_CURRENT_DESKTOP XDG_DATA_DIRS XDG_CONFIG_DIRS 2>/dev/null || true
fi

# The portal caches its backend list at startup, so a portal already running
# from a previous session will not see ours until it restarts.
systemctl --user stop xdg-desktop-portal.service 2>/dev/null || true

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

exec nix develop --command ./build/viewport \
	--url      "file://$PWD/data/shell/index.html" \
	--fallback "file://$PWD/data/fallback.html" \
	--terminal ghostty \
	--menu     wmenu-run \
	--startup  ghostty \
	--debug "$@" >"$LOG" 2>&1
