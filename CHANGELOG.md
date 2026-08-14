# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are tagged `vX.Y.Z`. The list below starts at the release the tree is
currently cut from; earlier history lives in `git log`, which this file exists
to summarise rather than to duplicate.

## [Unreleased]

### Added
- A colour or a gradient as the wallpaper: `wallpaper` takes a CSS value —
  `#1a1b26`, `rgb(...)`, `linear-gradient(...)`, `url(...)` — as well as a
  path, so a colour scheme with no photograph in it does not need one.
- A picture for the desktop background, set three ways: `wallpaper` and
  `wallpaper_mode` in the config file, `--wallpaper` and `--wallpaper-mode` on
  the command line, and `config.wallpaper` on the control socket for changing
  it without a reload. The five fittings — `fill`, `fit`, `stretch`, `center`
  and `tile` — are stylix's `imageScalingMode` spelled the same way, so a
  themed NixOS session hands its settings straight across; see
  `docs/configuration.md`.

### Protocols
- `zwlr_foreign_toplevel_management_v1`: activate, close and fullscreen for
  taskbars (`crates/viewport/src/foreign_toplevel.rs` — see also
  `docs/RUST-REWRITE.md`); maximise and minimise are accepted and not acted
  on, because the shell owns the layout.
- `kde-server-decoration` via Smithay's `KdeDecorationState` — a Qt or KDE
  client that speaks only the KDE verb no longer draws doubled decorations,
  and the manager's default and the per-surface answers both derive from the
  `"decorations"` config key.
- `xdg-toplevel-drag-v1`: not advertised. The tab-tearing global is withdrawn
  rather than carried as an advertised-but-inert object that makes browsers
  take a path the compositor never delivers; see `docs/protocols.md`.
- `wlr-export-dmabuf-v1` and `ext-transient-seat-v1`: not advertised.
  Zero-copy capture has `ext-image-copy-capture` and `wlr-screencopy`, and a
  second seat has no virtual-input back end. An always-refused global leaves a
  client that probes and does not fall back worse off than absence would; see
  `docs/protocols.md`.
- Wire up the color management, HDR, output management and tearing-control
  protocol surfaces the shell and clients use — `color-management-v1`,
  `hdr-output-metadata-v1`, `wlr-output-management` and the smithay fork's
  tearing-control patch — see `docs/protocols.md`.
- Publish the workspaces to outside clients, so an external bar has something
  to draw.

### Tests
- Drive `zwlr_foreign_toplevel_management_v1`, `wlr-output-management-v1` and
  `ext-workspace-v1` over the wire, headless and on a real socket alongside
  the existing capture/paint/output-order/screencast/session-lock tests
  (`scripts/integration.sh`, new `tests/foreign-toplevel*`,
  `tests/output-management*`, `tests/workspace*`). They `wayland-scanner` the
  generated marshalling code and check behaviour, not just presence.
- Keep the Wayland integration tests (`scripts/integration.sh`) driving the
  Rust binary rather than the deleted C compositor — capture, session lock,
  output order and the shell layout variants, all headless on a real socket.
- Run the same Rust suite under AddressSanitizer (`.#asan`).

### Shipping
- Move the renderer out to its own repository.
- Point the AUR `-bin` package at the `0.1.3` artifact.
- Ship `viewport-smithay-wpe-bin` alongside `viewport-smithay-webkitgtk-bin`;
  keep a single `viewport-smithay` source recipe at
  `packaging/arch/webkitgtk/PKGBUILD` (see `packaging/aur/README.md`) rather
  than carrying a second copy under `packaging/aur/viewport-smithay/src/`.
  The WPE `-bin` remains not yet pushed until the artifact's real
  `sha256sums_x86_64` is filled.

### Docs
- Add this changelog, a `CONTRIBUTING.md`, and a `viewport(1)` man page
  documenting the binary's command-line flags. The man page is now installed
  as `usr/share/man/man1/viewport.1` from the Arch recipes and from the nix
  package's `postInstall`, and a flag change must be reflected there.

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
- **`servoshell` is the default backend** — `nix build`, `nix run` and
  `programs.viewport.shellBackend` all land on it. It is the lightest desktop
  measured (8.5% of a core under load against 9.9 to 11.5, 357 MB against 449
  to 639, four processes against nine to twelve) and the slowest to paint by
  some way (14 frames a second against 43 to 48). `cef`, the previous default,
  is still the answer for a desktop that should feel quick; see
  `docs/benchmarks.md`.
- Two Servo shell backends, `servo` and `servoshell` — the engine embedded and
  the engine driven, as `cef` and `chromium` are for Blink. `servoshell` runs
  nixpkgs' prebuilt browser and compiles no engine (`nix build .#servoshell`);
  `servo` embeds the `servo` crate from a workspace of its own, so the engine
  build it costs cannot be reached by `cargo test --workspace`, by CI or by a
  compositor rebuild. Neither needs an edit to `data/shell/*.js`: the bridge is
  a user script both ends. See `docs/shell-backends.md`.

### Fixed
- A `--exit-after` deadline that silently did nothing on an idle compositor,
  by arming a timerfd the event loop actually wakes for (see `main.rs`).
- Shell loading on a plain `file://` page when the shared MIME database is
  missing — WebKit treated such pages as empty documents (see the `wpe`
  PKGBUILD's notes on `shared-mime-info`).

[Unreleased]: https://github.com/codebam/viewport-smithay/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/codebam/viewport-smithay/releases/tag/v0.1.3
