# Rust rewrite (branch `smithay`)

This branch is a ground-up reimplementation of the Viewport compositor in Rust,
tracking `git@github.com:codebam/viewport-smithay.git`. The C compositor on
`main` (`git@github.com:codebam/viewport.git`) remains the daily driver and
keeps receiving fixes; this branch optimises for correctness over speed to
parity.

What does *not* change: `data/shell/` is the product. The HTML/CSS/JS shell is
carried over untouched, and the JSON protocol it speaks is the fixed contract
the Rust compositor must satisfy.

## Decisions

| Area | Decision |
|---|---|
| Compositor framework | Smithay 0.7 |
| Scene graph | `smithay::desktop::Space` + a custom `RenderElement` for the shell |
| Web engine | Servo (`libservo`), replacing WPE WebKit |
| IPC | Byte-compatible with the C protocol; `data/shell/*.js` unchanged |
| Colour | Colour-managed render path from day one, `color-management-v1` wire protocol deferred |
| Explicit sync | `linux-drm-syncobj-v1` from day one (Smithay ships it) |
| License | `viewport` GPL-3.0-or-later; `viewport-ipc` and `viewport-web` MIT |

### Licensing

The C build is MIT throughout. The rewrite splits it, which is only possible
because there is exactly one copyright holder — relicensing needs no consent
today and would need everyone's later.

`crates/viewport` is **GPL-3.0-or-later**. Compositor internals are the part
with the most existing prior art, and nearly all of it is GPL: niri,
cosmic-comp, KWin, mutter. `color-management-v1` and `wlr-output-management`
both have to be written from scratch against Smithay, and being able to adapt
niri's rather than work from the bare XML is worth real weeks.

`crates/viewport-ipc` and `crates/viewport-web` stay **MIT**. The protocol crate
should be reusable by anyone writing an alternative shell against this
compositor, and the DMA-BUF `RenderingContext` in `viewport-web` is exactly the
kind of thing Servo would want upstream — Servo is MPL-2.0, which cannot absorb
GPL-3.0 code.

The cost of the GPL half is that it is a one-way valve: code can be adapted
*from* niri and cosmic-comp, but nothing in `crates/viewport` can ever be
upstreamed into Smithay or wlroots, which are MIT. Anything generic enough to
be worth upstreaming should be written in one of the MIT crates on purpose.

### Why the C tree is still here

`src/` and `include/` stay for now as the reference being ported against —
every Rust module lands with the C file it replaces named in its header
comment. They get deleted once parity lands. `data/`, `protocols/`, `docs/`
and `packaging/` are permanent.

## What Smithay gives us and what it does not

Verified against Smithay master `39cd5f1` (2026-07-24).

Present in `smithay::wayland`: `shell::xdg`, `shell::wlr_layer`, `session_lock`,
`text_input`, `tablet_manager`, `foreign_toplevel_list`, `drm_syncobj`, `dmabuf`,
`presentation`, `viewporter`, `fractional_scale`, `image_copy_capture`,
`idle_notify`, `security_context`, `single_pixel_buffer`, `content_type`,
`alpha_modifier`.

Absent, and therefore ours to write:

- **`color-management-v1`.** Smithay has no handler. The *wire bindings* do
  exist — `wayland_protocols::wp::color_management`, already pulled in via
  Smithay's `staging` feature — so this is a `Dispatch` impl plus renderer
  colour transforms, not protocol codegen. niri and cosmic-comp are GPL-3.0
  and Viewport is MIT, so their implementations can be read but not copied.
- **`wlr-output-management-unstable-v1`.** Same situation; XML is already in
  `protocols/`.
- **`wlr-output-power-management-unstable-v1`.** XML in `protocols/`.
- **A `wlr_scene` equivalent.** See below.

## Scene graph

The C build leans on `wlr_scene` for three things at once: hit-testing via
`wlr_scene_node_at()`, damage tracking, and the shell-buffer-under-clients
stack. Smithay has no scene graph, so:

- `desktop::Space` holds the client windows.
- The shell's DMA-BUF becomes a custom `RenderElement` rendered *below* every
  space element, spanning the whole output layout.
- Hit-testing is `Space::element_under()` with a fallback to the shell element,
  which reproduces the C property that "pointer is over a window" versus
  "pointer is over the shell" needs no geometry bookkeeping.

Subsurface and popup trees, which `wlr_scene` handled for free, come from
`smithay::desktop::{Window, PopupManager}`.

## Web engine: Servo

Two gaps between WPE WebKit and Servo have to be bridged. Neither is fatal.

