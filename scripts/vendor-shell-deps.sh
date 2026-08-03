#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Refresh data/shell/vendor from node_modules.
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
# and commit what changes under data/shell/vendor.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ ! -d node_modules/gsap ]; then
  echo "node_modules/gsap is missing — run 'npm install' first" >&2
  exit 1
fi

mkdir -p data/shell/vendor

version=$(node -p 'require("./node_modules/gsap/package.json").version')
cp node_modules/gsap/dist/gsap.min.js data/shell/vendor/gsap.min.js

echo "vendored gsap ${version} -> data/shell/vendor/gsap.min.js"
