# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are tagged `vX.Y.Z`. The list below starts at the release the tree is
currently cut from; earlier history lives in `git log`, which this file exists
to summarise rather than to duplicate.

## [Unreleased]

### Changed
- `Mod4` + right-drag resizes from the corner nearest where the drag started,
  rather than always pulling the bottom right one. The whole window is the
  handle — there is no border to aim at — so the quarter the press lands in
  names the corner, which is how sway reads the same gesture. Taking hold of a
  window on its left and pulling left made it *smaller* before this, because
  the only edges that ever moved were the far ones. The compositor sends the
  corner along with the delta; the shell decides what it means, which differs
  per layout: a tiled window trades with the sibling that edge faces, a
  floating one and a window on the canvas pin the opposite corner and move as
  they size. An edge with no neighbour to trade with — the leftmost window's
  left edge — still takes from the other side rather than doing nothing.

### Fixed
- The bar showing the old volume after a scroll. The widget spawned `wpctl`
  through `shell.exec` and asked for a re-sample in the next message, which
  samples the sink before the spawned process has run — so the bar redrew the
  number that was already there and the new one waited for the next two-second
  tick. `status.volume` changes and samples in that order.
- The compositor losing `org.freedesktop.Notifications` for the rest of the
  session once anything else took it. zbus asks for a name with `DoNotQueue`
  by default, which also decides what happens *after* being replaced — a
  replaced owner is dropped rather than queued — so when the program that took
  it exited, the name was owned by nobody and every notification failed with
  `ServiceUnknown` while the compositor sat there serving the interface. Both
  names it claims are queued for now, and neither is taken from whoever holds
  it: `ReplaceExisting` was in zbus's default too, and it is what let a nested
  compositor take the notification daemon and the portal backend from the
  session it was started inside — the opposite of what `appearance.rs` has
  always said it did.

## [0.1.5] - 2026-08-14

Packaging, and nothing else: the compiled compositor is what 0.1.4 shipped.
The packages are named after the compositor rather than the toolkit it was
rewritten on, every engine has a `-git` and a `-bin` form beside its source
recipe, and all nine sit in one directory each under `packaging/aur`. This is
the release whose artifacts were built from the recipes in its own tree, which
0.1.4's — cut before the rename — could not be.

### Shipping
- One directory per AUR package, all nine under `packaging/aur`, named exactly
  after the repository each one is pushed to — the three source recipes moved
  there from `packaging/arch`, which is gone. A push is a copy of a directory
  now rather than a rule about which file lives where. `build-in-container.sh`
  and `Containerfile` moved up to `packaging/`, and the build script takes a
  package name (`viewport-wpe-git`) as well as an engine (`wpe`), so a `-git`
  or `-bin` recipe can be built before it is pushed anywhere.
- Nine AUR recipes rather than five: every engine now has a `-git` form that
  follows `main` and reports `0.1.4.rN.gSHORT`, and Chromium has the `-bin`
  form the other two already had. The `-git` recipes are their source recipe
  with three differences — the name, a `pkgver()`, and a branch instead of a
  tag — so a change to a build step belongs in `packaging/arch` and then in its
  twin.
- The packages are named after the compositor rather than after the toolkit it
  was rewritten on: `viewport-webkitgtk`, `viewport-wpe`, `viewport-chromium`
  and the two `-bin` recipes beside them, each providing `viewport` and
  conflicting with the others, since a system takes one. Nothing had been
  published under the old names, so there is nothing to migrate. The recipes,
  the container image and every URL follow the repository, which is
  `codebam/viewport`.

## [0.1.4] - 2026-08-14

A desktop that can be given a picture, and a run of fixes to the things that
touch two monitors — a border drawn onto the screen next door, a capture of
both screens showing the second one empty — plus the keys and clicks that
reached nothing: the play/pause key, `Mod4+Tab` on a layout that keeps windows
out of view, and the bar's own workspace pills and window titles.

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

### Fixed
- A window dragged towards the edge of one monitor drawing a strip of the
  *next* monitor's window borders over that monitor's windows. The shell
  measures a frame rather than what it painted of one, and the compositor drew
  the shell's pixels wherever that rectangle reached — which on the screen next
  door is that screen's own desktop. The frame is now reported clipped to the
  output it was drawn on, and the compositor draws a window's frame only on the
  output the shell drew the window on.
- Clicking a workspace number or a window's title in the bar doing nothing
  under `bar: auto`. The bar is on screen only while Mod4 is held, so every
  click on it carries the gesture modifier — and with no window under the
  pointer the press started a *pan*, which swallowed it. The compositor now
  declines its Mod4 gestures over anything the shell drew in front of the
  windows, and the floating bar takes the pointer instead of waving it through.
- A capture of every monitor at once showing the second monitor as its desktop
  and window frames with no windows in them. Each monitor's element list
  carries the whole shell buffer — it spans the layout — and a monitor drawing
  itself is bounded by its own framebuffer, which a capture of the whole desk
  is not: the first monitor's copy of the shell was drawn over every monitor
  after it, with the clients behind it. Each monitor's picture is now held to
  its own rectangle.
- The play/pause media key doing nothing. It was bound as `XF86AudioPause`,
  which xkb puts on the *shifted* level of that key — chords match the
  unshifted keysym, so the binding named a level the key cannot produce, while
  skip and previous worked and made it look like playerctl failing. It is
  `XF86AudioPlay` now, running `playerctl play-pause` rather than `pause`.
- The volume, mute, mic-mute and brightness keys, documented as bound by
  default and bound only in `data/config.example.json`. They are defaults now,
  5% a press.
- The active window's border drawn across the bar. The bar sat on z-index 3
  and three window layers sat above it — floating at 5, the canvas's focused
  window and solar's sun at 4 — and the compositor's copy of the bar is a crop
  of the same page, so the border was over the clock on screen as well. The bar
  is above every window layer now, and a sweep in the shell tests holds it
  there.
- `Mod4+Tab` skipping the windows a layout keeps out of view — a column
  scrolled off the strip, a window panned off the canvas. Those are reported to
  the compositor as not on screen, and its cycle walks what is on screen, so the
  one key whose job is reaching them could not. In those two layouts the chord
  now goes to the shell, which walks the whole workspace and brings the window
  it lands on into view.

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
- Cut `0.1.4`: every place the version is written moves together, and the three
  source recipes in `packaging/arch` go back to naming the tag (`_tag=v0.1.4`)
  rather than the commit they sat on between releases. The `-bin` recipes carry
  the new `pkgver` and their `sha256sums_x86_64` stay stale until the artifacts
  are built and uploaded.
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

[Unreleased]: https://github.com/codebam/viewport/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/codebam/viewport/releases/tag/v0.1.5
[0.1.4]: https://github.com/codebam/viewport/releases/tag/v0.1.4
[0.1.3]: https://github.com/codebam/viewport/releases/tag/v0.1.3
