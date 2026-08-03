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

## A page that draws one frame and stops

The symptom is a desktop that loads, appears, and then never changes again —
input still works, the compositor is not wedged, and the log says nothing. Two
things can produce it, and they are told apart from the shell process's own
Wayland traffic rather than from the compositor's log.

`VIEWPORT_SHELL_WAYLAND_DEBUG=1` sets `WAYLAND_DEBUG=1` on the shell process
only — the compositor's own client traffic, which on the nested backend is
substantial, stays out of the way. What to look for after the first
`wl_surface.commit`:

* **no `wl_callback.done`** — nothing is inviting the page to draw. It is not in
  the `Space` and not in a layer map, so every path that sends frame callbacks
  has to name it: `udev.rs` in both `on_vblank` and the render pass, `winit.rs`
  after submit, `headless.rs` on its timer, and `state.rs`'s frame clock.

* **`wl_callback.done` but no `wp_presentation_feedback.presented`** — the page
  is being invited and cannot use the invitation. GTK4 presents through Mesa's
  Vulkan WSI, which will not acquire the next swapchain image until the last one
  is reported presented, so a compositor that advertises `wp_presentation` and
  never answers freezes it on frame one. Both backends answer now; a new backend
  has to.

Neither is what a *third* cause looks like. Running the compositor from
`nix develop .#rust` puts that flake's Mesa in `__EGL_VENDOR_LIBRARY_DIRS`,
which the shell process inherits — and nixpkgs' WebKit against a different Mesa
segfaults in `driQueryOptionb` while creating its GL context, before it has
rendered anything. The window then shows GTK's empty first frame and stops. The
tell is `WebKitWebProcess` in `coredumpctl`, and the fix is to leave the system
driver in place for the child:

```sh
nix develop .#rust --command bash -c \
  'unset __EGL_VENDOR_LIBRARY_DIRS
   export LD_LIBRARY_PATH=/run/opengl-driver/lib:$LD_LIBRARY_PATH
   exec ./target/debug/viewport --url https://example.com'
```

`VIEWPORT_SHELL_RATE=1` is the quick check for all three: it logs what the shell
painted every second, including the zeroes. One frame and then `0.0 frames/s`
for ever is this section; a rate that is merely low is `docs/benchmarks.md`.

## When the shell dies outright

The watchdog above covers a shell that is slow or wrong. A different thing
happens when WebKit's web process is killed — by a crash, or by the OOM killer
— and it is worse, because it does not look like a failure at all. WebKit leaves
the view blank and stops painting, so the last frame stays on screen for ever:
the bar is still there, the wallpaper is still there, and nothing redraws and no
click does anything. On a desktop whose entire interface is that one page, that
is indistinguishable from a compositor that has hung.

The web process is not the compositor's process, so this is survivable. It just
needed someone to act on it, and nothing was listening to
`web-process-terminated`. Now the compositor is: the frames in flight are
dropped, because their buffers belonged to the process that just died, and the
page is loaded again on the next pass through the event loop. Loaded rather than
reloaded — a process that died *during* the initial load has nothing to reload,
and a shell that throws on startup is exactly the case that produces. The
`WebKitWebView` and every signal on it survive; only the process behind it is
replaced.

The last painted frame is deliberately left on screen while the reload runs. It
is the compositor's own copy rather than WebKit's memory, so keeping it is safe,
and a transient crash then costs a second of a stale bar instead of a black
screen. It is dropped only when recovery is abandoned, where a frozen picture
would be a lie about the state of the desktop.

Five restarts inside a minute is the budget, and both halves of that are load-
bearing. A plain restart limit is wrong in either direction: a machine up for a
week that has crashed five times over that week is healthy, and one that crashes
five times in five seconds is a page that cannot load and must not be retried
for ever — each attempt spawns a process. The window is what tells the two
apart. When it is used up the compositor stops trying and says so; the desktop
is gone either way, and what stopping preserves is a machine that can still be
logged into and read the log.

A termination the API asked for is not a crash and is not restarted. Something
wanted that process gone, and reloading would fight it.

The restarted page knows nothing — it has painted nothing, said nothing, and has
no idea what the layout is. Everything the compositor derived from the old one
goes with it, including the recorded size, so the new process is told how big it
is; WebKit paints nothing into a view of no size, and a shell that is never told
would load and then sit there. The window list is rebuilt by the shell itself
through `view.query`, which is the same path a manual reload already used.

## More than one GPU

