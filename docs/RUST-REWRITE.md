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
| Web engine | WPE WebKit, as the C build uses |
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
and none exports a DMA-BUF.

**This was the one unproven assumption in the plan, and it has been spiked.**
See `crates/viewport-web/src/dmabuf.rs`. Verified against Servo `954690b`
(2026-07-27) and run on real hardware:

- The trait (`components/shared/paint/rendering_context.rs`) is implementable
  from outside Servo. Its required methods are `size`, `resize`, `present`,
  `make_current`, `read_to_image`, `gleam_gl_api` and `glow_gl_api`. Nothing
  forces surfman on an implementor: `connection()` defaults to `None`, which
  costs only WebGL surface sharing.
- `prepare_for_rendering` exists precisely to let an embedder bind its own
  framebuffer. So we allocate with GBM, import as an `EGLImage`, hang it off an
  FBO, and Servo draws into the buffer the compositor will scan out.
- `refresh_driver()` returning our own `RefreshDriver` gives back the frame
  pacing WPE had. `observe_next_frame(callback)` is called when Servo wants the
  next frame; calling it from the output's frame handler pins the shell's paint
  rate to real vblank rather than a free-running timer.
- The fence is `EGL_ANDROID_native_fence_sync`: `eglCreateSyncKHR` after the
  draw calls, `eglDupNativeFenceFDANDROID` for an fd that a `drm_syncobj`
  timeline takes. The test polls it as a `sync_file` to prove it is one.

The decisive test renders on one EGL display and reads the pixels back on a
second, entirely separate one, through the exported fd alone — which is exactly
the Servo-to-compositor handoff. Note that the GPU tests *skip* on a machine
with no render node, so `VIEWPORT_REQUIRE_GPU=1` turns every skip into a
failure; without it a machine where EGL will not even load reports a pass, which
is indistinguishable from the buffer sharing working.

One correction to an earlier assumption: the export path is not
`EGL_MESA_image_dma_buf_export`. The buffer is allocated by GBM, so the fd comes
from `gbm_bo_get_fd` and EGL is only used to *import* it. That is simpler and
avoids depending on a Mesa-specific extension.

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
3. **Done.** `viewport-web` — WPE WebKit, not Servo. The shell draws the
   desktop on both backends: WebKit paints into a DMA-BUF, the compositor
   copies it into a buffer of its own, and each backend imports that.
4. **Done.** udev/DRM backend, verified on hardware: two 2560x1440 displays
   side by side on an RX 7900 XTX, the Vulkan renderer drawing to both, VT
   switching and the quit chord working. Explicit sync and colour transforms
   are in `crates/viewport-vulkan`.
5. **Done.** Config file, layer-shell (bars and launchers), xdg-activation,
   linux-dmabuf, xdg-decoration, Xwayland, the cursor, and directional focus.
6. **Done.** session-lock, verified against swaylock.
7. **Next.** foreign-toplevel, notifications, text-input, tablet,
   `wlr-output-management`, and the X11 half of the clipboard.

## What the config file does, and does not

`~/.config/viewport/config.json`, or `--config`. Applied: `url`, `terminal`,
`menu`, `layout`, `logo`, `tutorial`, `bar`, `rules`, `theme`, `binds`,
`binds_override`, `keyboard`, `cursor`, `outputs`, `startup`.

Also applied: `fallback` and `timeout_ms` (the deadline is on the first
painted frame, not on the load event — a page that loads and then stalls is
invisible to load-failed), `idle`, `adaptive_sync`, `vt_switching`,
`decorations`.

Not acted on: `dark_mode`. Acting on it means running the
`org.freedesktop.impl.portal.Settings` D-Bus service, which is how a GTK or Qt
application learns the colour scheme — the value alone changes nothing. That
is its own deliverable rather than a config key, and the value is stored ready
for it.

Absence is not a default. Every key is optional and the file is a patch over
the built-in values, so a key left out never resets something a flag set. Two
bind blocks, deliberately: `binds` is the whole keymap — presence means no
built-ins, so an empty one asks for none — while `binds_override` layers over
them, and a `null` claims a chord and does nothing with it, which is how a
default is removed rather than replaced.

## Known rough edges on real hardware

Two things the log says on every run. Neither stops it working:

- `Failed to restore previous state. Error: Invalid argument` on exit. Smithay
  restores the pre-compositor DRM state when the device drops, and that commit
  is rejected. The session comes back regardless.
- `Failed to destroy old mode property blob: No such file or directory`, once
  per output during modeset. Harmless; there was no old blob to destroy.

`Unable to become drm master, assuming unprivileged mode` used to be listed
here too, with a note that master was "evidently acquired later, because the
modeset succeeds". It was not. Nothing claimed it, every page flip failed with
EPERM, and because a failed flip takes its vblank with it the output stopped
for good — which read as a dead monitor rather than a permission error. It is
claimed explicitly at startup now. A warning that is filed as harmless because
the next step appeared to work is worth more suspicion than that one got.

## Two backends, one description of a frame

`crates/viewport/src/render.rs` holds what an output draws, and both backends
use it. A `Frame` says what should appear and is worked out with no renderer at
all; `build` turns it into elements with whichever renderer the backend has —
Vulkan on DRM, GLES nested.

That split exists because the two had drifted. The element list used to be
assembled inside the DRM path against one concrete renderer, so the nested
backend drew windows on a flat colour while the real one drew the desktop, and
nested is where most development happens. Layering, clipping and the cursor
live in one place now.

## Diagnostics

Bring-up on real hardware turned on questions the log could not answer, because
the screen was the only witness and several different faults produced the same
picture. Two things exist for that:

- `VIEWPORT_DUMP_OUTPUT=/tmp/out.ppm` writes one composite per output, from the
  same element list and damage tracker the output was given, plus the shell's
  own buffer from the same moment. Comparing the two separates "the shell
  painted nothing" from "the shell never reached the screen"; measuring a pixel
  separates the clear colour from a window's background. Both distinctions
  cost a run each to learn.
- `VIEWPORT_SCANOUT=0` composites everything instead of putting elements on DRM
  planes. Slower and always correct, so it tells a plane problem from a
  renderer problem in one run.

Two references were worth more than reading this code again: `src/` for what
the C build does about the same problem, and smithay's `anvil` for how a
smithay compositor is normally shaped. The rendering loop drew several times
per refresh until it was compared against the latter.

## Running it now

```
./scripts/run-drm.sh                    # real hardware, from a TTY
./scripts/run-drm.sh --exit-after 120   # ... stopping by itself
./scripts/quit.sh                       # stop one from another TTY
cargo run -p viewport -- --headless --socket /tmp/vp.sock   # no GPU, no display
cargo run -p viewport                                       # nested, in a window
node scripts/place.js /tmp/vp.sock                          # a stand-in shell
```

`run-drm.sh` re-enters the dev shell itself: the binary dlopens libvulkan,
libgbm, libEGL and libwayland rather than linking them, so it needs that
library path at run time and not only at build time. Ctrl+Alt+F1 through F12
switch VT and Ctrl+Alt+Backspace quits.

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
