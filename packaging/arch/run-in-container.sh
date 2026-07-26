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
# is not something a rootless container can do. There is no logind in the
# container to ask either, so a seatd is started inside it to hand out the
# devices. Whichever of run0, sudo or doas is installed is used; set
# VIEWPORT_ELEVATE to name another.
#
# Switch to a free console first (Ctrl+Alt+F3, say) and run it there. If it
# fails to start, Ctrl+Alt+F1 gets you back to whatever you were in.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
work=${TMPDIR:-/tmp}/viewport-container
logdir=${VIEWPORT_LOGDIR:-$HOME/viewport-logs}
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

# How to become root. Not everyone has sudo — a systemd machine may have only
# run0, and installing a sudo shim to satisfy one script is a system-wide change
# to work around a three-line problem.
elevate=()
if [ -n "${VIEWPORT_ELEVATE:-}" ]; then
	read -r -a elevate <<< "$VIEWPORT_ELEVATE"
elif [ "$(id -u)" = 0 ]; then
	elevate=()
else
	for candidate in run0 sudo doas; do
		if command -v "$candidate" >/dev/null; then
			elevate=("$candidate")
			break
		fi
	done
fi

mkdir -p "$work"

# Everything from here is logged, not just the compositor.
#
# The first version captured only what the compositor printed, which meant the
# one failure that actually happened — the run never getting that far — left no
# trace at all. A build that cannot find an image, an elevation that is refused,
# a package that will not compile: those are the interesting failures, and they
# all happen before the compositor exists.
if [ "$mode" != build ]; then
	mkdir -p "$logdir"
	stamp=$(date +%Y%m%d-%H%M%S)
	logfile=$logdir/viewport-$stamp.log
	# tee, so the console still shows progress. Written as the ordinary user
	# from the outset, so nothing needs its ownership repaired afterwards.
	exec > >(tee -a "$logfile") 2>&1
	echo "==> logging to $logfile"
fi

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
# libinput to enumerate them by.
#
# Seat management is the part with no obvious answer inside a container. logind
# is not running in here, and libseat's builtin backend — which would open the
# devices directly — is a build-time option Arch does not enable, so asking for
# it fails with "No backend matched name 'builtin'". What is left is to run a
# seatd of our own inside the container and point libseat at that.
#
# --privileged rather than a list of capabilities: this is a throwaway container
# driving real hardware, and enumerating exactly which capabilities libinput and
# amdgpu need between kernel versions is a worse trade than granting them.
tty_args=(
	--rm -it
	--privileged
	-v /dev:/dev
	-v /run/udev:/run/udev:ro
	-e LIBSEAT_BACKEND=seatd
	-e XDG_RUNTIME_DIR=/tmp/xdg
	-e HOME=/root
	# WebKit sandboxes its web process with bubblewrap, and inside a privileged
	# container that sandbox is available enough to be used and broken enough
	# not to work: the page loads, its scripts never run, and the desktop comes
	# up with no bar and no layout. Rootless containers avoid this by accident,
	# because WebKit notices bubblewrap cannot work at all and turns itself off
	# — the only difference between the nested run, which worked, and this one,
	# which did not.
	#
	# Turned off deliberately here. This is a throwaway container built from a
	# known package, rendering a shell that ships inside it; there is no
	# untrusted content for the sandbox to be protecting anything from.
	-e WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1
)

if [ ${#elevate[@]} -eq 0 ] && [ "$(id -u)" != 0 ]; then
	echo "no way to become root: install one of run0, sudo or doas, or set" >&2
	echo "VIEWPORT_ELEVATE to whatever this system uses." >&2
	exit 1
fi

# Root's podman keeps its own image store, so an image built rootless is not
# there. Copied across once rather than rebuilt, which would download every
# package a second time; skipped when it is already present.
if ! "${elevate[@]}" podman image exists "$runtime" 2>/dev/null; then
	echo "==> copying $runtime into root's image store (once; it is not small)"
	podman image save -o "$work/runtime.tar" "$runtime"
	"${elevate[@]}" podman load -i "$work/runtime.tar"
	rm -f "$work/runtime.tar"
fi

# Started before the compositor and waited for: libseat gives up immediately if
# the socket is not there yet, and "started seatd" is not the same as "seatd is
# listening".
start_seatd='
	seatd >/tmp/seatd.log 2>&1 &
	for _ in $(seq 50); do
		[ -S /run/seatd.sock ] && break
		sleep 0.1
	done
	if [ ! -S /run/seatd.sock ]; then
		echo "seatd did not start:" >&2
		cat /tmp/seatd.log >&2
		exit 1
	fi
	mkdir -p /tmp/xdg && chmod 700 /tmp/xdg
'

if [ "$mode" = shell ]; then
	echo "==> root shell in the container; seatd is running, so 'viewport' works"
	exec "${elevate[@]}" podman run "${tty_args[@]}" "$runtime" \
		bash -c "$start_seatd exec bash"
fi

# The compositor currently corrupts its heap during teardown and can die of it
# while still holding DRM master. In a normal session that is survivable — the
# process is exiting anyway and logind restores the console. Here there is no
# logind, so a death at the wrong moment leaves the display dead and VT
# switching gone, and the only way out is the power button. It has happened.
if [ -z "${VIEWPORT_I_ACCEPT_THE_LOCKUP_RISK:-}" ]; then
	cat >&2 <<'WARN'
This can lock up the machine.

The compositor has a teardown bug that can crash it while it still holds the
display, and nothing in this container will hand the console back if it does.
That means a hard power off, losing whatever is unsaved elsewhere.

--nested is safe and tests the same package. If you need the real display
anyway, set VIEWPORT_I_ACCEPT_THE_LOCKUP_RISK=1.
WARN
	exit 1
fi

echo "==> starting viewport on this console (Mod4+Shift+e to quit)"

# The container inherits the stdout already being teed, so its output lands in
# the same log as the build steps above it — one file describing the whole run.
"${elevate[@]}" podman run "${tty_args[@]}" "$runtime" \
	bash -c "$start_seatd exec viewport --debug --startup foot" \
	|| echo "==> viewport exited $?"

echo
echo "==> log saved: $logfile"
echo "    a report to hand over:  ./scripts/collect-report.sh"
