#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The version is written in six places, because four of them cannot inherit it
# from the fifth and the sixth is not a Rust file at all: the root manifest;
# the two crates that sit outside the workspace (`viewport-shell-cef` and
# `viewport-shell-servo`, excluded so their engine-only dependency trees can
# carry lock files of their own); the two derivations in flake.nix, one per
# excluded crate; and package.json. Bump one and miss another and the mismatch
# ships: a package reporting a version no tag was cut from.
#
#   scripts/check-versions.sh
#
# Exits 0 when all six agree, 1 naming every place that disagrees.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

expected="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
if [ -z "$expected" ]; then
  echo "check-versions: no version found in Cargo.toml" >&2
  exit 1
fi

status=0
check_one() {
  local name="$1" found="$2"
  if [ "$found" != "$expected" ]; then
    echo "check-versions: $name is ${found:-unset}, Cargo.toml is $expected" >&2
    status=1
  fi
}

manifest_version() {
  sed -n 's/^version = "\(.*\)"$/\1/p' "$1" | head -1
}

check_one "crates/viewport-shell-cef/Cargo.toml" \
  "$(manifest_version crates/viewport-shell-cef/Cargo.toml)"
check_one "crates/viewport-shell-servo/Cargo.toml" \
  "$(manifest_version crates/viewport-shell-servo/Cargo.toml)"

# The two derivations spell their version identically, so both lines are
# taken — each via the `pname` above it, which also skips the WPE WebKit
# overlay's own version — and both are checked: checking the first twice
# would let the second drift.
mapfile -t flake_versions < <(sed -n '/^          pname = "viewport/{n;s/^          version = "\(.*\)";$/\1/p;}' flake.nix)
if [ "${#flake_versions[@]}" -ne 2 ]; then
  echo "check-versions: expected 2 versions in flake.nix, found ${#flake_versions[@]}" >&2
  status=1
else
  for version in "${flake_versions[@]}"; do
    check_one "flake.nix" "$version"
  done
fi

check_one "package.json" \
  "$(sed -n 's/^  "version": "\(.*\)",$/\1/p' package.json | head -1)"

exit "$status"
