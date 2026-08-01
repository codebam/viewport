#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Virtual monitors, for testing multi-output behaviour without monitors.
#
#   ./scripts/virtual-monitors.sh up          # two virtual screens
#   ./scripts/virtual-monitors.sh up 3        # three
#   ./scripts/virtual-monitors.sh status      # what exists now
#   ./scripts/virtual-monitors.sh unplug 0    # hotplug: pull the first one out
#   ./scripts/virtual-monitors.sh plug 0      # and put it back
#   ./scripts/virtual-monitors.sh down        # remove everything this made
#
# Why this exists: most of what goes wrong in this compositor goes wrong on the
# second output or the second GPU, and neither is reachable from the test
# suite. `control_socket.rs` churns hotplug against the *headless* backend,
# which has its own output code — so a DRM-only fault reads as a passing test.
# Ten bugs were found in one afternoon by running the real compositor against
# the screens this makes, including a monitor that was never released when
# unplugged and an active output that went on naming a screen that had gone.
#
# It needs root, and takes it through run0 or sudo. Everything it creates lives
# in memory: nothing here survives a reboot, and `down` undoes all of it.
set -euo pipefail

# ---------------------------------------------------------------------------
# Root, and how to get it.
#
# NixOS has neither sudo nor doas by default and does have run0, so both are
# tried rather than one being assumed.
# ---------------------------------------------------------------------------
if [ "$(id -u)" = 0 ]; then
    as_root() { "$@"; }
elif command -v run0 >/dev/null 2>&1; then
    as_root() { run0 --no-ask-password "$@"; }
elif command -v sudo >/dev/null 2>&1; then
    as_root() { sudo "$@"; }
else
    echo "no run0 and no sudo: this needs root to load vkms and write configfs." >&2
    exit 1
fi

DEVICE=${VIRTUAL_MONITORS_NAME:-bench}
CONFIGFS=/sys/kernel/config/vkms/$DEVICE
RULE=/run/udev/rules.d/99-vkms-seat.rules
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# configfs takes a value from a file, and a shell redirect into a root-owned
# path needs the redirect itself to be root. Writing a temporary file and
# copying it is the way that works through both run0 and sudo.
write() { printf '%s' "$2" >"$tmp/v"; as_root cp "$tmp/v" "$1"; }

cards() { ls -d /sys/class/drm/card[0-9]*/ 2>/dev/null | xargs -n1 basename; }

status() {
    echo "cards:"
    for c in $(cards); do
        case "$c" in
            *-*) continue ;;
        esac
        # No `| grep -q`: with pipefail, grep exiting on its first match
        # sends SIGPIPE to the left-hand side and the pipeline reports that
        # failure, so the test reads backwards. Match on the string instead.
        case "$(readlink -f "/sys/class/drm/$c/device" 2>/dev/null)" in
            *faux*) printf "  %-8s %s\n" "$c" "(virtual)" ;;
            *) printf "  %-8s %s\n" "$c" "(real)" ;;
        esac
    done
    echo "connectors:"
    for s in /sys/class/drm/card*-*/status; do
        [ -r "$s" ] || continue
        printf "  %-20s %s\n" "$(basename "$(dirname "$s")")" "$(cat "$s")"
    done
    # Asked of /sys rather than lsmod, for the same reason: no pipe to be
    # tripped by, and it is the kernel's own answer.
    printf "vkms module: %s\n" "$([ -d /sys/module/vkms ] && echo loaded || echo 'not loaded')"
    printf "seat rule:   %s\n" "$([ -e "$RULE" ] && echo installed || echo absent)"
}

