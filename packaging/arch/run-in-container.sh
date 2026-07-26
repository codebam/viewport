#!/usr/bin/env bash
#
# Run the Arch package inside a container, on real hardware from a TTY.
#
# This is how to see what someone installing the package actually gets: a clean
# Arch with only what pacman pulls in, no Nix, no checkout, nothing left over
# from development. It drives the actual display and input devices, so it has to
# be started from a text console rather than from inside a graphical session.
#
#   ./packaging/arch/run-in-container.sh              # build and run on the TTY
#   ./packaging/arch/run-in-container.sh --rebuild    # rebuild the package first
#   ./packaging/arch/run-in-container.sh --shell      # a root shell in the container
#   ./packaging/arch/run-in-container.sh --nested     # a window in the session you are in
#   ./packaging/arch/run-in-container.sh --build-only # build the images, run nothing
#
# The TTY mode needs root, because taking DRM master and reading input devices
# is not something a rootless container can do. Without logind inside the
# container, libseat's builtin backend does that directly — which is exactly why
# it needs the privileges.
#
# Switch to a free console first (Ctrl+Alt+F3, say) and run it there. If it
# fails to start, Ctrl+Alt+F1 gets you back to whatever you were in.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
work=${TMPDIR:-/tmp}/viewport-container
pkgver=0.1.0

builder=localhost/viewport-builder
runtime=localhost/viewport-arch

rebuild=0
mode=tty
for arg in "$@"; do
	case $arg in
		--rebuild) rebuild=1 ;;
		--shell) mode=shell ;;
		--nested) mode=nested ;;
		--build-only) mode=build ;;
		-h|--help) sed -n '2,24p' "$0" | sed 's/^# \?//'; exit 0 ;;
		*) echo "unknown option: $arg" >&2; exit 1 ;;
	esac
done

# Checked before anything is built: someone who runs this from inside their
# desktop should find out in a second, not after two image builds.
if [ "$mode" = tty ] && [ -n "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]; then
	echo "refusing to take the display: this looks like a graphical session." >&2
	echo "switch to a text console (Ctrl+Alt+F3) and run it there, or pass" >&2
	echo "--nested to open it as a window here instead." >&2
	exit 1
fi

mkdir -p "$work"

# ---------------------------------------------------------------------------
# Build the package, as an ordinary user.
#
# makepkg refuses to run as root and a rootless container normally gives you
# only root, so the dependencies go in while building the image and the package
# itself is built afterwards by a mapped ordinary user.
# ---------------------------------------------------------------------------
if ! podman image exists "$builder" || [ "$rebuild" = 1 ]; then
	echo "==> building $builder"
	podman build -t "$builder" -f "$here/Containerfile" "$here"
fi

if [ "$rebuild" = 1 ] || ! compgen -G "$work/viewport-$pkgver-*-x86_64.pkg.tar.zst" >/dev/null; then
	echo "==> building the package from HEAD"
	rm -f "$work"/*.pkg.tar.zst "$work/viewport-$pkgver.tar.gz"

	# From HEAD rather than the working tree: a package should be rebuildable
	# from what is committed, and a tarball of uncommitted edits quietly is not.
	git -C "$repo" archive --format=tar.gz --prefix="viewport-$pkgver/" \
		-o "$work/viewport-$pkgver.tar.gz" HEAD
	cp "$here/PKGBUILD" "$work/"

	podman run --rm --userns=keep-id:uid=1000,gid=1000 -e HOME=/tmp \
		-v "$work:/out:z" "$builder" bash -c '
			set -e
			mkdir -p /tmp/work && cd /tmp/work
			cp /out/PKGBUILD /out/viewport-*.tar.gz .
			makepkg --noconfirm --nodeps
			cp *.pkg.tar.zst /out/
		'
fi

# ---------------------------------------------------------------------------
# A clean Arch with the package installed from the file, exactly as someone
# receiving it would install it.
# ---------------------------------------------------------------------------
echo "==> building $runtime"
cp "$(ls -t "$work"/viewport-"$pkgver"-*-x86_64.pkg.tar.zst | head -1)" \
	"$work/viewport.pkg.tar.zst"

cat > "$work/Containerfile.runtime" <<'EOF'
FROM docker.io/library/archlinux:latest
COPY viewport.pkg.tar.zst /tmp/
# foot and a font so there is something to open and something to read; mesa and
# the Vulkan driver so the renderer has one; seatd for libseat; xwayland for X11
# clients. --disable-sandbox because pacman cannot drop privileges for its
# downloads inside a rootless container.
RUN pacman -Syu --noconfirm --disable-sandbox --needed \
      foot ttf-dejavu mesa vulkan-radeon vulkan-icd-loader \
      seatd xorg-xwayland wmenu \
 && pacman -U --noconfirm --disable-sandbox /tmp/viewport.pkg.tar.zst \
 && rm /tmp/viewport.pkg.tar.zst \
 && pacman -Scc --noconfirm
EOF
podman build -t "$runtime" -f "$work/Containerfile.runtime" "$work"

# ---------------------------------------------------------------------------
# Run it.
# ---------------------------------------------------------------------------
if [ "$mode" = nested ]; then
	host_socket=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is not set}/${WAYLAND_DISPLAY:?WAYLAND_DISPLAY is not set}
	[ -S "$host_socket" ] || { echo "no wayland socket at $host_socket" >&2; exit 1; }

	# Only the one socket is bound in. Mounting the whole runtime directory
	# would hand the container every other socket in it as well.
	exec podman run --rm -it \
		--userns=keep-id:uid=1000,gid=1000 \
		--group-add keep-groups \
		--device /dev/dri \
		-v "$host_socket:/tmp/xdg/wayland-0" \
		-e XDG_RUNTIME_DIR=/tmp/xdg \
		-e WAYLAND_DISPLAY=wayland-0 \
		-e WLR_BACKENDS=wayland \
		-e HOME=/tmp \
		"$runtime" viewport --startup foot
fi

if [ "$mode" = build ]; then
	echo "==> images built; nothing started"
	exit 0
fi

# Root, and with the devices: DRM master, every input device, and udev for
# libinput to enumerate them by. LIBSEAT_BACKEND=builtin is what stands in for
# logind, which is not running in here — it opens the devices directly, which
# only works because this is root.
#
# --privileged rather than a list of capabilities: this is a throwaway container
# driving real hardware, and enumerating exactly which capabilities libinput and
# amdgpu need between kernel versions is a worse trade than granting them.
tty_args=(
	--rm -it
	--privileged
	-v /dev:/dev
	-v /run/udev:/run/udev:ro
	-e LIBSEAT_BACKEND=builtin
	-e XDG_RUNTIME_DIR=/tmp/xdg
	-e HOME=/root
)

if [ "$mode" = shell ]; then
	echo "==> root shell in the container; run 'viewport' when you are ready"
	exec sudo podman run "${tty_args[@]}" "$runtime" bash
fi

echo "==> starting viewport on this console (Mod4+Shift+e to quit)"
exec sudo podman run "${tty_args[@]}" "$runtime" \
	bash -c 'mkdir -p /tmp/xdg && chmod 700 /tmp/xdg && exec viewport --startup foot'