**1. There is no `WPEBufferDMABuf` equivalent.** Servo's built-in
`RenderingContext` implementations (`WindowRenderingContext`,
`OffscreenRenderingContext`, `SoftwareRenderingContext`) are all surfman-backed
and none exports a DMA-BUF. Servo does accept a custom `RenderingContext`, so
we implement one that renders into a GBM buffer object imported as an
`EGLImage` and exported with `EGL_MESA_image_dma_buf_export`. The per-frame
fence WPE attached to each buffer becomes an `EGL_KHR_fence_sync` created after
the GL flush, imported into the same `drm_syncobj` timeline the C build used.

**2. There is no `window.webkit.messageHandlers`.** The shell's entire bridge
to the compositor is two call sites:

- outbound, `data/shell/state.js:13` — `window.webkit.messageHandlers.viewport.postMessage(JSON.stringify(msg))`
- inbound, `src/web.c:51` — the compositor evaluates
  `window.dispatchEvent(new CustomEvent('viewport', {detail: ...}))`

So a preload script injected before the shell's own scripts can define
`window.webkit.messageHandlers.viewport` over whatever transport Servo gives us,
and dispatch the same `CustomEvent` inbound. The JSON on the wire stays
byte-identical and `data/shell/*.js` needs no edit. This is why
`viewport-web` exposes a transport-agnostic `WebEngine` trait rather than
binding the shell to Servo directly — if Servo does not work out, only that
crate changes.

## Crates

```
viewport-ipc   the JSON protocol, serde types, no compositor deps
viewport-web   WebEngine trait; servo backend behind the `servo` feature
viewport       the binary: Smithay compositor, Space, input, outputs
```

`viewport-ipc` has no dependency on Smithay or Servo on purpose: it is the
piece with an existing test oracle (`tests/shell.test.js`) and it should stay
testable without a compositor.

## Protocol coverage

Inbound (shell to compositor), from `src/ipc.c:1415`:

`view.layout` `view.visible` `view.fullscreen` `view.focus` `view.close`
`view.opacity` `view.query` `shell.focus` `shell.overview` `session.save`
`session.query` `notification.action` `notification.dismiss`
`notification.expire` `output.configure` `output.hdr` `output.confirm`
`output.active` `output.query` `output.test_add` `output.test_remove`
`bind.add` `quit`

Outbound (compositor to shell): `view.added` `view.removed` `view.props`
`view.focused` `config` `modifiers` `session.restore` `notification.add`
`notification.close` `output.layout` `shell.command` `error`

## Order of work

1. **Done.** `viewport-ipc` — the protocol, ported field by field.
2. **Done.** `viewport` — winit and headless backends, `Space`, xdg-shell, the
   control socket. Windows are placeable by a script before any web engine
   exists; see below.
3. `viewport-web` — Servo behind the custom DMA-BUF `RenderingContext`, the
   preload shim, and the real shell rendering.
4. udev/DRM backend, explicit sync, colour transforms.
5. layer-shell, session-lock, foreign-toplevel, text-input, tablet.
6. `color-management-v1`, `wlr-output-management`, Xwayland.

## Running it now

```
cargo run -p viewport -- --headless --socket /tmp/vp.sock   # no GPU, no display
cargo run -p viewport                                       # nested, in a window
node scripts/place.js /tmp/vp.sock                          # a stand-in shell
```

`scripts/place.js` speaks the same protocol `data/shell/*.js` speaks and does
the one thing the compositor cannot do for itself: decide where windows go. It
tiles them left to right. Point a client at the compositor's Wayland socket
(`WAYLAND_DISPLAY=wayland-N foot`) and it gets placed.

The headless backend is a virtual output and a timer standing in for vblank, no
renderer. It is what makes the whole window lifecycle and the entire IPC
protocol testable in CI, and what `output.test_add` is gated on.

### What step 2 deliberately does not do

- **No layout policy, at all.** A window is created but not mapped into the
  `Space` until a `view.layout` arrives for it. There is nowhere a window could
  legitimately be drawn before the shell has said where.
- **No move or resize grabs.** A client asking the compositor to move or resize
  it has asked the wrong party — the frame is DOM and dragging an edge is the
  browser resizing a flex container. Those requests are ignored, not implemented.
- **Per-window opacity is stored but not applied.** It needs a render element
  that can carry alpha, which arrives with the shell render path in step 3.
- Notifications, keybindings, config parsing and HDR answer with an `error`
  naming what is missing rather than failing silently.
