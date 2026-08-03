# Vendored shell dependencies

Built files, checked in rather than fetched. The shell is a `file://` page: it
has no bundler, no module resolution and no network, and both the Nix
derivation and the Arch PKGBUILDs install this directory with `cp -r`. A
dependency that is not on disk here is a dependency the desktop cannot load.

The versions are declared in `package.json` at the repository root. To change
one:

    npm install && npm run vendor

and commit the result.

| File | Package | Version | License |
| --- | --- | --- | --- |
| `gsap.min.js` | [gsap](https://www.npmjs.com/package/gsap) | 3.15.0 | GreenSock Standard "no charge" — <https://gsap.com/standard-license> |

**The GSAP licence is not the MIT licence the rest of this repository is
under.** It permits use and redistribution at no charge, including in
commercial and open-source work, and since 2024 that covers every plugin — but
it is a bespoke licence with its own terms, and a distribution package built
from this tree ships a file that is not MIT. Anyone repackaging Viewport
should read it rather than assume the top-level `LICENSE` covers the whole
tree.
