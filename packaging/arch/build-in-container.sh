#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Build the Arch package in a container, because makepkg needs pacman and an
# Arch userland and this is developed on NixOS.
#
#   ./packaging/arch/build-in-container.sh wpe             # into ./out/wpe
#   ./packaging/arch/build-in-container.sh chromium ~/     # into ~/
#
# The variant is a directory beside this script, one per engine the shell can
# be drawn by. The finished .pkg.tar.zst is copied to the directory given
# (default ./out/<variant>) and installs anywhere with `pacman -U`.
#
# `cef` and `full` need a CEF distribution: pass CEF_PATH and it is mounted
# into the container and exported for makepkg. See packaging/arch/cef/PKGBUILD.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

variant=${1:-wpe}
[ -f "$here/$variant/PKGBUILD" ] || {
    echo "no such variant: $variant" >&2
    echo "one of: $(cd "$here" && ls -d */ | tr -d / | tr '\n' ' ')" >&2
    exit 2
}
out=${2:-$here/out/$variant}
mkdir -p "$out"
dest=$(cd "$out" && pwd)

engine=${CONTAINER_ENGINE:-}
if [ -z "$engine" ]; then
    for candidate in podman docker; do
        command -v "$candidate" >/dev/null 2>&1 && engine=$candidate && break
    done
fi
[ -n "$engine" ] || { echo "no podman or docker on PATH" >&2; exit 1; }

image=localhost/viewport-smithay-builder

echo "building the image with $engine..." >&2
"$engine" build -t "$image" -f "$here/Containerfile" "$here"

# The PKGBUILD fetches its own source from git, so only the recipe goes in.
echo "running makepkg..." >&2
"$engine" run --rm \
    -v "$here/$variant/PKGBUILD:/build/PKGBUILD:ro" \
    -v "$dest:/out:z" \
    ${CEF_PATH:+-v "$CEF_PATH:/cef:ro" -e CEF_PATH=/cef} \
    "$image" \
    bash -lc '
        set -euo pipefail
        cp /build/PKGBUILD /pkg/PKGBUILD
        cd /pkg
        makepkg --syncdeps --noconfirm --clean
        cp -v ./*.pkg.tar.* /out/
    '

echo >&2
echo "package written to $dest:" >&2
ls -la "$dest"/*.pkg.tar.* >&2
