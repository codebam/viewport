# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are tagged `vX.Y.Z`. The list below starts at the release the tree is
currently cut from; earlier history lives in `git log`, which this file exists
to summarise rather than to duplicate.

## [Unreleased]

### Fixed
- VRAM climbing for as long as a screen share was open — a couple of hundred
  megabytes a minute on a shared 1440p screen, none of it given back until the
  session ended. Two things were doing it, and a share is what made both
  visible: it keeps every client painting, and an idle desktop paints nothing.
  - The Vulkan renderer keeps one image per shared-memory `wl_buffer` it has
    uploaded, so a client that paints every frame is not reallocated and
    re-uploaded every frame. Those entries are keyed by the buffer's object id
    and the renderer offers `forget_shm_buffer` for the compositor to call when
    one dies — which nothing ever did. `buffer_destroyed` was an empty body, so
    every shm buffer any client had ever destroyed left its image behind for
    the life of the session. Destroyed buffers are now queued there and
    forgotten once a turn of the event loop, which is before the renderer is
    moved out to draw with.
  - A capture composited into a buffer it allocated and freed per frame: a
    whole screen off the GPU, thirty times a second, for every share on the
    readback path — and one `vkCreateImage` and one import per frame with it.
    The buffers are now kept between frames by shape, at most four of them, and
    let go of once nothing is being captured.
- The screencast test's stand-in frontend never claimed
  `org.freedesktop.portal.Desktop`, so every call it made was refused as soon
  as the compositor started checking that every caller on the impl interface is
  the frontend. It claims the name now, and retries the first call while the
  compositor's watcher catches up with the signal. What still fails there is a
  token that no longer survives a compositor restart, which is a question about
  the remembered table rather than about the test.
- A share that was resized never went back to being drawn into directly. Only
  the Vulkan renderer can allocate the buffers a consumer imports, and the
  renderer is generic where a resize is answered — so the DRM backend answered
  every renegotiation with no buffers at all, which is the offer of a DMA-BUF
  format withdrawn. One resize of a shared window, and the share spent the rest
  of the session compositing into an offscreen and reading it back. The
  allocation is now asked of whichever renderer is drawing, through
  `Captures::cast_targets`; GLES still answers with nothing, which is what puts
  a nested session on shared memory as before.

## [0.1.8] - 2026-08-17

### Added
- A binding's output and exit status reach the log. A spawned command had its
  stderr on `/dev/null` and its status thrown away, so the log said only that
  something had been started — a screenshot script that died on a bad argument,
  a missing tool or a `set -e` two lines in looked exactly like one that
  worked, and there is no terminal watching to tell them apart. The first
  twenty lines of a child's stderr are now logged against the command that
  produced them, and a non-zero exit is logged with its status. Twenty because
  a failing script says what is wrong at the start, while a browser left
  running for a day writes tens of thousands of lines that would bury
  everything else; reading continues past the cap even though logging stops,
  since a pipe nobody drains fills up and the next write would block the child
  mid-task.

### Changed
- The shell is restarted with a backoff, and is no longer given up on. It used
  to be respawned on the next tick every time, five times a minute, and then
  left down for the rest of the session. Both halves of that were wrong on a
  real fault: an AMD GPU that had run out of memory — a game, a screen capture
  and a language model on the same card — rejected every command submission on
  it, Mesa aborts a process when that happens, and the shell, Chromium and OBS
  all died together. The shell was then respawned five times in ninety
  seconds, each one asking the same exhausted GPU for another 5120x1440
  buffer, before the desktop went blank until the session was restarted. Now
  the first restart is immediate and each one after it waits twice as long
  (1s, 2s, 4s, 8s), and a run that gets through those keeps retrying every
  thirty seconds rather than stopping — so the desktop comes back on its own
  once the cause has gone, and a page that genuinely cannot load costs one log
  line every thirty seconds instead of a session.
- A frame costs less, and the desktop stops re-answering questions it has
  already answered. The shell posts one `view.layout` per window per animation
  frame, and each one used to end by restacking the whole space and re-asking
  which output colour every feedback surface sits on — questions about the
  desktop rather than about the window the message was for, so eight windows
  got the same answer eight times, each time allocating and scanning every
  view. Both are now owed once and settled once per batch, before anything
  else in the event loop can see a stack that has not been restacked. Around
  that: `views` keeps a surface-to-position index, checked against the list
  before it is trusted and falling back to the old walk when it is stale, so
  finding the window behind a commit, a focus change or a hit test no longer
  costs a `WlSurface` clone per window passed; the bridge's writer thread
  drains whatever the producer has already queued into one `write_all`,
  bounded at 64 KiB and never waiting for a straggler, which takes an
  eight-window desk at 60fps from around 480 small syscalls a second to a
  handful; and `frame_for` stops minting border element ids for windows with
  no view, stops building a `String` key per output per frame for a debug line
  nobody is listening to, and stops looking up a lock surface in a map that is
  empty outside a locked session. No behaviour changes from any of it.

