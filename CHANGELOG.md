# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are tagged `vX.Y.Z`. The list below starts at the release the tree is
currently cut from; earlier history lives in `git log`, which this file exists
to summarise rather than to duplicate.

## [Unreleased]

### Protocols
- Wire up the color management, HDR, output management and tearing-control
  protocol surfaces the shell and clients use — `color-management-v1`,
  `hdr-output-metadata-v1`, `wlr-output-management` and the smithay fork's
  tearing-control patch — see `docs/protocols.md`.
- Publish the workspaces to outside clients, so an external bar has something
  to draw.

### Tests
- Keep the Wayland integration tests (`scripts/integration.sh`) driving the
  Rust binary rather than the deleted C compositor — capture, session lock,
  output order and the shell layout variants, all headless on a real socket.
- Run the same Rust suite under AddressSanitizer (`.#asan`).

### Shipping
- Move the renderer out to its own repository.
- Point the AUR `-bin` package at the `0.1.3` artifact.
- Add a source package recipe under `packaging/aur` and a `wpe` binary
  variant, so the AUR carries more than a single WebKitGTK `-bin` package.

### Docs
- Add this changelog, a `CONTRIBUTING.md`, and a `viewport(1)` man page
  documenting the binary's command-line flags.

## [0.1.3] - 2026-08-05

The first release cut from the Rust rewrite after it reached parity with the
deleted C compositor and the tree stopped carrying two implementations.

### Added
- A `viewport` binary that nests inside the session it was started from, or
  takes the DRM session from a TTY, and a packaged compositor for Arch
  (`packaging/arch/`) and NixOS (`flake.nix`).
- A `viewport` subcommand for the control socket, so a running session is
  drivable from a terminal without anything else installed.
- The WPE, WebKitGTK and Chromium shell backends, selectable at build time
  with `--features wpe` or at runtime with `--shell-backend`.

### Fixed
- A `--exit-after` deadline that silently did nothing on an idle compositor,
  by arming a timerfd the event loop actually wakes for (see `main.rs`).
- Shell loading on a plain `file://` page when the shared MIME database is
  missing — WebKit treated such pages as empty documents (see the `wpe`
  PKGBUILD's notes on `shared-mime-info`).

[Unreleased]: https://github.com/codebam/viewport-smithay/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/codebam/viewport-smithay/releases/tag/v0.1.3
