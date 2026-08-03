#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The Arch package, on Arch, in a window.
#
#   ./scripts/arch-vm.sh                     # newest package under packaging/arch/out
#   ./scripts/arch-vm.sh --variant cef
#   ./scripts/arch-vm.sh --package ~/viewport-smithay-wpe-0.1.1-1-x86_64.pkg.tar.zst
#   ./scripts/arch-vm.sh --shell                 # a login shell, no compositor
#
# `nix run .#vm` boots this compositor on NixOS, which proves the flake and
# nothing about the three PKGBUILDs beside it. Those declare their own
# dependencies against Arch's repositories, install their own wrapper, and are
# the thing an Arch user actually gets — and until now the only way to find out
# whether that worked was to have an Arch machine.
#
# So: an Arch cloud image, the built package handed to it, `pacman -U`, and the
# compositor started on tty1 with a real DRM device under it. Everything the
# package declares is resolved by pacman from the network the same way it would
# be on a real install, which is the half of the packaging that a container
# build cannot check — makepkg only needs the *build* dependencies to exist.
#
# Nothing is installed on the host. qemu and cloud-localds come from nixpkgs
# for the length of the run; the image and its overlay live in a cache
# directory and can be deleted.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cache=${XDG_CACHE_HOME:-$HOME/.cache}/viewport/arch-vm

# The rolling image, which is the point: a package that installs against last
# month's Arch and not today's is a package that is broken, and pinning a
# snapshot here would hide exactly that.
image_url=https://geo.mirror.pkgbuild.com/images/latest/Arch-Linux-x86_64-cloudimg.qcow2

variant=
package=
memory=6144
cores=4
shell_only=0
fresh=0
backend=

while [ $# -gt 0 ]; do
    case "$1" in
        --variant) variant=$2; shift 2 ;;
        --package) package=$2; shift 2 ;;
        --backend) backend=$2; shift 2 ;;
        --memory) memory=$2; shift 2 ;;
        --cores) cores=$2; shift 2 ;;
        --shell) shell_only=1; shift ;;
        # The overlay only. The base image is left alone, because re-downloading
        # 500 MB to undo a bad `pacman -U` is not a reset, it is a punishment.
        --fresh) fresh=1; shift ;;
        -h|--help) sed -n '3,25p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done

# The package to install. Named, or the newest built one — and never a -debug
# package, which is the larger of the two files makepkg leaves behind and
# installs no compositor at all.
if [ -z "$package" ]; then
    search=$here/packaging/arch/out
    [ -n "$variant" ] && search=$search/$variant
    package=$(find "$search" -name '*.pkg.tar.zst' ! -name '*-debug-*' \
        -printf '%T@ %p\n' 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2- || true)
fi
if [ -z "$package" ] || [ ! -f "$package" ]; then
    echo "no package to install." >&2
    echo "build one first:" >&2
    echo "    ./packaging/arch/build-in-container.sh ${variant:-wpe}" >&2
    exit 1
fi
package=$(cd "$(dirname "$package")" && pwd)/$(basename "$package")
echo "installing $(basename "$package")" >&2

# qemu and cloud-localds for the length of the run. Fetched rather than
# required, for the same reason bench-vkcube.sh fetches vkcube: this is a
# development machine's tool and making it a dependency of anything would be
# wrong.
if ! command -v qemu-system-x86_64 >/dev/null || ! command -v cloud-localds >/dev/null; then
    echo "fetching qemu and cloud-utils..." >&2
    # Rebuilt rather than "$@": the package has already been resolved to a path
    # by now, and re-running the search inside the nix shell would find it
    # again for no reason and differently if something was built meanwhile.
    again=(--package "$package" --memory "$memory" --cores "$cores")
    [ -n "$variant" ] && again+=(--variant "$variant")
    [ -n "$backend" ] && again+=(--backend "$backend")
    [ "$shell_only" = 1 ] && again+=(--shell)
    [ "$fresh" = 1 ] && again+=(--fresh)
    exec nix shell nixpkgs#qemu nixpkgs#cloud-utils --command "$0" "${again[@]}"
