# Viewport

A Wayland compositor whose entire shell — wallpaper, dock, window frames,
titlebars — is a web page, composited zero-copy alongside native Wayland
clients.

Smithay handles DRM/KMS, input and the `xdg-shell` protocol. WebKit renders the
UI to a DMA-BUF. Neither ever hands a pixel to the CPU.

Which engine is a choice: WPE inside the compositor, WebKitGTK in a process of
its own as an ordinary Wayland client, Chromium as a browser this does not link
at all, the same Blink embedded through CEF, or Servo — driven as a browser by
default, and embedded for anyone willing to compile it. Only two are built from
source, and the page cannot tell which one it is running under. See
[`docs/shell-backends.md`](docs/shell-backends.md).

```
        ┌──────────────────────────────────────────┐
        │  shell (HTML/CSS/JS, loaded from a URL)  │
        └───────────────┬──────────────────────────┘
                        │  WPEBufferDMABuf     ▲ JSON over
                        ▼  (render_buffer)     │ script handler
        ┌──────────────────────────────────────┴───┐
        │  viewport (Rust)                         │
        │    Space       shell element ← shell     │
        │                space elements ← clients  │
        └───────────────┬──────────────────────────┘
                        ▼  DrmOutput / frame submission
                     DRM/KMS
```

There was a C implementation on wlroots, and this is a rewrite of it rather
than a new project — which is why so much here is described as "what the C
build did". It reached parity and was deleted; `docs/RUST-REWRITE.md` records
what parity was defined to mean and how it was measured.

## How it fits together

**The shell is the bottom layer.** A custom `RenderElement` spanning the whole
output layout, fed by WebKit's DMA-BUF and drawn below every space element.
Client windows are stacked above it in `smithay::desktop::Space`.

**Window placement is JavaScript's job.** The shell draws a frame with a hole in
it — a `<div class="viewport">` — measures where that hole landed on screen, and
sends the rect over IPC. The compositor moves the client's scene node there.
There is no layout policy in the C code at all.

**Tiling is a tree of flexboxes.** Split containers hold leaves, i3-style, and
the tree renders to nested CSS flex containers — so the browser computes every
rectangle and the shell only measures the result. Splitting, moving, resizing
and fullscreen restructure the tree or adjust weights; no code calculates a
window position. The gap between windows is a real element, which is why
dragging a window edge needs no compositor support at all.

**Hit-testing falls out of the layering.** `Space::element_under()` returns a
client surface when the pointer is over a window, and the shell element
everywhere else, so "click went to the titlebar" versus "click went to the app"
needs no geometry bookkeeping and cannot go stale mid-animation.

**Frame pacing is real vblank.** WebKit will not paint frame N+1 until we
acknowledge frame N; we acknowledge from the output's frame handler.

**Explicit sync throughout.** WebKit attaches a rendering fence to each frame;
we import it into a `drm_syncobj` timeline and wait on that point in the render
pass rather than blocking the compositor. Wayland clients get
`zwp_linux_drm_syncobj_v1`.

## Why WPEPlatform and not libwpe

nixpkgs ships `libwpe` and `libwpe-fdo`, and neither is a browser engine —
`libWPEBackend-fdo-1.0.so` contains zero WebKit symbols. They are the backend
ABI and one implementation of it. The engine, `libWPEWebKit`, is not packaged in
nixpkgs at all, and the `webkitgtk` tarball cannot be rebuilt with `-DPORT=WPE`
because it ships the GTK port only. So the flake builds WPE WebKit from the
separate upstream `wpewebkit-2.52.5.tar.xz`.

Since we compile the engine either way, we build it with
`-DENABLE_WPE_PLATFORM=ON`. That drops `libwpe` and `libwpe-fdo` entirely and
replaces the old `wpe_view_backend` indirection with GObject subclassing:
`WPEDisplay` advertises our DRM device and formats, `WPEView::render_buffer`
delivers each frame as a `WPEBufferDMABuf` whose fds, offsets, strides and
modifier map field-for-field onto `wlr_dmabuf_attributes`.

The legacy path would have meant standing up our own EGL display on a `gbm`
device and pulling fds out with `EGL_MESA_image_dma_buf_export` — several
hundred lines of glue to arrive at the same dma-buf.

## Run it

```sh
nix run github:codebam/viewport-smithay
```

That is the whole thing: a compositor and a desktop, from a flake, with no
WebKit compiled anywhere. It nests inside the session you are already in when
there is one and takes the DRM device when there is not, so trying it from a
terminal costs a window rather than the machine. `Mod4+Shift+e` quits, and
`-- --exit-after 30` gives it a deadline in case that is the thing that is
broken.

