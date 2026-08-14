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
are free. Everything a push needs is here — the v0.1.5 release carries one
artifact per engine, the three `-bin` recipes carry those artifacts' real
checksums, and every package has a `.SRCINFO` beside its PKGBUILD. Pushing is
copying a directory into its AUR repository and committing it.

`.SRCINFO` is generated, never edited, and goes stale the moment a PKGBUILD
changes — regenerate it in the same commit:

```sh
podman run --rm -v "$PWD/packaging/aur:/aur:z" localhost/viewport-builder \
  bash -lc 'for d in /aur/viewport-*/; do (cd "$d" && makepkg --printsrcinfo > .SRCINFO); done'
```

### The `-git` recipes' version

`pkgver=` in a `-git` recipe is a marker, not a claim. makepkg runs `pkgver()`
after fetching and rewrites that line with what it returns, so what installs is
always what `main` says at build time — the number in the file is whatever the
last person to build it saw, and is one commit behind by construction. Every
VCS package in the AUR carries the same. Bring it forward when you touch one:

```sh
git describe --long --tags --abbrev=7 | sed 's/^v//;s/\([^-]*-g\)/r\1/;s/-/./g'
```

then regenerate the `.SRCINFO`, since that is what the AUR displays.

### What has been built, and what has not

| recipe | built | how |
| --- | --- | --- |
| `viewport-{webkitgtk,wpe,chromium}` | yes | they built the v0.1.5 artifacts |
| `viewport-{webkitgtk,wpe,chromium}-bin` | yes | fetched the published artifact and checked its sum |
| `viewport-chromium-git` | yes | `0.1.5.r1.gf5fe7d2`, so `pkgver()` and the branch fetch work |
| `viewport-{webkitgtk,wpe}-git` | no | identical to `viewport-chromium-git` apart from the engine, which their source twins prove |

`namcap` is clean on the three chromium recipes and on the packages they
produce, apart from warnings it cannot avoid: the wrapper is a `/bin/sh`
script, and the libraries this compositor opens at runtime rather than linking
(Vulkan, EGL, pipewire) read to it as dependencies that "may not be needed".

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