Every GPU on the seat is opened, and each drives the connectors wired to it —
so a monitor on the second card lights up rather than not existing. Each has its
own renderer and its own output manager, and draws its own outputs: a buffer is
only cheap on the device that allocated it, so the alternative is drawing
everything on the primary and copying each frame across PCIe.

The cost of that choice is that a client buffer allocated against the primary
has to be importable by the secondary, which is what a shared modifier is for.
Where it cannot be, that surface does not appear on that screen — the session
carries on. A GPU that cannot be opened at all is skipped with a warning, on the
same reasoning: one card failing is a monitor that stays dark, and refusing to
start is every monitor dark.

Outputs are addressed by `OutputId { device, crtc }` rather than by CRTC alone.
A `crtc::Handle` is only unique within the device that issued it, and two GPUs
routinely hand out the same value — keyed on the handle by itself, a vblank from
the second card redraws an output on the first.

Each surface is told which GPU to allocate against, per frame, from the device
displaying it. The `linux-dmabuf` global carries one default feedback naming the
primary — right with one GPU, wrong with two, because a window on the second
card would be told to allocate for the first while the renderer that has to
import it belongs to the second.

That matters with one GPU as well. A client rendering on another device —
anything under `prime-run`, or the discrete half of a hybrid machine — takes the
main device from this feedback and allocates something that device can import.
Getting it right is the difference between a buffer that imports and one the
client is told to allocate again.

The feedback carries two tranches. The main one names the GPU's render node and
everything its renderer can import, which is what makes a buffer usable at all.
The preferred one carries `Scanout` and the formats this output's *primary
plane* accepts, which is what lets a fullscreen buffer go straight to the
display controller instead of through a composite.

That second tranche is why the feedback is per output rather than per GPU: plane
formats belong to a CRTC, and a monitor on the same card can have a different
set from the one beside it.

The scanout formats are intersected with what the renderer can import. A format
the plane takes and the compositor cannot read is no use — the moment anything
overlaps that surface it has to be composited, and a buffer that cannot be
sampled has nowhere to go. Advertising it would trade a rare zero-copy frame for
a window that vanishes when a notification appears over it.

Still missing: any sharing of a rendered frame between devices. **Untested on
real multi-GPU hardware** — it was written on a machine with one GPU, where
every secondary path is unreachable.

Which GPU is primary — the one clients and the shell allocate against — is the
part that bites on a hybrid laptop. The candidates are ranked by whether a Vulkan device actually
exposes them and then by what the seat calls primary — and where both GPUs pass
that test, which one gets used came down to the order the seat listed them.
That is a preference (battery or frames), not something the hardware answers.

```
VIEWPORT_GPU=card1 viewport
```

names one. Matched as a substring of the device path, so `card1`, `renderD129`
or a whole `/dev/dri/by-path/pci-0000:01:00.0-card` all work — the by-path names
are the only ones stable across reboots. A value matching nothing is reported
and then ignored, rather than silently falling back: that combination is
indistinguishable from the variable not existing, which is the state this was
added from.

The startup log names every GPU it found and which it took whenever there is
more than one, so the wrong choice is visible without guessing.


## A GPU that crashes and comes back

An overclocked card that pushes too far does not fail politely. The kernel
notices a job that never finished and resets the engine, which takes a second or
several; if that does not take, the driver falls back to a bus reset, which
unregisters the DRM device and registers it again. From userspace the second one
is indistinguishable from the card being unplugged and plugged back in.

Neither should cost the session, and `crates/viewport/src/recovery.rs` is what
makes sure it does not. The failure it exists for is quieter than a crash: a page
flip is queued, the reset eats it, the vblank that would say "that frame is on
screen" never arrives, and the output is skipped from then on as having a frame
already in flight. The compositor keeps running, keeps taking input and keeps
answering the control socket, with a frozen screen and nothing in the log.

So every queued flip is timed. One that has not come back within 1.2 seconds —
or a commit that failed outright, which is the same thing without the waiting —
starts a ladder, each rung tried once and given 1.5 seconds to work before the
next:

1. **Resume.** Clear the stuck flip, throw away the damage history, draw again.
   The whole fix when the reset only ate the event.
2. **Reactivate.** Take DRM master again and put the mode back, exactly as a VT
   switch back does — after a reset nothing the compositor believes about the
   device's state is known to be true.
3. **Rebuild.** Build a new renderer on the same card. A Vulkan device that has
   taken `VK_ERROR_DEVICE_LOST` fails every submission from then on and cannot
   be revived, while the GBM device under it is still good.
