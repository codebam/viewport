# Debugging

## Debugging from inside

Both capture protocols are implemented — `ext-image-copy-capture-v1` and
`zwlr-screencopy-v1` — so `grim` works as an ordinary client:

```sh
grim ~/shot.png          # from a terminal inside the session
```

This is load-bearing rather than a nicety. Once the compositor is driving a TTY
there is no outer compositor to screenshot it from, so without this there is no
way to see what the desktop looks like, and debugging a *visual* shell becomes
guesswork.

Pair it with `--debug`, which mirrors the page console into the compositor log
and disables WebKit's cache:

```
CONSOLE INFO  outputs [object Object]
CONSOLE ERROR TypeError: ...        # a shell exception, otherwise silent
```

### Pointer capture

Games need two things a desktop pointer does not provide, and they only work
together:

| Protocol | Why |
| --- | --- |
| `zwp_relative_pointer_v1` | mouselook is driven by how far the mouse moved, not where the cursor landed — an absolute position saturates at the screen edge, so a game without it can only turn so far |
| `zwp_pointer_constraints_v1` | the cursor must stop moving, so it neither escapes onto the other monitor mid-fight nor feeds the client absolute motion it is no longer expecting |

While a lock is active the compositor stops moving its cursor entirely and the
client is driven purely by deltas. Unaccelerated values are forwarded
separately so a game can apply its own sensitivity. A constraint belongs to a
surface, so it lapses when that surface loses the pointer — otherwise a game
would keep the mouse captured after you focused something else.

This covers X11 games too. `XGrabPointer` has no direct equivalent on Wayland;
Xwayland implements it by taking out these same two protocols on the client's
behalf, so a native game and CS2 under Proton reach identical code.

### XWayland

X11 clients are tiled like any other window. The difference is confined to
`src/view.c`: an X11 window is not an `xdg_toplevel`, so title, class, sizing,
activation and closing go through accessors, and everything above — tiling,
workspaces, focus, the IPC view model — is unaware of which kind it has.

Three things about X11 shape that file:

- a surface exists before it has a `wl_surface`, so mapping is two-stage and
  the scene tree is built on `associate` rather than at creation;
- position and size are a single operation in absolute screen coordinates, so
  placement is repeated on every resize;
- override-redirect windows — X11 menus and tooltips — bypass the window
  manager entirely. They are never tiled and appear exactly where the client
  asks, on the overlay layer. Tiling them would break every X11 menu.

`DISPLAY` is exported the moment the X11 socket exists, not when Xwayland
reports itself ready. Starting lazily only defers *running* the server until
something connects; `ready` does not fire until that has happened, so setting
`DISPLAY` there means the first client to launch finds none — it fails, and the
second launch works because by then something else has woken Xwayland.

Override-redirect surfaces that ask for focus are given the keyboard. An X11
menu takes focus and closes when it loses it, so left unfocused it sits there
inert — the menu appears and nothing in it can be clicked. Only surfaces that
ask are given it, since a tooltip must not steal the keyboard, and focus goes
back to the previously focused window when the menu closes. They also follow
their own `set_geometry`, because a submenu moves itself beside its parent
rather than opening where it was first placed.

Xwayland starts lazily: a session that never runs an X11 client pays nothing.

Touchpad gestures are split by finger count: three fingers belong to the
compositor (swipe to scroll the strip or change workspace), everything else is
forwarded to the focused client through `pointer-gestures-v1`, so pinch-to-zoom
in a browser keeps working. A gesture that starts as the compositor's stays
that way until it ends.

Client scale is reported over both `fractional-scale-v1` and the `wl_surface`
preferred buffer scale, taken from the largest scale among the outputs in the
layout — a client renders at whatever it is told and the compositor stretches
whatever it gets, so saying nothing means everything paints at 1x and is scaled
up. Whole-number scales are rounded up, so a client that only understands those
overshoots rather than blurs.

## When the shell breaks

The entire layout lives in a web page, which is the point of this compositor
and also its one structural risk: a JavaScript error or an unreachable shell
means no window is ever placed, and the session becomes a black screen with a
working keyboard.

