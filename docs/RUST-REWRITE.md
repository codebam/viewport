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

- **`color-management-v1`.** Written, in `crates/viewport/src/color_management.rs`, with
  the renderer's colour transforms behind it. The *wire bindings* existed —
  `wayland_protocols::wp::color_management`, pulled in via Smithay's `staging`
  feature — so it was a `Dispatch` impl rather than protocol codegen. niri and
  cosmic-comp are GPL-3.0 and Viewport is MIT, so their implementations could
  be read but not copied.
- **The wlr protocols, all written by hand.** `wlr-output-management`,
  `wlr-output-power-management`, `wlr-gamma-control`, `wlr-screencopy` and
  `wlr-foreign-toplevel-management`, with XML in `protocols/`.
- **`tearing-control-v1`.** The protocol *and* the flip: Smithay's DRM backend
  had no way to ask for an immediate page flip either, which is why the fork
  exists. See below.
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

Added by the rewrite, and not in the C build: `screencast.pick` and
`screencast.pick.done` outbound, `screencast.rect` inbound, and a `frame` field
on `view.layout`. All four exist because the shell is one buffer *under* the
windows, so anything it draws that overlaps a client is covered by it — the
compositor draws the named rectangles a second time, in front. See
`docs/ipc.md`.

### Wayland protocols

Beyond what Smithay hands over: `wlr-screencopy`, `wlr-output-management`,
`wlr-output-power-management`, `wlr-gamma-control`,
`wlr-foreign-toplevel-management`, `tearing-control-v1`, `ext-data-control`,
`ext-image-capture-source` and `ext-image-copy-capture`, `cursor-shape`,
`content-type`, `alpha-modifier`, `pointer-gestures`,
`keyboard-shortcuts-inhibit`, `text-input-v3` with `input-method-v2` and
`virtual-keyboard-v1`, `tablet-v2`, `linux-drm-syncobj-v1`, `xdg-dialog-v1`,
and `color-management-v1`.

Also implemented, from what Smithay ships: `xdg-system-bell`,
`xdg-toplevel-tag` and `wp-pointer-warp`.

**`wp-fifo` and `wp-commit-timing` are back, and this time they work.** Both
block a client's commit until the compositor releases it, and the first attempt
advertised them and froze every client that used them. Three things were wrong,
and only the first was the one the removal commit predicted:

1. **Nothing re-examined the blocked commit.** `Barrier::signal` sets a flag; the
   commit it was holding sits in a queue that is only looked at again when the
   compositor calls `CompositorClientState::blocker_cleared`. Without that call
   the client commits for ever and the compositor applies none of them — which
   from the outside is a window frozen on its first frame while the client is
   busy and healthy. `WAYLAND_DEBUG=1` on the client is what showed it: a steady
   stream of `set_barrier`, `wait_barrier`, `commit`, and nothing coming back.
2. **The deadline was measured against the wrong clock.** A commit timer's
   timestamp is CLOCK_MONOTONIC; `release_frame_barriers` was handing it time
   since the compositor started, which is smaller by the machine's uptime, so
   every deadline was in the future and every timed commit blocked for ever.
3. **The clock stopped when the drawing did**, which is the one that was
   foreseen. `arm_barrier_tick` runs a timer at the refresh interval while any
   surface is using either protocol, so a barrier is released even on a frame
   that has nothing to draw. It stops after a second of releasing nothing, and a
   commit starts it again — an idle desktop with no such client ticks not at
   all.

Verified with `vkcube`, which uses both through Mesa's Vulkan WSI: 1170 commits
applied and 600 barriers released in ten seconds, against one commit and a
frozen cube before. `MESA_VK_WSI_PRESENT_MODE=immediate` is what proves the
protocols are the difference rather than something else about the client.

`VIEWPORT_FIFO=0` withdraws both globals without a rebuild, because these two
have frozen every client that used them once already.

Still not implemented, all present in Smithay: `ext-workspace` — external bars
cannot see the workspaces, which are the shell's and are not published —
`drm-lease`, `security-context`, `xdg-toplevel-icon`, `xdg-foreign` and
`xwayland-keyboard-grab`.

## Why smithay is a fork

