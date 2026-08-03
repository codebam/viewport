# The AUR packages

Two of them, both for the WebKitGTK shell:

| AUR package | what it is | recipe |
| --- | --- | --- |
| `viewport-smithay-webkitgtk` | builds the tagged tree from source | `packaging/arch/webkitgtk/PKGBUILD`, pushed verbatim |
| `viewport-smithay-webkitgtk-bin` | unpacks the release artifact | `packaging/aur/viewport-smithay-webkitgtk-bin/PKGBUILD` |

The source package has no copy of its own here on purpose: the recipe in
`packaging/arch/webkitgtk` is the one that is pushed, so there is one file to
change rather than two that drift apart. The other two engines (`wpe`,
`chromium`) are built and published on the GitHub release, but are not in the
AUR.

## Cutting a release

1. Tag the tree: `git tag -a vX.Y.Z && git push rewrite vX.Y.Z`.
2. Point the three recipes in `packaging/arch` at it (`_tag=vX.Y.Z`).
3. Build all three: `./packaging/arch/build-in-container.sh <variant>`.
4. Upload the three `.pkg.tar.zst` to the GitHub release.
5. Update both AUR packages — `pkgver`, and for `-bin` the
   `sha256sums_x86_64` of the uploaded artifact.

## Pushing to the AUR

`.SRCINFO` is generated, never written by hand, and has to be regenerated
whenever the PKGBUILD changes — the AUR reads the package's metadata from it
and rejects a push whose `pkgname` does not match the repository:

```sh
makepkg --printsrcinfo > .SRCINFO   # on Arch, or in the container image
git clone ssh://aur@aur.archlinux.org/viewport-smithay-webkitgtk.git
```