So placement is watched. A window that maps and is not given a rect within two
and a half seconds is laid out by a deliberately stupid built-in tiler — equal
columns across the output, ignoring workspaces and the tiling tree, since the
thing that maintains them is what stopped responding. It is not a layout anyone
would choose; it exists so a broken shell leaves a desktop usable enough to open
a terminal and fix it. The moment the shell does answer, the watchdog is
disarmed for that window, so a merely slow shell costs nothing.

Not yet implemented: tablet and stylus input.

## Fallback

The load deadline is on the **first painted frame**, not on the load event. A
server that accepts the connection and then stalls, and a shell whose JS throws
before rendering, both leave the user at a black screen and both are invisible
to `load-failed`. On timeout the compositor loads `fallback.html`, which speaks
the same IPC — windows opened while offline are still tiled, listed and
closable.


## Running it in a virtual machine

It does not work in a plain one, and the reasons are worth knowing before
somebody spends an evening on it. Tested against a QEMU guest running current
Arch, with the package from `packaging/arch/smithay`.

**Software Vulkan cannot drive it.** `vulkan-swrast` (lavapipe) installs and
`vulkaninfo` is happy, but the renderer needs
`VK_EXT_image_drm_format_modifier` to hand buffers to KMS and lavapipe does not
implement it. The compositor says so and stops:

    llvmpipe (LLVM 22.1.8, 256 bits) is missing VK_EXT_image_drm_format_modifier

**Accelerated Vulkan through virtio-gpu aborts.** With
`-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=2G` and `vulkan-virtio` in
the guest, the guest sees the host's GPU through Venus, the compositor gets as
far as setting a mode on `Virtual-1` — and then the *driver* calls `abort()`
five frames deep inside `libvulkan_virtio.so`, with nothing printed. That is
not this compositor failing a check; it is the driver ending the process.

Two smaller things the same guest showed, both fixed:

- A virtual machine usually has two DRM devices — the firmware's VGA and the
  virtio-gpu — and the seat calls the VGA primary. Scanning out on one device
  while drawing on another is a black screen at best, so the card is chosen by
  whether a Vulkan device claims it before falling back to what the seat says.
- Neither lavapipe nor Venus exposes a DRM node at all, so *no* card can match
  in a virtual machine. Refusing to start on that basis is wrong — the renderer
  imports buffers through `VK_EXT_external_memory_dma_buf` and does not need to
  own the display — so a mismatch is now a warning rather than an exit.

**It runs on OpenGL instead.** virgl gives a guest hardware GL through the GBM
platform, which is what the nested backend has always drawn with, so the DRM
path takes it when Vulkan cannot serve the display. A plain
`-device virtio-gpu-gl-pci` guest — no Venus, no `vulkan-virtio` — brings up a
desktop, and a terminal in it composites and reads back:

    qemu-system-x86_64 -enable-kvm -m 4096 -smp 4 -cpu host \
      -drive file=arch.qcow2,if=virtio \
      -device virtio-gpu-gl-pci -display egl-headless

`VIEWPORT_RENDERER=gles` or `--renderer gles` asks for it outright, and
`VIEWPORT_RENDERER=vulkan` refuses the fallback so that a machine which should
be using Vulkan says why it is not.

## Render churn, and why the throttle is off

The compositor attempts far more renders a second than the screen can show, and
most find nothing to draw. Counted on a 240Hz machine with a video playing:
Firefox commits **17,232 times a second**, which becomes 23,432 output-dirty
marks and 7,843 render passes, against 240 vblanks, at around 60% of a core.

An earlier note here blamed the compositor's own `render_if_needed`, on the
strength of a counter that sat above that function's early return and so
counted calls that did nothing. Calls are cheap; the passes are not. The churn
is client commits, one render each.

**Not fixed.** Rendering once a frame instead of once per commit measures far
cheaper — 81% of a core down to 9% with a video playing — and is the right
shape, but it cannot be turned on yet, for a reason worth writing down. The outer loop
now only arms the frame clock; the clock does one `render_if_needed` for
everything that arrived since the last tick. Same 17,232 commits, ~240 renders,
and the compositor went from 87% of a core to 10% with the video still smooth.
This is what wlroots does, and why sway answers the same traffic without the
heat.

