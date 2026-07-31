# Viewport

A Wayland compositor whose entire shell — wallpaper, dock, window frames,
titlebars — is a web page, composited zero-copy alongside native Wayland
clients.

Smithay handles DRM/KMS, input and the `xdg-shell` protocol. WPE WebKit renders
the UI to a DMA-BUF. Neither ever hands a pixel to the CPU.

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

## Build

```sh
nix build .#viewport-smithay   # the compositor, web engine included
```

Or to work in the tree:

```sh
nix develop .#rust   # the toolchain and the test suite's dependencies
cargo test --workspace
scripts/integration.sh target/debug/viewport   # real clients, headless
```

`.#rust` deliberately carries no WPE WebKit: the web engine is behind a
non-default feature, so the tests do not need it and the shell is not linked
into what they run. `nix develop` on its own is the fuller workstation shell.

WPE WebKit is a full WebKit build — hours, and tens of gigabytes — so it comes
from the project's binary cache rather than being compiled. flake.nix names
that cache and the key it is signed with.

```sh
nix build .#wpewebkit   # do this once, deliberately, before anything else
```

Run nested inside an existing compositor:

```sh
./build/viewport --url http://localhost:3000 --startup foot
```

On a TTY it takes the DRM session directly (needs `seatd` or logind).

## Documentation

The reference material lives in `docs/`:

| | |
| --- | --- |
| [`docs/configuration.md`](docs/configuration.md) | the two config tiers, the command-line flags, the default keybindings, dark mode and the portal, media keys, window rules, and the two layout models |
| [`docs/ipc.md`](docs/ipc.md) | both transports and every message in each direction, what a shell has to do to place a window, and how the layout is remembered across a restart — plus the overview, logging and the shell tests |
| [`docs/protocols.md`](docs/protocols.md) | HDR, notifications, tablets, idle and locking, what clients may ask for and what they are told, and what is verified on real hardware |
| [`docs/debugging.md`](docs/debugging.md) | screenshotting the session from inside it, pointer capture, XWayland, and what happens when the shell stops answering |

Two shells ship with it, at opposite ends of the same protocol:

| | |
| --- | --- |
| [`data/shell/`](data/shell/shell.md) | the reference desktop — tiling and scrolling layouts, workspaces, an overview, a status bar |
| [`examples/kiosk/`](examples/kiosk/README.md) | one application fullscreen and nothing else, in about two hundred lines, with a config that locks the machine down and a frank account of what that does not achieve |

## Installing

On Arch or CachyOS, `packaging/arch/` has a PKGBUILD. Every dependency is
already packaged there — including `wpewebkit` built with the WPE platform API,
which is the one that would otherwise mean compiling a browser engine — so it
builds in about a minute:

```sh
git archive --format=tar.gz --prefix=viewport-0.1.0/ -o viewport-0.1.0.tar.gz HEAD
cp packaging/arch/PKGBUILD . && makepkg -si
```

An installed copy needs no arguments: it ships its own shell and defaults to
it, and a `wayland-sessions` entry means a display manager will offer it as
something to log into. See `packaging/arch/README.md` for building it in a
container, which has two non-obvious wrinkles.

On NixOS, `flake.nix` provides both a package and a dev shell.

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
licence condition on this code.
