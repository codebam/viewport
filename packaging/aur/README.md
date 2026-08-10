# The AUR packages

Three of them. Each engine's shell can be installed either from source or as
the prebuilt release artifact; the `-bin` packages are the artifact, and the
source package is the tag compiled:

| AUR package | what it is | recipe |
| --- | --- | --- |
| `viewport-smithay` | builds the tagged tree from source (WebKitGTK shell) | `packaging/aur/viewport-smithay/src/PKGBUILD` |
| `viewport-smithay-webkitgtk-bin` | unpacks the release artifact | `packaging/aur/viewport-smithay-webkitgtk-bin/PKGBUILD` |
| `viewport-smithay-wpe-bin` | unpacks the WPE release artifact | `packaging/aur/viewport-smithay-wpe-bin/PKGBUILD` |

The `wpe` binary exists because that engine is the tallest build of the four —
WebKit inside the compositor process — the one the `-bin` form saves a machine
from. `chromium` is built and published on the GitHub release but not in the
AUR, because it links no engine and building from source there buys an Arch
user nothing over `chromium` from the repositories.

## Cutting a release

1. Tag the tree: `git tag -a vX.Y.Z && git push rewrite vX.Y.Z`.
2. Point the three recipes in `packaging/arch` at it (`_tag=vX.Y.Z`).
3. Build all three: `./packaging/arch/build-in-container.sh <variant>`.
4. Upload the three `.pkg.tar.zst` to the GitHub release.
5. Update the AUR packages — `pkgver`, and for the `-bin` packages the
   `sha256sums_x86_64` of the uploaded artifact. The source package's
   `sha256sums` is the sha256 of the tagged tarball (one run of
   `sha256sum tarball`).

## Pushing to the AUR

`.SRCINFO` is generated, never written by hand, and has to be regenerated
whenever the PKGBUILD changes — the AUR reads the package's metadata from it
and rejects a push whose `pkgname` does not match the repository:

```sh
makepkg --printsrcinfo > .SRCINFO   # on Arch, or in the container image
git clone ssh://aur@aur.archlinux.org/viewport-smithay.git
```