The obstacle was the GLib bridge, and it is gone. GLib owns the blocking wait
and watches calloop's epoll fd as a single source, so a calloop timer only fires
if GLib comes back round to let calloop dispatch it — and calloop keeps its
timers in a wheel it consults while *it* is the one waiting. A wheel entry is
not an fd, so it is invisible to the poll that is actually blocking. `prepare`
setting `*timeout = -1` was not the cause, only the thing that made it obvious.

The frame clock is now armed on a **timerfd**, watched as an ordinary calloop
`Generic` source. An expiring timerfd makes the epoll fd readable like any other
event, so GLib wakes for it. `crates/viewport/tests/frame_clock.rs` runs the
experiment both ways and is the record: a timerfd wakes an outer poll, a calloop
`Timer` does not.

Ways round it that were tried before that and did not work, so they are not
tried again:

- Hand GLib the clock's deadline as the poll timeout. The clock ticked — 199
  times a second with the browser stopped — and the desktop still froze, which
  never made sense at the time and still doesn't; the timerfd made the question
  moot.
- Return readiness from `prepare` when the frame is due. Broke input outright.
- Report the same from `check`. Also broke input.

This also fixes a freeze that had nothing to do with the render pacing, and was
the reason the clock mattered before the pacing change ever landed. Frame
callbacks are throttled to one per surface per refresh. A client that commits
just after a callback went out is refused the next one, and the only thing that
was going to come back and offer it again was the clock tick — which never
arrived. That is a terminal that stops updating until the mouse moves or another
window happens to wake the loop. Two smaller pieces go with it:

- `prepare` and `dispatch` no longer arm the clock on every pass. With a tick
  that actually fires, arming there re-arms the clock immediately after each of
  its own ticks, and it never stops. Commits arm it, which is where the work
  comes from.
- A tick remembers whether one was asked for while it was pending
  (`frame_pending`) and arms one more, so the surface whose invitation this tick
  was too early to send gets it on the next one rather than never.

Whatever touches this has to be verified with **every other client closed**, not
just with a video playing.

Four earlier attempts cost more than they saved, all by trying to do less work
per commit rather than fewer renders. `VIEWPORT_COALESCE`, the env var that
switched the first of them on for A/B testing, is gone with them. Recorded so they are not tried again in
the same form:

- Holding attempts to one a frame, anchored to the last attempt. Merges any two
  commits sharing a window, which halves a client already at panel rate.
- Skipping commits that attached no buffer and asked for no damage, read after
  `on_commit_buffer_handler` has already taken both. Every paint looks empty
  and the client freezes on one frame.
- The same read before that call, and again over the whole surface tree to
  catch sync subsurfaces. Still misses paints; video goes stale.
- Suppressing a second pass on an output that just found nothing, until the
  frame clock clears it. Correct only if something always comes back for the
  output — and with no client committing, nothing armed the clock, so the
  screen stayed dirty and never drew.

What is genuinely fixed is that frame callbacks no longer depend on a render
happening. They have their own clock (`arm_frame_clock`), which ticks at the
refresh rate while clients are committing and stops when they stop. Without it
any damage-driven rendering deadlocks: no damage, no render, no callback, and a
client that paints only when invited never paints again.

The lesson for anything that changes pacing: CPU and delivered frames move in
opposite directions, and only one of them is visible to a person. Count frames
actually submitted per output rather than renders avoided, and have someone
watch a video. A "renders finding nothing: 7,316/s → 73/s" line looks like a
total win while real frames are going in the bin with the wasted ones.

What a guest gives up: colour management and HDR, which are the Vulkan
renderer's; DMA-BUF screen sharing, which takes the shared-memory path instead;
and the copy of the shell's frame, without which WebKit's next paint can land
in the buffer being sampled and the shell can flicker. All three are warned
about once and none of them stop a session.