fi

mkdir -p "$cache"
base=$cache/arch-base.qcow2
if [ ! -f "$base" ]; then
    echo "downloading the Arch cloud image (about 500 MB, once)..." >&2
    curl -L --fail --progress-bar -o "$base.part" "$image_url"
    mv "$base.part" "$base"
fi

# A fresh copy-on-write layer per run by default: this exists to find out what
# installing the package does to a clean machine, and the second run of a
# script that kept its disk would be testing an upgrade instead.
overlay=$cache/arch-overlay.qcow2
if [ "$fresh" = 1 ] || [ ! -f "$overlay" ] || [ "$base" -nt "$overlay" ]; then
    rm -f "$overlay"
    qemu-img create -q -f qcow2 -F qcow2 -b "$base" "$overlay" 32G
fi

# cloud-init does the install, because the alternative is typing it. The
# package arrives over 9p rather than baked into the image, so rebuilding it
# and running this again does not touch the disk at all.
seed=$cache/seed.iso
# Emptied per run: a log from the last boot read as this one's is worse
# than no log at all.
xchg=$cache/xchg
rm -rf "$xchg"; mkdir -p "$xchg"
userdata=$cache/user-data

# What tty1 does after logging in. Built here rather than edited into the file
# afterwards: the `--shell` case used to be a sed over the generated YAML, and
# an `if` replaced by a comment leaves its own `fi` behind.
if [ "$shell_only" = 1 ]; then
    # Install the package and stop, which is what to run when the question is
    # about the packaging rather than about whether the desktop comes up.
    launcher='        : # --shell: not starting the compositor'
else
    launcher="        if [ -x /usr/bin/viewport ]; then
          exec viewport ${backend:+--shell-backend $backend} 2>&1 \\
            | tee /mnt/xchg/viewport.log
        else
          echo 'no /usr/bin/viewport: the package installed no compositor.' >&2
        fi"
fi
cat > "$userdata" <<EOF
#cloud-config
users:
  - name: viewport
    groups: [wheel, seat, video, input]
    sudo: ["ALL=(ALL) NOPASSWD:ALL"]
    lock_passwd: false
    # Set so tty2 onwards is usable when the compositor does not come up, which
    # is the only reason anyone would want to log in here by hand.
    plain_text_passwd: viewport
    shell: /bin/bash
ssh_pwauth: false
disable_root: true

write_files:
  - path: /etc/systemd/system/getty@tty1.service.d/autologin.conf
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin viewport --noclear %I \$TERM
  # In /etc/profile.d rather than the user's ~/.bash_profile, which is where
  # this started and did not run: cloud-init creates the user around the same
  # time it writes files, and a home directory that already exists is a home
  # directory useradd will not populate from /etc/skel — so which of the two
  # ~/.bash_profile survives depends on ordering nobody should have to know.
  # /etc/profile sources this for every login shell, and owns no such race.
  - path: /etc/profile.d/viewport.sh
    permissions: '0644'
    content: |
      # tty1 only: the other VTs stay a shell, which is where to look when the
      # compositor does not start.
      if [ "\$(tty)" = /dev/tty1 ]; then
$launcher
      fi
  - path: /etc/tmpfiles.d/viewport-xdg-runtime.conf
    content: |
      d /run/user/1000 0700 viewport viewport -

