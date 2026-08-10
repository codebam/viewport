# Contributing

Thanks for wanting to work on Viewport. This page is what a change has to get
past — the build, the tests, the hook that stands in for the CI that does not
run, and the conventions the tree is written under.

## Building and testing

Work inside `nix develop .#rust`, which is the toolchain and the dependencies
the tests need and deliberately nothing else — no WPE WebKit, because the web
engine is behind a non-default feature and the tests do not link it:

```sh
nix develop .#rust
cargo test --workspace        # the unit and control-socket tests
scripts/integration.sh target/debug/viewport   # the Wayland integration tests
```

`scripts/integration.sh` takes the path to a compositor binary and does not
care which language wrote it: it starts it headless, drives it with real
clients over a real socket, and checks what comes back. `cargo build --bin
viewport` first, then hand it the result. `nix develop` on its own is the
fuller workstation shell; `nix develop .#wpe` is the one that also builds WPE
WebKit, which is hours, so it is behind a shell of its own:

```sh
nix build .#wpewebkit   # do this once, deliberately, before anything else
```

The flake's packaged compositor is `nix build .#webkitgtk` (the default), and
the other engines are `.#wpe`, `.#chromium` and `.#cef`.

The shell itself — the JavaScript tree under `data/shell/` — is tested without
a compositor at all, against a stubbed DOM:

```sh
node tests/shell.test.js data/shell tiling
node tests/shell.test.js data/shell scrolling
node tests/shell.test.js data/shell solar
node tests/shell.test.js data/shell matrix
node tests/kiosk.test.js examples/kiosk
```

Each layout model also has a `session` variant (`node tests/shell.test.js
data/shell tiling session`), which is where the three are most likely to
disagree — restoring a saved layout is the one thing that writes the tree
rather than reading it.

## The checks before a commit

CI runs three jobs on every push — `shell`, `rust`, `asan` — as described in
`.github/workflows/ci.yml`. The same checks also live in a hook, which has to
be turned on once per clone, so there is a local pre-check before the push
triggers anything remote:

```sh
git config core.hooksPath .githooks
```

After that, `git commit` runs against the staged changes only. It runs:

- the shell layout tests, if anything under `data/shell`, `examples/kiosk`,
  `tests/*.js` or `tests/*.test.js` is staged;
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
  -D warnings` and `cargo test --workspace`, if anything under `crates/`,
  `Cargo.toml`, `Cargo.lock` or `flake.nix` is staged;
- a `cargo check` of *both* halves of the `wpe` feature — with and without —
  because a `#[cfg(feature = "wpe")]` on the wrong item compiles cleanly in
  whichever configuration you happen to test and breaks the other. The `wpe`
  half runs in the `scripts/container-full` image (podman or docker); on a
  machine with neither it falls back to the nix dev shell, which builds WebKit
  from source if the store lacks it.

Staging nothing it can break — a PKGBUILD, a document — runs nothing.
`git commit --no-verify` skips the hook. It builds into `target/pre-commit`
rather than `target`, so it cannot silently replace a
`target/release/viewport` built with `--features wpe` with one that was not.

## Code conventions

- **No warnings where warnings can fail the build.** Clippy runs with
  `-D warnings`. The handful of lints that are deliberate carry an
  `#[allow(...)]` with a comment saying why, which is the form an exception
  should take.
- **Comment the *why*, not the *what*.** The tree's comments are long and
  reason about the failure mode being guarded against — search `main.rs` for
  "which is a spectacular way to find out" for the spirit of it. A fix that
  does not say why a previous approach was wrong invites a future patch to
  reintroduce it.
- **A flag works or is warned about.** Every option the compositor accepts is
  in one table (`OPTIONS` in `crates/viewport/src/main.rs`) that both feeds
  `--help` and is checked against for unknown options, so the two cannot
  drift. Add a new flag in both places by adding it to the table.
- **SPDX headers.** Source files carry `// SPDX-License-Identifier:
  GPL-3.0-or-later` (the crates that are deliberately MIT say so in
  `Cargo.toml` and the README's Licence section). See `README.md` for why the
  split exists.
- **Licence, seriously.** Nothing in `crates/viewport` may be adapted back
  into Smithay or wlroots, which are MIT — see `docs/RUST-REWRITE.md`.

## Documentation

The reference material lives in `docs/` (see the table in `README.md`), and
the shell's own workings in `data/shell/shell.md`. A change to a flag or a
config key should be reflected in `docs/configuration.md` and the `viewport(1)`
man page (`docs/viewport.1`); a change to a message should be reflected in
`docs/ipc.md`. The changelog lives at the root (`CHANGELOG.md`, Keep a
Changelog style).

## Releasing / packaging

See `packaging/aur/README.md` for the AUR release procedure. The short form:
tag the tree, point the recipes in `packaging/arch/` at the tag, build them
with `./packaging/arch/build-in-container.sh <variant>` (which also works on a
machine that is not Arch), upload the artifacts to the GitHub release, and
regenerate each AUR package's `.SRCINFO` with `makepkg --printsrcinfo`.

## Reporting a bug

The compositor logs to stderr under the `VIEWPORT_LOG` filter (`VIEWPORT_LOG=
debug` for everything). If a session comes up wrong, a screenshot can be taken
from inside it; `docs/debugging.md` covers that and the rest of what to gather
before filing.
