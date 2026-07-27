# Viewport

A Wayland compositor whose entire shell — wallpaper, dock, window frames,
titlebars — is a web page, composited zero-copy alongside native Wayland
clients.

wlroots handles DRM/KMS, input and the `xdg-shell` protocol. WPE WebKit renders
the UI to a DMA-BUF. Neither ever hands a pixel to the CPU.

```
        ┌──────────────────────────────────────────┐
        │  shell (HTML/CSS/JS, loaded from a URL)  │
        └───────────────┬──────────────────────────┘
                        │  WPEBufferDMABuf     ▲ JSON over
                        ▼  (render_buffer)     │ script handler
        ┌──────────────────────────────────────┴───┐
        │  viewport (C)                            │
        │    wlr_scene   layer_web   ← shell       │
        │                layer_apps  ← xdg clients │
        └───────────────┬──────────────────────────┘
                        ▼  wlr_scene_output_commit
                     DRM/KMS
```

## How it fits together

**The shell is the bottom layer.** A single `wlr_scene_buffer` covering the
whole output layout, fed by WebKit's DMA-BUF. Client windows are stacked above
it in `layer_apps`.

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

**Hit-testing falls out of the layering.** `wlr_scene_node_at()` returns a client
surface when the pointer is over a window and the shell's buffer everywhere
else, so "click went to the titlebar" versus "click went to the app" needs no
geometry bookkeeping and cannot go stale mid-animation.

**Frame pacing is real vblank.** WebKit will not paint frame N+1 until we
acknowledge frame N; we acknowledge from the output's frame handler.

**Explicit sync throughout.** WebKit attaches a rendering fence to each frame;
we import it into a `drm_syncobj` timeline and let `wlr_scene` wait on that
point rather than blocking the compositor. Wayland clients get
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
nix develop          # meson, ninja, wlroots 0.20, WPE WebKit, test clients
meson setup build
ninja -C build
```

The first `nix develop` compiles WPE WebKit from source. It is a full WebKit
build: expect hours and tens of gigabytes. There is no binary cache for it.

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

MIT, matching wlroots. WPE WebKit and GLib are LGPL-2.1+ and dynamically
linked, which imposes no licence condition on this code.