up() {
    local count=${1:-2}
    if [ "$count" -lt 1 ] || [ "$count" -gt 8 ]; then
        echo "between one and eight, please." >&2
        exit 2
    fi

    as_root modprobe vkms
    # The module makes one screen of its own on load — `create_default_dev` —
    # which is not the one this configures and is left alone. Anything wanting
    # only the configured ones can load with create_default_dev=0.

    if [ ! -d /sys/kernel/config/vkms ]; then
        echo "this kernel's vkms has no configfs support, so the number of" >&2
        echo "screens cannot be chosen. It needs roughly 6.13 or newer." >&2
        exit 1
    fi

    if as_root test -d "$CONFIGFS"; then
        echo "$DEVICE already exists; run 'down' first." >&2
        exit 1
    fi

    as_root mkdir "$CONFIGFS"

    # One CRTC, one encoder, one connector and one plane per screen, wired
    # together. Each piece is a directory and each wiring a symlink; the shape
    # is the same one a real card exposes, which is the point — the compositor
    # cannot tell these apart from hardware.
    local i
    for (( i = 0; i < count; i++ )); do
        as_root mkdir "$CONFIGFS/planes/plane$i"
        as_root mkdir "$CONFIGFS/crtcs/crtc$i"
        as_root mkdir "$CONFIGFS/encoders/encoder$i"
        as_root mkdir "$CONFIGFS/connectors/connector$i"
    done
    for (( i = 0; i < count; i++ )); do
        as_root ln -s "$CONFIGFS/crtcs/crtc$i" "$CONFIGFS/planes/plane$i/possible_crtcs/crtc$i"
        as_root ln -s "$CONFIGFS/crtcs/crtc$i" "$CONFIGFS/encoders/encoder$i/possible_crtcs/crtc$i"
        as_root ln -s "$CONFIGFS/encoders/encoder$i" \
            "$CONFIGFS/connectors/connector$i/possible_encoders/encoder$i"
        # 1 is DRM_PLANE_TYPE_PRIMARY. A CRTC with no primary plane cannot be
        # used for a modeset at all, and the default here is overlay.
        write "$CONFIGFS/planes/plane$i/type" 1
    done

    # So logind will hand the device over.
    #
    # libseat asks logind for the device, and logind refuses one that carries no
    # ID_FOR_SEAT — which a vkms card does not, because it hangs off the faux
    # bus rather than a real one. Without this the compositor opens the card,
    # fails to take DRM master, and carries on in "unprivileged mode": it can
    # read the connectors and never set a mode, so every virtual screen stays
    # black and the only clue is one warning from deep inside Smithay.
    #
    # In /run rather than /etc because /etc/udev/rules.d is read-only on NixOS,
    # and because nothing here should outlive the machine's uptime. Scoped to
    # the faux bus so no real GPU's seat assignment is touched.
    as_root mkdir -p /run/udev/rules.d
    cat >"$tmp/rule" <<'EOF'
# Virtual DRM devices (vkms), so libseat can hand them to a compositor.
# Removed by scripts/virtual-monitors.sh down.
ACTION!="remove", SUBSYSTEM=="drm", KERNEL=="card[0-9]*", DEVPATH=="*/faux/*", \
  TAG+="seat", TAG+="master-of-seat", ENV{ID_FOR_SEAT}="drm-$kernel", ENV{ID_SEAT}="seat0"
EOF
    as_root cp "$tmp/rule" "$RULE"
    as_root udevadm control --reload

    write "$CONFIGFS/enabled" 1
    # The rule has to be in place before the device appears for the properties
    # to be applied to it, and the trigger covers the case where it was not.
    as_root udevadm trigger --subsystem-match=drm --action=change
    sleep 2

    echo
    status
    cat <<'NOTES'

Running the compositor against these
------------------------------------
DRM master needs a session with a seat, which a tmux or ssh shell does not
have. Borrow the active one:

  s=$(loginctl list-sessions --no-legend | awk '$4=="seat0"{print $1}' \
      | while read i; do [ "$(loginctl show-session $i -p Active --value)" = yes ] \
      && echo $i; done | head -1)
  nix develop -c env XDG_SESSION_ID=$s XDG_VTNR=$(loginctl show-session $s -p VTNr --value) \
      XDG_SEAT=seat0 ./target/release/viewport --drm --exit-after 30

Two things these cannot do
--------------------------
No render node. Clients are told to allocate on the card they are shown on,
and a vkms card has only a primary node, which a client may not open — so a
compositor whose *primary* GPU is virtual gives every client "failed to get
driver name for fd -1". Keep a real GPU primary and let these be secondary,
which is what happens when VIEWPORT_GPU is left unset.

No Vulkan, and no syncobj. vkcube segfaults on them. glmark2-es2-wayland
works, since it is GLES:

  nix run nixpkgs#glmark2 -- --help   # glmark2-es2-wayland is in the same output

That also means the fifo/commit-timing pacing path cannot be exercised here:
it is Mesa's Vulkan present mode that uses wp_fifo_v1, and a GLES client paces
off frame callbacks instead. Frame-pacing work needs real monitors.
NOTES
}

# ---------------------------------------------------------------------------
# Hotplug. A vkms connector's status is writable, so a monitor can be pulled
# out and put back under a running compositor — on real DRM, which is the part
# the headless backend cannot imitate. This is how the unplug path was found to
# be missing entirely.
#
# The values are the DRM enum: 1 connected, 2 disconnected. 0 is rejected.
# ---------------------------------------------------------------------------
plug()   { write "$CONFIGFS/connectors/connector${1:-0}/status" 1; echo "connector${1:-0} plugged in"; }
unplug() { write "$CONFIGFS/connectors/connector${1:-0}/status" 2; echo "connector${1:-0} unplugged"; }

down() {
    if as_root test -d "$CONFIGFS"; then
        write "$CONFIGFS/enabled" 0 || true
        # Symlinks before directories, innermost first: configfs refuses to
        # remove anything still referenced.
        local i
        for (( i = 0; i < 8; i++ )); do
            as_root rm -f "$CONFIGFS/planes/plane$i/possible_crtcs/crtc$i" 2>/dev/null || true
            as_root rm -f "$CONFIGFS/encoders/encoder$i/possible_crtcs/crtc$i" 2>/dev/null || true
            as_root rm -f "$CONFIGFS/connectors/connector$i/possible_encoders/encoder$i" 2>/dev/null || true
        done
        for (( i = 0; i < 8; i++ )); do
            as_root rmdir "$CONFIGFS/connectors/connector$i" 2>/dev/null || true
            as_root rmdir "$CONFIGFS/encoders/encoder$i" 2>/dev/null || true
            as_root rmdir "$CONFIGFS/crtcs/crtc$i" 2>/dev/null || true
            as_root rmdir "$CONFIGFS/planes/plane$i" 2>/dev/null || true
        done
        as_root rmdir "$CONFIGFS" 2>/dev/null || true
    fi

    as_root rm -f "$RULE"
    as_root udevadm control --reload 2>/dev/null || true
    # Takes the module's own default screen with it. Fails harmlessly if
    # something still holds a card, which is worth seeing rather than hiding.
    as_root modprobe -r vkms 2>&1 | head -2 || true
    sleep 1
    echo
    status
}

case "${1:-}" in
    up) shift; up "${1:-2}" ;;
    down) down ;;
    status) status ;;
    plug) shift; plug "${1:-0}" ;;
    unplug) shift; unplug "${1:-0}" ;;
    *) sed -n '3,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
