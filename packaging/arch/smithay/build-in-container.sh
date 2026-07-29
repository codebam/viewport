#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Build the Arch package in a container, because makepkg needs pacman and an
# Arch userland and this is developed on NixOS.
#
#   ./packaging/arch/smithay/build-in-container.sh            # into ./out
#   ./packaging/arch/smithay/build-in-container.sh ~/         # into ~/
#
# The finished .pkg.tar.zst is copied to the directory given (default ./out)
# and can be installed anywhere with `pacman -U`.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
dest=$(cd "${1:-$here/out}" 2>/dev/null && pwd || { mkdir -p "${1:-$here/out}" && cd "${1:-$here/out}" && pwd; })

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
    -v "$here/PKGBUILD:/build/PKGBUILD:ro" \
    -v "$dest:/out:z" \
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