The same binary is the client for its own control socket, so a running session
can be driven from a terminal without anything else installed:

```sh
viewport msg -t view.focus --id 12
viewport msg -t output.query --pretty
viewport msg -t subscribe view.focused    # follow events until ^C
viewport msg -t quit                      # from a second TTY, when the first is stuck
viewport msg --help                       # every message and its fields
```

Every message in [`docs/ipc.md`](docs/ipc.md) can be sent this way, under its
wire name.

The default package runs Servo, in the browser nixpkgs builds: nothing in that
closure builds an engine, and it is the lightest desktop measured — 8.5% of a
core under load against 9.9 to 11.5, 357 MB against 449 to 639, four processes
against nine to twelve. What it costs is paint rate, 14 frames a second against
43 to 48, which is worth knowing before taking the default: `.#cef` is the one
for a desktop that should feel quick and `.#webkitgtk` for a machine short of
memory rather than CPU. See [`docs/benchmarks.md`](docs/benchmarks.md), and ask
for any of them by name.

## Build

```sh
nix build github:codebam/viewport-smithay        # the default, above — servoshell
nix build github:codebam/viewport-smithay#wpe    # in-process; builds WebKit
nix build github:codebam/viewport-smithay#chromium
nix build github:codebam/viewport-smithay#cef
nix build github:codebam/viewport-smithay#webkitgtk
```

The package attributes are named for the engine that draws the shell — `.#wpe`,
`.#webkitgtk`, `.#chromium`, `.#cef`, `.#servoshell` — because that is the only
thing that differs between them. There is no `.#servo`: the embedded Servo
shell is a cargo dependency on the engine's source, so it is built by hand
rather than packaged. See [`docs/shell-backends.md`](docs/shell-backends.md).

Or to work in the tree:

```sh
nix develop .#rust   # the toolchain and the test suite's dependencies
cargo test --workspace
scripts/integration.sh target/debug/viewport   # real clients, headless
```

`.#rust` deliberately carries no WPE WebKit: the web engine is behind a
non-default feature, so the tests do not need it and the shell is not linked
into what they run. `nix develop` on its own is the fuller workstation shell —
debugger, test clients, and the CEF and WebKitGTK engines, all of which
substitute from cache.nixos.org.

No engine there is WPE WebKit either. That one is a full WebKit build — hours,
and tens of gigabytes — and nothing substitutes it, because `wpewebkit` is
packaged by nobody, so it is behind a shell of its own:

```sh
nix develop .#wpe    # the workstation shell plus the WPE engine
```

```sh
nix build .#wpewebkit   # do this once, deliberately, before anything else
```

That build is the whole reason there are other backends at all, and the reason
none of them is `wpe`. `.#webkitgtk` runs the same shell against nixpkgs'
prebuilt WebKitGTK — the same WebKit version, a different port, out of process
— and substitutes from cache.nixos.org like anything else, as do `.#chromium`,
`.#cef` and the default `.#servoshell`.

Run nested inside an existing compositor:

```sh
./build/viewport --url http://localhost:3000 --startup foot
```

`--url` on a session with more than one monitor puts that page on the first
screen and the bundled desktop on the rest. `--url-span` puts it back across
every screen, which is what a shell being developed wants — see
docs/configuration.md.

On a TTY it takes the DRM session directly (needs `seatd` or logind).

### Checks before a commit

The checks run in two places, and neither is a superset of the other.

`.github/workflows/ci.yml` runs on every push and pull request: `shell` for the
layout engine under node, `rust` for `cargo fmt`, `cargo clippy`, the unit tests,
the Wayland integration tests and a packaged build, and `asan` for the same suite
instrumented. What it cannot run is anything that needs WPE WebKit — four hours
on a hosted runner, against a build tree larger than the disk — so `--features
wpe` is checked by the hook below and nowhere else. The comment at the top of
that file says what used to be there and why it went.

The hook is the other half, and has to be turned on once per clone:

```sh
git config core.hooksPath .githooks
```

After that `git commit` runs, against the staged changes only:

- the shell layout tests, if anything under `data/shell`, `examples/kiosk` or
  the JavaScript tests is staged;
- `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test --workspace`,
  if anything under `crates/`, `Cargo.toml`, `Cargo.lock` or `flake.nix` is;
- and a `cargo check` of *both* halves of the `wpe` feature, because a
  `#[cfg(feature = "wpe")]` on the wrong item compiles cleanly in whichever
  configuration you happen to test and breaks the other.

