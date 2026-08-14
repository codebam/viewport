# The AUR packages

Nine of them: three engines, each in three forms.

| AUR package | what it is | recipe |
| --- | --- | --- |
| `viewport-webkitgtk` | builds the tagged tree from source | `packaging/aur/viewport-webkitgtk/PKGBUILD` |
| `viewport-wpe` | the same, with the engine inside the compositor | `packaging/aur/viewport-wpe/PKGBUILD` |
| `viewport-chromium` | the same, driving a browser it does not link | `packaging/aur/viewport-chromium/PKGBUILD` |
| `viewport-webkitgtk-bin` | unpacks the release artifact | `packaging/aur/viewport-webkitgtk-bin/PKGBUILD` |
| `viewport-wpe-bin` | the same for WPE | `packaging/aur/viewport-wpe-bin/PKGBUILD` |
| `viewport-chromium-bin` | the same for Chromium | `packaging/aur/viewport-chromium-bin/PKGBUILD` |
| `viewport-webkitgtk-git` | follows `main` | `packaging/aur/viewport-webkitgtk-git/PKGBUILD` |
| `viewport-wpe-git` | the same for WPE | `packaging/aur/viewport-wpe-git/PKGBUILD` |
| `viewport-chromium-git` | the same for Chromium | `packaging/aur/viewport-chromium-git/PKGBUILD` |

Every one of them installs a binary called `viewport` and provides that name,
and every one conflicts with the rest: a machine takes one engine in one form.

One directory per AUR repository, named exactly after it, so a push is a copy
of a directory rather than a rule about which file goes where. A `-git` recipe
is its source recipe with three differences — the name, a `pkgver()`, and a
branch instead of a tag — so a change to a build step belongs in the source
recipe and then in its twin.

`cef` has no recipe at all: CEF is a prebuilt bundle, Arch's only package of it
is `cef-minimal` at CEF 121 against the 149 this tree needs, and `chromium`
gives Arch the same engine out of the repositories. `servoshell` has none
either — nixpkgs' `servo` has no Arch counterpart.

## What is not pushed yet

Nothing is: none of the nine repositories exist on the AUR, and all nine names
are free. The artifacts they need do exist now — the v0.1.5 release carries one
per engine and the three `-bin` recipes carry those artifacts' real checksums —
so what is left before a push is a `.SRCINFO` per package, which is generated
rather than written; see below.

## Cutting a release

1. Tag the tree: `git tag -a vX.Y.Z && git push origin vX.Y.Z`.
2. Point the three source recipes in `packaging/aur` at it: `_tag=vX.Y.Z` in the
   source line, and `pkgver=X.Y.Z`. Between releases those recipes sit on a
   plain commit instead — `_commit=` the commit, `#commit=` in the source line,
   and `pkgver=X.Y.Z.rN.gSHORT`, the last release, how many commits past it,
   and which one — which is what a snapshot built for someone to try should say
   on sight. The version bump itself goes in the same commit the tag names, so
   the recipes in the tagged tree already point at their own tag.
3. Build all three: `./packaging/build-in-container.sh <package>`.
4. Upload the three `.pkg.tar.zst` to the GitHub release.
5. Update the AUR packages — `pkgver`, and for the `-bin` packages the
   `sha256sums_x86_64` of the uploaded artifact.

## Pushing to the AUR

`.SRCINFO` is generated, never written by hand, and has to be regenerated
whenever the PKGBUILD changes — the AUR reads the package's metadata from it
and rejects a push whose `pkgname` does not match the repository:

```sh
makepkg --printsrcinfo > .SRCINFO   # on Arch, or in the container image
git clone ssh://aur@aur.archlinux.org/viewport-webkitgtk.git
```