`crates/*/Cargo.toml` point at github.com/codebam/smithay rather than upstream,
for three patches: `DrmCompositor::set_allow_tearing`, a hook that lets the
compositor see keys from a virtual keyboard, and one line of errno in the
drm-lease pre-flight.

The second is `VirtualKeyboardKeyFilter`, on branch `virtual-keyboard-filter`.
Keys from `zwp_virtual_keyboard_v1` went straight to the focused client, so
nothing the compositor does with a key applied to them: `wtype hello` typed and
`wtype -M logo -k Return` did nothing at all, and the screen-share chooser could
only be answered by hand. wlroots delivers the same events as a keyboard on the
seat, which is the behaviour anything driving a session by script expects. The
hook offers each key to the compositor first, with the keysym resolved against
the virtual keyboard's own keymap — the client picked the keycode out of a
keymap it uploaded, and against the seat's it would mean another key — and both
the modified symbol and the one at level 0, because a chord is written
"Mod4+Shift+q" and the key part of it is `q`. The default keeps nothing, so a
compositor that does not implement it sees no change. Also upstreamable.

The third is smaller: `drm_lease`'s pre-flight opens the primary node and drops
master, tolerating EINVAL as "this fd never had master". On kernel 7.1 with
amdgpu that case answers EACCES instead — permission checked before state — so
no lease global was created at all here. Measured by calling DROP_MASTER on a
fresh fd directly. A fd that really is master still drops it, so tolerating
EACCES hides nothing.

Tearing is an asynchronous page flip — the frame lands as soon as the hardware
takes it rather than at the next vblank — and the flag for it lives inside the
atomic commit smithay builds. Nothing upstream reaches it: `FrameFlags` decides
which planes may scan out, and `queue_frame` takes no commit flags. Without the
patch, tearing-control-v1 is a protocol the compositor can advertise and never
honour, which is worse than not advertising it.

The patch gates on the driver capability and returns whether the request will
be honoured, because a commit carrying a flag the driver does not know is
refused outright — that would stop the output rather than tear it. It is
otherwise upstreamable, and the fork should go away when it lands.

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
7. **Done.** The layout watchdog, and system statistics for the bar.
8. **Done.** Pointer capture, the foreign toplevel list, and notifications.
9. **Done.** The small protocols a desktop assumes are there: primary
   selection, clipboard managers, idle inhibit and idle notify, viewporter,
   presentation time, single-pixel buffers, fractional scale — and the X11
   half of the clipboard.
10. **Written by hand, because Smithay implements none of them.** Done:
    `zwlr_screencopy_manager_v1`, and HDR's two connector properties —
    `Colorspace` and `HDR_OUTPUT_METADATA` — which its DRM backend does not
    expose, `zwlr_output_manager_v1`, `zwlr_gamma_control_v1`,
    `zwlr_foreign_toplevel_management_v1` and `tearing-control-v1`.
11. **Done. Ordinary ports.** text-input, tablet and gestures, and the
    appearance portal.
12. **Done. The screencast portal.** `org.freedesktop.impl.portal.ScreenCast`
    and a PipeWire stream, served from the compositor rather than left to
    xdg-desktop-portal-wlr — which can only offer monitors, because
    wlr-screencopy can only capture outputs. Offering a *window* is the whole
    reason to own the interface, and it is what niri and hyprland concluded
    too. Frames are composited straight into a DMA-BUF the consumer imports.
13. **Done. The screen-share chooser**, drawn by the shell and steered by the
    compositor.

### Still open

Nothing in the compositor's own behaviour, as far as this list knows. What is
left is protocols nobody here has needed yet:

`ext-workspace` is done, and was the interesting one: the compositor did not
know what the workspaces *were*. They are the shell's, so the IPC grew a
message in each direction — `workspace.list` in, `workspace.request` out — and
`crates/viewport/src/workspace.rs` relays between that and the protocol. The
shell remains the only thing that decides what a workspace is or which one is
showing. `cargo run -p viewport --example workspaces` is what a bar sees.

`drm-lease` is done too, with the caveat that nobody here has a headset: a
connector marked `non-desktop` is not driven as a monitor but offered for
lease, and a client that takes one gets the connector, a free CRTC and that
CRTC's primary plane. `cargo run -p viewport --example leases` shows what a VR
runtime sees — on a machine with no headset, a device that hands over a DRM fd
and offers nothing, which is the right answer rather than a failure. The lease
*request* path has never run against real hardware and is the part to distrust.