### Fixed
- A second locker taking a screen from a lock screen that is drawing, which
  could leave a session locked after a correct password. Smithay grants every
  `ext-session-lock` request and leaves the decision to the compositor, so
  nothing refused the second one; taking it also cleared `lock_surfaces`,
  dropping the surfaces of a locker that was still running and still drawing.
  Two clients then owned one screen, only the newer was rendered, and
  unlocking the one you could see left the other holding a lock nothing on
  screen could reach. It needed nothing unusual to hit: the idle deadline and
  the `lock` binding both call `lock_session` and neither asked whether the
  session was already locked, so locking by hand and then letting the idle
  timer expire was two lockers. The rule is about pixels rather than about who
  asked first — a lock screen that is drawing may not be taken over, one that
  is not may, because running another locker is the only way out of a locker
  that crashed and `check_lock_screen` says to do exactly that. A refusal
  drops the `SessionLocker`, whose `Drop` sends `finished`.
- `Mod4+Shift+b` turning the screens off and something turning them straight
  back on. `render_if_needed` was the only place that checked whether the
  session was blanked, and four paths call `render` around it: the vblank of a
  flip still in the air when the screens went off, the watchdog resuming a
  chain that has legitimately stopped, a connector rescan, and a session
  resume. Any one of them queues a frame, and a queued frame is what wakes a
  panel. The check now sits where the frame is built and committed, and again
  in `render_pass`, the last gate before KMS — so a monitor that arrives
  during a blank comes up asleep with the rest rather than lighting on its
  modeset, which a DisplayPort screen does on its own after sleeping.
- The cursor being drawn at whatever size the theme happened to ship rather
  than the size asked for: the nearest image to `XCURSOR_SIZE` * scale was
  drawn at its own resolution, so asking for 40 from a theme with 32 and 48
  gave 32. The nearest image is now picked for resolution only and drawn with
  an explicit source rectangle and logical size, with the hotspot rescaled to
  match or the pointer aims off the tip of the arrow. Two related leaks are
  closed with it: a reload rebuilt the compositor's theme without telling the
  settings portal, so every toolkit sized its own cursors from the startup
  value for the rest of the session — the portal is now updated, announced,
  and the pointer redrawn rather than waiting for the next motion — and
  neither `XCURSOR` variable reached the session environment, so a client
  started by systemd or activated over D-Bus picked its own default.
- The keymap on an empty desktop not scrolling when it does not fit. The
  tutorial block has had a max-height and `overflow-y: auto` since it was
  written and neither did anything: `.empty` is inert so a click reaches the
  desktop behind it, `pointer-events` is inherited, and the wheel went through
  the list to the page — the one box on screen meant to scroll was the one
  that could not. A `columns: 2` box with a bounded height does not scroll in
  any case, since multicol lays out another column to the right and
  `overflow-y` alone computes the other axis to `auto` as well, so the chords
  that did not fit went behind a horizontal scrollbar instead of below the
  fold. The list is now a wrapping flex row, which overflows downwards, and
  takes the pointer itself while the empty state around it stays
  click-through. Flex and not grid because Servo drops `display: grid` and the
  box falls back to `block` — a single ribbon down the middle, which is what
  two columns exist to avoid.
- The border on a zoomed-out window: one thin line hanging below it and
  nothing anywhere else. `border_sides` was given the frame the shell measured
  on screen together with the hole in the client's own pixels, which are the
  same rectangle only at scale 1. A window on a zoomed-out canvas plane — or a
  thumbnail in the overview, or a cold window in solar's outer orbit — is
  drawn small and never asked to resize itself, so the hole was far larger
  than the frame around it: the bottom and right sides started past the
  frame's far corner and clamped to nothing, and the left side kept the
  client's full height. The scale now comes with it and the hole is converted
  to what is actually drawn before the sides are cut out.

## [0.1.7] - 2026-08-15

### Added
- Notifications can make a sound. The compositor claims
  `org.freedesktop.Notifications` itself, which took playback away along with
  the window when it replaced mako and dunst — a notification has been silent
  here since. `notifications.sound_file` and `notifications.sound_name` in the
  config file say what one sounds like by default, and all three of the
  specification's sound hints are honoured: a sender's own `sound-file` or
  `sound-name` overrides the default for its notification, and
  `suppress-sound` silences it, because that hint means the sender is playing
  its own and two sounds for one event is worse than none. Playback is
  PipeWire, which this program already links for the screencast portal, with
  symphonia decoding — no libcanberra, and so no new library in the closure or
  in nine AUR packages. Each sound decodes and plays on a thread of its own, so
  no sender blocks for the length of one, and decoded files are kept because
  the same short sound plays all session. `sound_name` is resolved by the
  sound-theme search written out: data directories, `stereo/` before the flat
  layout, `.oga`/`.ogg`/`.wav`, and `Inherits` followed with `freedesktop` as
  every theme's implicit parent. A session with no sound server plays nothing,
  says so once, and drops `sound` from its reported capabilities so a sender
  knows to play its own.

## [0.1.6] - 2026-08-15

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

[Unreleased]: https://github.com/codebam/viewport/compare/v0.1.8...HEAD
[0.1.8]: https://github.com/codebam/viewport/releases/tag/v0.1.8
[0.1.7]: https://github.com/codebam/viewport/releases/tag/v0.1.7
[0.1.6]: https://github.com/codebam/viewport/releases/tag/v0.1.6
[0.1.5]: https://github.com/codebam/viewport/releases/tag/v0.1.5
[0.1.4]: https://github.com/codebam/viewport/releases/tag/v0.1.4
[0.1.3]: https://github.com/codebam/viewport/releases/tag/v0.1.3