Staging nothing it can break — a PKGBUILD, a document — runs nothing.

`git commit --no-verify` skips it. It builds into `target/pre-commit` rather
than `target`, so it cannot replace a `target/release/viewport` built with
`--features wpe` with one built without: that swap is silent, and what it
produces is a session that comes up grey with nothing in the log to say why.

## Documentation

The reference material lives in `docs/`:

| | |
| --- | --- |
| [`docs/configuration.md`](docs/configuration.md) | the two config tiers, the command-line flags, the default keybindings, dark mode and the portal, media keys, window rules, and the two layout models |
| [`docs/ipc.md`](docs/ipc.md) | both transports and every message in each direction, what a shell has to do to place a window, and how the layout is remembered across a restart — plus the overview, logging and the shell tests |
| [`docs/protocols.md`](docs/protocols.md) | HDR, notifications, tablets, idle and locking, what clients may ask for and what they are told, and what is verified on real hardware |
| [`docs/debugging.md`](docs/debugging.md) | screenshotting the session from inside it, pointer capture, XWayland, and what happens when the shell stops answering |
| [`docs/shell-backends.md`](docs/shell-backends.md) | the six engines the shell can be drawn by — WPE, WebKitGTK, Chromium driven, Chromium embedded, Servo driven, Servo embedded — what changes between them and what does not, and how to run the shell process by hand against a live session |
| [`docs/benchmarks.md`](docs/benchmarks.md) | Viewport measured against sway and niri on real scanout — frame rate, CPU per frame, memory, and the second monitor while the first is saturated |

Two shells ship with it, at opposite ends of the same protocol:

| | |
| --- | --- |
| [`data/shell/`](data/shell/shell.md) | the reference desktop — tiling and scrolling layouts, workspaces, an overview, a status bar |
| [`examples/kiosk/`](examples/kiosk/README.md) | one application fullscreen and nothing else, in about two hundred lines, with a config that locks the machine down and a frank account of what that does not achieve |

## Installing

On Arch or CachyOS, `packaging/arch/` has one PKGBUILD per engine — `wpe`,
`webkitgtk` and `chromium`. Every dependency is already packaged there,
including `wpewebkit` built with the WPE platform API, which is the one that
would otherwise mean compiling a browser engine, so each builds in about a
minute:

```sh
cd packaging/arch/wpe && makepkg -si
```

They all install a binary called `viewport` and conflict with each other: a
system takes one. `./packaging/arch/build-in-container.sh wpe` builds any of
them on a machine that is not Arch, which is how they are tested here, and
`./scripts/arch-vm.sh --variant wpe` installs the result into a throwaway Arch
guest and boots the desktop in it.

The `cef` backend is not packaged for Arch. CEF is a prebuilt binary bundle,
the only Arch package of it is `cef-minimal` in the AUR, and that is CEF 121
against the 149 this tree needs. `chromium` gives Arch the same engine with
nothing outside the repositories.

An installed copy needs no arguments: it ships its own shell and defaults to
it, and a `wayland-sessions` entry means a display manager will offer it as
something to log into.

On NixOS, `flake.nix` provides packages, a dev shell and two modules, and
`programs.viewport.shellBackend` picks which engine the session is installed
with:

```nix
programs.viewport = {
  enable = true;
  shellBackend = "servoshell";   # the default; also "cef", "webkitgtk",
                                 # "chromium" or "wpe"
};
```

## Licence

Split, and deliberately. `crates/viewport` is **GPL-3.0-or-later**, because
the compositor internals with the most prior art — `color-management-v1`,
`wlr-output-management` — are all GPL, and being able to adapt niri's rather
than work from the bare XML is worth real weeks. `crates/viewport-ipc` and
`crates/viewport-web` stay **MIT**, so the protocol crate is reusable by anyone
writing an alternative shell and the DMA-BUF `RenderingContext` could be
upstreamed to Servo, which is MPL-2.0 and cannot absorb GPL-3.0 code.

The cost is that it is a one-way valve: nothing in `crates/viewport` can go
back to Smithay or wlroots, which are MIT. `docs/RUST-REWRITE.md` has the
reasoning in full.

WPE WebKit and GLib are LGPL-2.1+ and dynamically linked, which imposes no
licence condition on this code. `crates/viewport-shell-gtk` — the shell as a
separate process, on WebKitGTK — is MIT for the same reason the other two are:
it is a browser window and a socket, and nothing in it is adapted from a GPL
compositor.