Done since, all four by delegation to what Smithay ships:
`xdg-toplevel-icon` — a window says what it looks like in a list, and the icon
name reaches the shell as an optional `icon` on `view.props`, so the taskbar
and the overview can draw one; `security-context` — a client that connected
through a sandbox's socket is tagged with what the sandbox said, and a
sandboxed client cannot create sandboxes of its own; `xdg-foreign` — a surface
one client can hand to another by name, so a portal's dialog is parented to
the window that asked; and `xwayland-keyboard-grab` — an X11 client that wants
every key, which is games and virtual machines. That last global is only
advertised to Xwayland (Smithay's `can_view`), so it does not appear in
`wayland-info` and its absence there is not a fault.

Three things this list called open have been done, and are noted because the
list said otherwise for long enough to be worth contradicting on the record:
the shell **does** receive input (`shell_pointer_motion`, `shell_pointer_button`
and `shell_pointer_axis` in `input.rs`, and keys through `Action::Web`, all of
which become real `WPEEvent`s in `crates/viewport-web/shim/viewport-shim.c`); a
screencast **does** renegotiate; and Mod4 with the left button **does** move a
tiled window, by floating it where it already is, which is what sway does.

Output rotation works, on all eight transforms, verified on the panel rather
than only in a capture. It did not for a long time, and the way the tests said
otherwise is worth reading before trusting a test in this area:
`docs/ROTATION.md`.

A screencast renegotiates now — `Stream::renegotiate`, with a settle of 250ms
so that dragging a window's edge does not allocate three screens' worth of
buffers per frame of the drag.

Keys from a virtual keyboard reach the compositor. `zwp_virtual_keyboard_v1`
sends them straight to the focused client, so `wtype` could type and could not
work a binding or answer the screen-share chooser; the fork carries a hook
(`VirtualKeyboardKeyFilter`) that offers each key to the compositor first, with
the keysym resolved against the virtual keyboard's own keymap.

## Parity, and when the C tree goes

"Every Rust module lands with the C file it replaces named in its header
comment. They get deleted once parity lands" is above, and until now *parity*
was not defined anywhere. Here is the definition, chosen because it is the one
that can be run rather than argued about.

**The C integration tests are the specification.** `tests/capture.test.sh`,
`tests/lock.test.sh` and `tests/output-order.test.sh` each take the compositor
binary as their first argument. They start it `--headless`, drive it over the
control socket with real clients, and check what came back — they do not care
which language wrote it. Point them at the Rust binary and the answer is a
number.

Making that possible needed one change: the scripts find the socket by grepping
`WAYLAND_DISPLAY=` out of the startup log, because libwayland picks the name
and there is no way to ask for a particular one. `src/main.c:304` prints it in
that shape and the Rust build printed only the bare name, so every one of these
tests failed at "the compositor did not come up" without ever reaching what it
meant to test. `crates/viewport/src/main.rs` now prints the same shape.

The three JS tests (`shell-*`, `kiosk`) are not part of this. They exercise
`data/shell/`, which is the product and outlives both compositors.

### Where it actually stands

Run against both binaries on one machine, 2026-07-30 — an AMD workstation, no
display attached to the headless runs:

| test | C | Rust |
|---|---|---|
| `capture-tiling` | pass | **fail** |
| `output-order` | pass | **fail** |
| `session-lock-crash` | fail | fail |

`session-lock-crash` fails for both and is not evidence of anything: it wants a
renderer that a headless run on this machine does not give it. CI runs it
against lavapipe, which is where its result means something.

The other two are real, and they are different in kind.

**`capture-tiling` is a decision, not a gap.** The test client binds
`ext_foreign_toplevel_image_capture_source_manager_v1` and the Rust build does
not advertise it. That is deliberate and already written down at
`crates/viewport/src/handlers/mod.rs:255`: a source is a thing that can be
captured, this compositor offers outputs, and a window captured apart from the
desktop loses the frame the shell drew around it — which is not what the picker
showed the user. Windows are offered through the screencast portal instead,
which composites them with their frames.

So this test encodes a C-era answer that the rewrite reversed on purpose.
Before `src/` goes, one of two things has to happen: the rewrite advertises the
global (Smithay ships `ToplevelCaptureSourceState`, so it is small) and the
decision above is withdrawn, or the test is rewritten to capture through the
portal. It cannot just stay red.

**`output-order` is a gap, and the error message is wrong about it.**
`output.test_add` is rejected with "headless hotplug is only available under
`--headless`" — from `crates/viewport/src/apply.rs:215`, which rejects it
unconditionally, including under `--headless`. `crates/viewport/src/headless.rs`
opens by saying `output.test_add` is what the headless backend exists for. One
of those two files is lying and it is `apply.rs`.

Without it there is no way to hotplug a second headless output, so nothing
tests the enumeration order that `output.layout` hands the shell — the order
the bar's fallbacks read as "leftmost display". That order has been wrong
before.

### The checklist

`src/` and `include/` can be deleted when, and not before:

1. `capture.test.sh`, `lock.test.sh` and `output-order.test.sh` pass against
   the Rust binary — or a test has been rewritten, with the reason recorded
   here, as `capture-tiling` will need.
2. `meson.build` no longer needs to build a compositor: the `shell-*`, `kiosk`
   and `unit` targets stay, and the C sources they compile against are gone or
   moved.
3. The sanitizer job has an equivalent. ASan over the C compositor is what
   caught the output-outlives-its-surface family of bugs more than once;
   Rust rules out the use-after-free but not the logic that led to it, so what
   replaces that job is a question to answer rather than a box to tick.
4. `.github/workflows/ci.yml` no longer has a job gated on `COMPOSITOR_CI`
   because it needs wlroots.

## Notifications, and where they come from

Nothing on a Linux desktop sends a notification to the compositor: they go over
D-Bus to whichever program has claimed `org.freedesktop.Notifications`. This
compositor claims it and forwards each one to the shell, so a notification is
part of the desktop rather than a separate client floating over it, and its
styling is the stylesheet already open in the editor.

That service runs on a thread of its own with a channel back into the event
loop. zbus wants an async runtime and this loop is GLib with calloop nested
inside it; making three schedulers agree is worse than one channel.

Failing to claim the name is not fatal. A session where mako or dunst already
holds it still has a working compositor — it just has no notifications, which
is what it had a moment earlier.

## The window list

`ext_foreign_toplevel_list_v1` is implemented: a taskbar or switcher written as
an ordinary client can see every window, its title and its app id, and hears
when one closes.

`zwlr_foreign_toplevel_management_v1` is not, and that is the one most existing
taskbars use — it carries activate, close and fullscreen requests as well as
the list. Smithay has no implementation, so it means writing the protocol
dispatch by hand rather than wiring one up.

## What the config file does, and does not

`~/.config/viewport/config.json`, or `--config`. Applied: `url`, `terminal`,
`menu`, `layout`, `logo`, `tutorial`, `bar`, `rules`, `theme`, `binds`,
`binds_override`, `keyboard`, `cursor`, `outputs`, `startup`.

Also applied: `fallback` and `timeout_ms` (the deadline is on the first
painted frame, not on the load event — a page that loads and then stalls is
invisible to load-failed), `idle`, `adaptive_sync`, `vt_switching`,
`decorations`.

`dark_mode` too, and it is the config key with the most machinery behind it:
acting on it means running the `org.freedesktop.impl.portal.Settings` D-Bus
service, which is how a GTK or Qt application learns the colour scheme. The
value alone changes nothing — see `appearance.rs`. `Mod4+Shift+d` toggles it,
and running applications switch on the portal's `SettingChanged` signal rather
than at next start.

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
- **No client-driven move or resize grabs.** A client asking the compositor to
  move or resize it has asked the wrong party — the frame is DOM and dragging
  an edge is the browser resizing a flex container. Those requests are ignored,
  not implemented. Mod4 with a button *is* implemented, and works the other way
  round: the compositor follows the pointer and sends the shell a delta, which
  the shell resolves against whatever layout the window is in.
- **Per-window opacity is stored but not applied.** Applied since step 3.
- Notifications, keybindings, config parsing and HDR answer with an `error`
  naming what is missing rather than failing silently. All four are implemented
  now; the last two points are kept for the record of what step 2 was.