4. **Reopen.** Close the card and open it again. The only thing that helps when
   the DRM device was unregistered, and the one rung that repeats — with a
   backoff up to 30 seconds, because a card behind a bus reset comes back on its
   own schedule.

A vblank at any point puts the device straight back to healthy, so a card that
recovers at the first rung never reaches the second.

Whole-device hotplug is handled alongside it, because it is the same event.
A card that goes takes its outputs, its `wl_output` globals and its vblank source
with it — the last of those mattering most: a source watching a revoked fd polls
ready forever, which is a compositor burning a core over a GPU that is not there.
A card that arrives is matched back to the slot it left by its **PCI address**
rather than its node path, since a card that returns while anything still holds
an fd on the old one takes the next free minor and comes back as `card2`. A GPU
this session has never seen is added at the end of the list.

Device indices never shift. `OutputId { device, crtc }` indexes the device list
and every vblank closure has captured one, so a slot whose card is gone is kept
and the same card comes back into it.

In the log, the whole sequence names itself:

```
gpu 0 (DrmNode(card1)) stopped responding: resuming the flip chain
gpu 0 (DrmNode(card1)) stopped responding: resetting the device
gpu 0 (DrmNode(card1)) is answering again
```

Three lines and no third rung is a soft reset that took two tries. A run that
reaches `reopening the card` and then `is up; bringing its outputs back` is a bus
reset. One that reaches `will not open` and stays there is a GPU that is
genuinely gone — the retry keeps going for as long as the session does, so
re-binding the driver by hand brings the monitors back without a restart.

To cause one on amdgpu, **read** the debugfs entry. It triggers on the read and
has no write handler at all, so the obvious `echo 1 >` fails with a permission
error even as root, which reads like a lockdown or a mount problem and is
neither:

```
cat /sys/kernel/debug/dri/N/amdgpu_gpu_recover
```

`N` is the card's DRM minor; `cat /sys/kernel/debug/dri/N/name` says which card
it is. For the bottom rung — reopen, and the matching of a returning card by its
PCI address — take the device off the bus instead:

```
echo 1 > /sys/bus/pci/devices/0000:03:00.0/remove
sleep 5
echo 1 > /sys/bus/pci/rescan
```

Do either from SSH rather than from a terminal inside the session being reset.
Not only because a bug here leaves no way back in, but because that terminal is
one of the things that will not survive.

**The clients do not come back, and cannot be made to from here.** A reset loses
every context on the device, so anything drawing through Vulkan or GL — a
terminal with GPU rendering, a browser, a player — has the device torn out from
under it and dies. Surviving that is the client's own job, through robustness
extensions it has to ask for and handle; a compositor cannot do it on their
behalf. What this is answerable for is being there afterwards to open a new one
in, with the desktop, the shell and the layout intact.


## A video player dying with "Invalid stride"

Under heavy load — twelve 4K streams at once — a player using the zero-copy
video path occasionally dies outright:

```
error 6: Invalid stride (4096) or height (2160) for plane 0.
[vo/dmabuf-wayland/wayland] Error occurred on the display fd
```

That is a `zwp_linux_buffer_params_v1` out-of-bounds error, and a protocol error
kills the client: libwayland aborts. The compositor is right to send it, and the
cause is not on this side.

The check is `offset + stride * height` against the size of the file the plane's
descriptor refers to. Instrumenting it and reproducing on real hardware — 909
samples, three rejections — gives the answer directly:

```
pass:   link=/dmabuf:3945763-mpv   st_size=15728640
reject: link=/memfd:mpv (deleted)  st_size=94848  mode=0o100777
```

The rejected descriptor is not a DMA-BUF at all. It is a memfd belonging to the
player, a regular file of about 95 KB, where a 15 MB buffer should be. The
parameters around it are identical to the ones that pass — same offset, same
stride, same height, same modifier — which is why the wire log looks innocent:
it records the numbers, not what a descriptor points to.

So the client sends the wrong descriptor for plane 0, rarely and under load, and
the compositor refuses a buffer whose first plane is a small regular file. Both
halves are behaving correctly. Report it to the player.

Worth knowing for anything similar: nothing about this is visible from the
protocol trace, and the compositor's own log only says a buffer was refused.
`readlink /proc/self/fd/N` at the point of the check is what separates "the
measurement is wrong" from "the descriptor is wrong", and those two have
opposite culprits.


## Fallback