runcmd:
  # The package, over 9p. Not a repository: this is a file built minutes ago
  # and the whole point is to install *that* file.
  - [ mkdir, -p, /mnt/pkgs, /mnt/xchg ]
  - [ mount, -t, 9p, -o, "trans=virtio,ro,version=9p2000.L", pkgs, /mnt/pkgs ]
  # Writable, and shared with whoever started the VM. The compositor's log is
  # on tty1, which is a screen that has already gone black by the time there is
  # anything worth reading on it; this is the copy that can be read from
  # outside while it is still running. nix/vm.nix uses QEMU's own xchg share
  # for the same reason.
  - [ mount, -t, 9p, -o, "trans=virtio,version=9p2000.L", xchg, /mnt/xchg ]
  - [ chmod, "0777", /mnt/xchg ]
  # -Sy before -U so pacman can resolve what the package declares. This is the
  # part a container build never checks: makepkg needs the build dependencies
  # to exist, and says nothing about whether the runtime ones do.
  - [ pacman, -Sy, --noconfirm ]
  # `mesa` carries the virgl driver, which is what makes OpenGL in here the
  # host's GPU rather than llvmpipe: the compositor's EGL comes up on
  # /dev/dri/card0 through GBM and draws with virgl.
  #
  # `vulkan-virtio` rather than `vulkan-swrast`, which is the difference
  # between hardware Vulkan and none: Venus needs the host to offer it
  # (virtio-gpu-gl with venus, blob and hostmem all on), and where it is not
  # offered nothing loads and the compositor draws with OpenGL, which is
  # correct here. lavapipe
  # would load, own no DRM node, drive no display, and do every shell-frame
  # copy on the CPU, which is the software rendering worth not having.
  - [ pacman, -S, --noconfirm, --needed, mesa, vulkan-virtio, foot, vulkan-tools, mesa-utils ]
  # The bar draws its icons in a Nerd Font, named in data/shell/shell.css as
  # fontconfig reports them. An optdepend of the package rather than a
  # dependency, because a desktop with boxes where the icons go is still a
  # desktop — but a screenshot of one proves nothing, so the VM installs them.
  - [ pacman, -S, --noconfirm, --needed, ttf-firacode-nerd, ttf-nerd-fonts-symbols ]
  # The compositor, and not the 79 MB of detached debug symbols makepkg leaves
  # beside it — which installs a package with no compositor in it and takes
  # longer to do it than everything above put together.
  - [ sh, -c, "pacman -U --noconfirm \$(ls /mnt/pkgs/*.pkg.tar.zst | grep -v -- -debug-)" ]
  - [ systemctl, enable, --now, seatd ]
  - [ systemctl, restart, getty@tty1 ]

final_message: "the package is installed; tty1 has the compositor"
EOF

cat > "$cache/meta-data" <<'EOF'
instance-id: viewport-arch-vm
local-hostname: viewport-arch
EOF

cloud-localds "$seed" "$userdata" "$cache/meta-data"

# `-vga none` first: QEMU adds its default VGA regardless, and a guest with two
# display devices binds the wrong one. See nix/vm.nix, which has the same line
# for the same reason.
#
# `-cpu host` needs KVM; without it this falls back to emulation, which boots
# but paints a desktop far too slowly to judge anything by.
accel=tcg
cpu=max
[ -w /dev/kvm ] && { accel=kvm; cpu=host; }
[ "$accel" = tcg ] && echo "! no /dev/kvm: this will be slow" >&2

exec qemu-system-x86_64 \
    -machine "accel=$accel" -cpu "$cpu" \
    -m "$memory" -smp "$cores" \
    -name viewport-arch \
    -drive "if=virtio,format=qcow2,file=$overlay" \
    -drive "if=virtio,format=raw,file=$seed,readonly=on" \
    -netdev user,id=net0 -device virtio-net-pci,netdev=net0 \
    -virtfs "local,path=$(dirname "$package"),mount_tag=pkgs,security_model=none,readonly=on" \
    -virtfs "local,path=$xchg,mount_tag=xchg,security_model=none" \
    -device virtio-rng-pci \
    -device virtio-keyboard -usb -device usb-tablet \
    -vga none -device virtio-vga-gl \
    -display gtk,gl=on,show-cursor=on \
    -serial stdio
