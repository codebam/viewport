#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Refresh data/shell/vendor from node_modules, or check the committed copy.
#
# The shell is a file:// page with no bundler and no network: every <script>
# it names has to exist beside it on disk, and the Nix build and the Arch
# PKGBUILDs all install data/shell with a plain `cp -r`. So the dependency is
# declared in package.json — that is where the version lives and where npm
# audits it — and the built file is committed, because neither the build
# sandbox nor a running desktop can fetch it.
#
# Run after changing a version in package.json:
#
#   npm install && npm run vendor
#
# and commit what changes under data/shell/vendor. That writes both
# gsap.min.js and the gsap.min.js.sha256 file CI verifies, so the two cannot
# drift apart unnoticed.
#
# Verify the committed pair without node_modules:
#
#   scripts/vendor-shell-deps.sh --check
set -euo pipefail

cd "$(dirname "$0")/.."

hash_file=data/shell/vendor/gsap.min.js.sha256

check() {
  sha256sum -c "$hash_file"
}

case "${1:-}" in
  --check)
    check
    exit 0
    ;;
  -h|--help)
    sed -n '3,23p' "$0" | sed 's/^# \?//'
    exit 0
    ;;
  "")
    ;;
  *)
    echo "usage: $0 [--check]" >&2
    exit 2
    ;;
esac

if [ ! -d node_modules/gsap ]; then
  echo "node_modules/gsap is missing — run 'npm install' first" >&2
  exit 1
fi

mkdir -p data/shell/vendor

version=$(node -p 'require("./node_modules/gsap/package.json").version')
# 0644 explicitly: the vendored file is data, not an executable, and a source
# checkout with a laxer umask must not be able to change that in the commit.
install -m 0644 node_modules/gsap/dist/gsap.min.js data/shell/vendor/gsap.min.js
sha256sum data/shell/vendor/gsap.min.js > "$hash_file"
chmod 0644 "$hash_file"

echo "vendored gsap ${version} -> data/shell/vendor/gsap.min.js"
echo "wrote $hash_file (CI verifies this)"