The load deadline is on the **first painted frame**, not on the load event. A
server that accepts the connection and then stalls, and a shell whose JS throws
before rendering, both leave the user at a black screen and both are invisible
to `load-failed`. On timeout the compositor loads `fallback.html`, which speaks
the same IPC — windows opened while offline are still tiled, listed and
closable.


## Running it in a virtual machine

It does work now, and `nix run .#vm` is the shortest way to see it: a NixOS
guest that boots straight into the compositor on a virtio-gpu, with the log
teed into the directory QEMU shares with the host. `scripts/arch-vm.sh` does
the same against an Arch cloud image with a built package installed into it.
What follows is what had to be true first, because most of it was not.

**Software Vulkan cannot drive it, and used to do so silently.** `vulkan-swrast`
(lavapipe) installs, `vulkaninfo` is happy, and the compositor would open it,
build a renderer and report success — because `for_node` falls back to any
Vulkan device when none of them owns the display's DRM node, which in a virtual
machine is always. The failure then surfaced somewhere else entirely, once per
output:

    Virtual-1: could not initialise: llvmpipe (LLVM 21.1.8, 256 bits) does not
    support DrmFourcc(AR24) with modifier 0xff

`0xff` there is `DRM_FORMAT_MOD_INVALID`, the implicit modifier a plane
advertises, and a software device cannot allocate for it. Every output failed
that way, the compositor came up with none, the shell was configured to `0x0`,
nothing was ever committed, and QEMU showed "Display output is not active" from
boot to shutdown. The renderer now asks for Vulkan on the display's own GPU
(`for_node_exactly`) and takes GLES when nothing owns it, which is the case
this paragraph describes.

A shell that traps a few seconds in — CEF took `SIGTRAP` at about thirteen
seconds, with no message — is worth suspecting of the same root cause before
anything else: a shell told its size is `0x0` has no good options.

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

`VIEWPORT_RENDERER=gles` or `--renderer gles` asks for it outright.
`VIEWPORT_RENDERER=vulkan` means Vulkan on whatever device there is, including
a software one that does not own the display — which is how to reproduce the
black screen above deliberately, and the only reason to want it.

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

Two smaller pieces go with it:

- `prepare` and `dispatch` no longer arm the clock on every pass. With a tick
  that actually fires, arming there re-arms the clock immediately after each of
  its own ticks, and it never stops. Commits arm it, which is where the work
  comes from.
- A tick remembers whether one was asked for while it was pending
  (`frame_pending`) and arms one more, so the surface whose invitation this tick
  was too early to send gets it on the next one rather than never.

## A terminal on an empty workspace shows nothing of what is typed

Fixed. The same root cause as above, in the other clock, and worth its own
section because it is the one a person actually notices.

Open one terminal on an otherwise empty workspace and type. Sometimes it works
and sometimes the text does not appear until a second window is on the screen,
or until the mouse moves. **The intermittency is the tell**, and it is what says
this is a wakeup problem rather than a rendering one.

rio paints through Mesa, and Mesa's Wayland WSI paces FIFO present mode with
`wp_fifo_v1` — `strings libvulkan_radeon.so` has `wp_fifo_v1` and
`wp_commit_timing` in it. So every frame rio draws commits with a barrier, and
the *next* commit is held until the compositor releases it. There are exactly
two things that release one: `on_vblank`, and `arm_barrier_tick`. A held commit
makes no damage, so there is no frame, so there is no vblank, so `on_vblank`
never runs — the tick is the only way out, and the tick was a `calloop::Timer`.

Which is invisible to GLib's poll, as above. It fired only when GLib woke for
some other reason: the shell is a WebKit page with a clock in it, so GLib does
wake, at whatever moment WebKit's own timers happen to fall. That is the
intermittency exactly — a barrier due in four milliseconds released after
however long it takes something unrelated to happen. Two windows, or a moving
mouse, mean the loop is being woken constantly and the tick is never late.

Both clocks are on timerfds now, one each. Not one shared fd: they are armed
from different places and fall due independently, so arming either on a shared
timer would move the other's deadline, and the loser would be the one nothing
else can rescue. `two_clocks_do_not_swallow_each_other` in
`tests/frame_clock.rs` is the guard.

Still on calloop timers, and so still late by however long GLib sleeps: the
one-second idle and lock check, the two-second status tick, the layout watchdog
and the pick timeout. All of them are slow enough that lateness is cosmetic
rather than a freeze, but a bar clock that stutters on an idle desktop is this,
not the shell.

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
