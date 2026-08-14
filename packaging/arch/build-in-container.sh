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
# Three of the four backends are packaged here. `cef` is not, and cannot be
# from the repositories: CEF is a prebuilt binary bundle, the only Arch package
# of it is `cef-minimal` in the AUR, and that is CEF 121 against the 149 this
# tree is written for — a mismatch the loader reports as
# "CefApp_0_CToCpp called with invalid version -1" rather than as a version
# mismatch. `chromium` gives Arch a Blink shell with nothing outside the
# repositories, and the flake still builds cef with the right version pinned.
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

image=localhost/viewport-builder

echo "building the image with $engine..." >&2
"$engine" build -t "$image" -f "$here/Containerfile" "$here"

# The PKGBUILD fetches its own source from git, so only the recipe goes in.
echo "running makepkg..." >&2
"$engine" run --rm \
    -v "$here/$variant/PKGBUILD:/build/PKGBUILD:ro" \
    -v "$dest:/out:z" \
    "$image" \
    bash -lc '
        set -euo pipefail
        cp /build/PKGBUILD /pkg/PKGBUILD
        cd /pkg

        # The dependencies, installed here rather than by `makepkg --syncdeps`,
        # for two reasons that both end in "could not resolve all
        # dependencies" and neither of which names what was missing.
        #
        # The image patches out makepkg`s refusal to run as root, so makepkg
        # believes it is unprivileged and reaches for sudo — which in a
        # container without CAP_AUDIT_WRITE dies with "error initializing audit
        # plugin sudoers_audit" before pacman is ever reached. Running pacman
        # directly as the root we already are skips the whole question.
        #
        # And anything the repositories do not have is named and skipped
        # rather than being allowed to fail the whole build with "could not
        # resolve all dependencies" and no list of what was missing — which is
        # how three wrong package names in these recipes went unnoticed
        # (`webkit2gtk-6.0` for `webkitgtk-6.0`, `at-spi2-atk` for
        # `at-spi2-core`, and a `cargo` that Arch ships inside `rust`).
        #
        # shellcheck disable=SC1091
        . ./PKGBUILD
        want=("${depends[@]:-}" "${makedepends[@]:-}")
        have=(); missing=()
        for spec in "${want[@]}"; do
            [ -n "$spec" ] || continue
            # Version constraints are for pacman to enforce on a real install,
            # and `pacman -Si foo>=1.2` is not a query pacman answers.
            name=${spec%%[<>=]*}
            if pacman -Si "$name" >/dev/null 2>&1; then
                have+=("$name")
            else
                missing+=("$name")
            fi
        done
        [ ${#missing[@]} -eq 0 ] || \
            echo "not in any repository, skipped: ${missing[*]}" >&2
        # -Syu, not -S. The image is built once and cached, so its package
        # database is as old as the last time the image was rebuilt while the
        # mirrors have moved on — and installing against a stale database asks
        # for versions that are no longer there:
        #
        #   error: failed retrieving file cmake-4.4.1-1-x86_64.pkg.tar.zst: 404
        #
        # Arch does not support partial upgrades and this is the shape that
        # failure takes. The container is thrown away after one package, so
        # upgrading everything in it costs nothing worth keeping.
        pacman -Syu --needed --noconfirm --disable-sandbox "${have[@]}"

        # --nodeps: they are installed above, and the ones that are not cannot
        # be. Everything else about the build is unchanged.
        makepkg --nodeps --noconfirm --clean
        cp -v ./*.pkg.tar.* /out/
    '

echo >&2
echo "package written to $dest:" >&2
ls -la "$dest"/*.pkg.tar.* >&2
