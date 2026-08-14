#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Build the Arch package in a container, because makepkg needs pacman and an
# Arch userland and this is developed on NixOS.
#
#   ./packaging/build-in-container.sh wpe                  # into ./out/wpe
#   ./packaging/build-in-container.sh chromium ~/          # into ~/
#   ./packaging/build-in-container.sh viewport-wpe-git     # by package name
#
# The argument names a recipe under `packaging/aur`, either by its package name
# or by the engine alone — `wpe` is `viewport-wpe`, the source recipe for that
# engine. The `-git` and `-bin` forms build here too, which is how a recipe is
# checked before it is pushed anywhere. The finished .pkg.tar.zst is copied to
# the directory given (default ./out/<package>) and installs anywhere with
# `pacman -U`.
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

# A package name, or an engine that stands for its source recipe.
asked=${1:-wpe}
case $asked in
    viewport-*) package=$asked ;;
    *)          package=viewport-$asked ;;
esac
recipe=$here/aur/$package/PKGBUILD
[ -f "$recipe" ] || {
    echo "no such recipe: $asked" >&2
    echo "one of: $(cd "$here/aur" && ls -d viewport-*/ | tr -d / | tr '\n' ' ')" >&2
    exit 2
}
out=${2:-$here/out/$package}
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
    -v "$recipe:/build/PKGBUILD:ro" \
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
        # makepkg.conf first, because a recipe may read what makepkg would have
        # set for it: `$CARCH` in a `source_x86_64` URL is unbound in a plain
        # shell, and under `set -u` that is the whole build failing on line 87
        # of a PKGBUILD that is perfectly correct.
        #
        # shellcheck disable=SC1091
        . /etc/makepkg.conf
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
