# Protocols and hardware support

## HDR

`Mod4+Shift+p` switches the monitor you are looking at into HDR, and back.
Per output, because a display that can do it usually sits next to one that
cannot, and global would mean choosing which of the two to get wrong.

Two halves make it work. The output is switched to BT.2020 primaries and the PQ
transfer function, at ten bits per channel — eight cannot carry a PQ curve
without visible banding in exactly the dark gradients this is for. On its own
that would make everything look *worse*, because every SDR window would be
interpreted as if it were HDR. The other half is `wp-color-management-v1`,
attached to the scene: clients say what their content actually is, and the
renderer converts rather than reinterprets. SDR windows keep looking like SDR
windows, and a client with real HDR content can say so.

Switching is a full reconfiguration, not a lone image description: changing the
colour space a connector drives is a modeset, and a state carrying only the new
colorimetry is refused outright — which is how a monitor that plainly does HDR
came back reporting that it would not take it. The mode is restated and
`allow_reconfiguration` is set, so the driver is allowed the brief disruption
the switch actually costs.

Three things must be true, and the failure says which is missing: the display
has to accept BT.2020 primaries, it has to accept the PQ transfer function, and
the renderer has to support output colour transforms. That last one is the
likeliest limit — wlroots' Vulkan renderer supports it and its GLES2 renderer
does not, which is another reason this compositor defaults to Vulkan. It is
reported at startup rather than left to be discovered by a keypress that does
nothing.

Ten bits is a preference rather than a requirement. Which deep formats a plane
accepts depends on the driver, the connector and what else is on screen, so
several are tried in order and the first the hardware accepts is committed —
falling back to the output's current depth if none are taken, because banded
HDR beats no HDR. Capability is read from the connector and every candidate is
tested before it is committed, so a display that will not take it is left showing what it was
showing rather than going dark. The bar shows `HDR` while an output is in it —
otherwise the picture changes and nothing says why, and a monitor left in HDR
by a mis-hit key looks like a broken colour profile rather than a setting.

Only the features this renderer actually implements are advertised: parametric
image descriptions, named primaries and named luminances. ICC profiles, power
curves and arbitrary chromaticities are refused rather than accepted and
ignored, and mastering metadata is accepted and dropped, because tone mapping
is not done here.

What an output says about itself follows what it is being driven as. An output
in HDR describes itself as BT.2020 with PQ, and its *target* volume — the pair
a client reads to decide whether the screen can go above reference white —
carries a maximum above it rather than equal to it. Both halves matter and
neither is decoration: Chrome asks the output for its image description once,
when it connects, and will not offer a page an HDR display without a target
maximum above reference white. Reporting the SDR numbers on a screen already in
HDR is why a 4K HDR video played its SDR rendition on a monitor that was in HDR
at the time.

The peak that maximum reports is 1000 cd/m² and it is not measured. The
connector's metadata blob carries zeroes for the mastering display, so there is
no panel figure to pass on, and some number above reference white has to be
sent or the answer reads as "no headroom". 1000 is what HDR10 is graded
against.

Switching an output notifies every client holding a
`wp_color_management_output_v1` for it, and every surface on it holding
feedback. Image descriptions are immutable, so the event only says *ask again* —
but without it a client keeps whatever it fetched at startup for as long as it
runs, and `Mod4+Shift+p` moves the screen into a colour space nothing on it
knows about.

The other direction is the surface's own description, and it reaches the shader
through `ImportDmaWl`/`ImportMemWl` rather than `ImportDma`: those are the only
import calls handed the surface the buffer came from, so they are the only
place a description recorded against a surface can be attached to the texture
made from its buffer. Recording it anywhere else means recording it and then
decoding the buffer as sRGB anyway.

## Hardware video

A video decoder does not produce colour. VA-API and NVDEC hand back NV12, P010
and their relatives — luma in one plane, the two colour-difference channels at
half resolution in another — and a compositor that can only import single-plane
RGB forces the player to convert every frame on the CPU before it can hand
anything over. That conversion is the largest avoidable cost in playing a video,
and it is paid sixty times a second.

So the multi-planar formats are imported directly: NV12, NV21, NV16, P010, P016,
YUV420, YVU420, YUV422 and YUV444. The pixels are never touched. Sampling them
as colour is `VkSamplerYcbcrConversion`'s job, done by the sampler as the shader
reads the texture, so the fragment shader is the same one every other surface
uses.

That conversion is not something a draw can choose. Vulkan requires the sampler
carrying it to be *immutable* in the descriptor set layout, which makes it part
of the pipeline layout and so part of the pipeline itself. A conversion
therefore brings its own set layout, pipeline layout and pipeline along with it,
and the conversion object is cached on the device rather than on either — the
image view and the pipeline's sampler have to name the same object, and a second
conversion built with identical parameters is a different object as far as
Vulkan is concerned.

Two things a DMA-BUF cannot carry have to be inferred. The matrix comes from the
picture's height — BT.601 at or below PAL's 576 active lines, BT.709 above it,
which is the rule every video stack uses — and the range is taken as narrow,
which is what broadcast and every hardware decoder default to. A full-range
buffer read as narrow comes out slightly washed out; the reverse clips. Chroma
siting is not guessed: it is whichever of the two the device says it can
reconstruct, preferring the one the MPEG family actually uses, and where chroma
cannot be filtered linearly the luma filter drops to nearest with it, because
Vulkan requires the two to agree.

How the planes are laid out is the exporter's choice and both are handled. A
decoder normally returns one allocation with the planes at different offsets;
some return one file descriptor per plane, which is a disjoint image and binds
one allocation per plane through `vkBindImageMemory2`. Which it is cannot be
decided by comparing the descriptors — two descriptors onto the same buffer are
two different numbers — so the DMA-BUF's inode is what identifies it.

A YUV image is never a render target and never claims a transfer usage. A copy
involving a multi-planar image names one plane aspect at a time, and every
transfer in this renderer covers the whole image, so claiming it would make a
screenshot of a video succeed and return a third of the picture. For the same
reason the YUV formats are filtered back out of what is offered to the web shell
and to capture clients: both allocate buffers to be copied *into*, and a shell
painted into a luma plane imports without complaint and looks like a greyscale
smear.

Where the device cannot sample YUV at all — the feature is core since Vulkan 1.1
and still optional there — it is not advertised, and the log says so at startup.
Enabling a feature a device does not have fails `vkCreateDevice` outright, which
would cost every machine without it a renderer to gain video import on the ones
with it.

## Notifications

The compositor claims `org.freedesktop.Notifications` itself and forwards each
one to the shell, which draws it as part of the desktop. So notification styling
is the stylesheet already open in the editor rather than a second configuration
language in a second program, and it live-reloads with everything else.

If mako or dunst is already running it keeps the bus name and this stands
aside — the log says so rather than failing.

Bodies are rendered as text, never as markup. A notification body is a string
from an arbitrary program and the shell is a web page; rendering it as HTML
would let anything that can send a notification run script in the desktop.
Capabilities are reported as only what the shell honours, since claiming more
has applications send content that is silently dropped.

Critical notifications never expire on their own, which the specification
requires and which applications rely on. Closing reports *why* — expired,
dismissed or withdrawn — because a daemon that never reports closure leaves
programs believing their notification is still on screen, and some wait for it
before sending the next.

## The system tray

There is no Wayland protocol for a tray and there is not going to be one.
An application that wants an icon registers itself with whichever program
holds `org.kde.StatusNotifierWatcher` and waits to be asked what it looks
like — the interface KDE wrote, GNOME adopted through an extension, and every
toolkit implements.

So the compositor claims the name, for the same reason it claims
`org.freedesktop.Notifications`: the shell is the desktop, and a tray drawn by
a separate bar would be a second program with a second configuration language
floating over a compositor that already knows where everything is.

Three names are involved and they are not the same thing. The *watcher* is the
registry, one per session, and it is the name applications look for. A *host*
is something that draws a tray, and it registers itself with the watcher so
that items know somebody is listening — several applications check that before
they will use a tray at all, and fall back to a window of their own when the
answer is no. This is both, so it claims `org.kde.StatusNotifierHost-<pid>` as
well.

Both names are claimed the way every other name here is: queued for, never
taken. A KDE session or a GNOME extension already drawing a tray knows more
about that desktop than this does, and when it exits the name comes here rather
than the session losing its tray. `"tray": false` in the configuration turns
the whole thing off — the names are released, applications see the tray
disappear exactly as they would if this program had exited, and the bar empties.
The setting applies on reload, not only at startup.

An item registers either a bus name or an object path, and both forms are in
use — Qt sends the name, Ayatana's library sends the path — so an
implementation that handles one of them has a tray that works for half the
desktop. Removal is mostly not announced at all: what arrives is the bus name
losing its owner, which is the only notice a crashing application gives.

**What reaches the shell is a picture, not a name.** An icon arrives as a theme
name, as raw ARGB pixmaps, or as both, and none of the three is something a
browser can draw — an icon name means nothing to it, and a `file://` path is
refused outright in a shell loaded over `http://`, which is how the shell is
developed. So the compositor resolves the name against the icon themes, or
encodes the pixmap, and sends a `data:` URL. `icon_theme` in the configuration
says which theme is searched before `hicolor`; `hicolor` is always searched,
because that is where a package installs an icon belonging to no theme.

The PNG written for a pixmap is not compressed. A tray icon is a couple of
kilobytes, it is encoded once and cached, and a deflate implementation is a
great deal of machinery — or a dependency carrying one — to save a couple of
kilobytes on a message sent when an application starts.

Every call an item answers happens on the tray's own thread. Fetching thirteen
properties from a program that has stopped answering the bus must not take the
desktop's frame loop with it, and the reply to an activation is not waited for
at all.

**Menus are not drawn yet.** An item that says `ItemIsMenu`, and a right click
on any item, is sent `ContextMenu`, which is what a tray item is told when the
user wants its menu — applications that implement it draw their own window and
those work. The ones that publish a `com.canonical.dbusmenu` object instead
have a menu this shell cannot yet render, and for those the primary click,
which most of them also handle, is what there is. See
[`docs/roadmap.md`](roadmap.md).

## Tablets

A stylus reports pressure, tilt, distance, its own buttons and whether it is
hovering or touching. `tablet-v2` carries all of it, and a drawing program that
gets only x and y has a pressure-sensitive brush reduced to one width of line.

The stylus also drives the cursor, which is what makes it work in the many
programs that only understand a pointer. Tablets are absolute devices, so the
cursor jumps to where the pen lands rather than moving relative to where it was,
and touching the tablet focuses what is under the pen — otherwise drawing in a
window would mean clicking it with a mouse first.

A tool's own cursor image is carried through as well. The pen and the mouse are
two devices sharing one visible cursor, so they keep separate images and the pen
wins while it is in proximity — an application asking for a crosshair means it
for the hand that is drawing, and has said nothing about what the mouse should
be. Lifting the pen away clears it and hands the cursor back, which is what
stops a crosshair being stranded under a pointer that has moved somewhere else.

## Touch

`wl_touch` carries the whole sequence — down, motion, up, frame and cancel — and
cancel matters as much as the rest: a touchscreen unplugged mid-gesture leaves
every client that was told about the contact waiting for an end that never
comes.

Dragging between clients works from a finger as well as from a pointer.
`data-device`'s drag request says which device started it, and a touch drag
that is refused looks to the application like a drag that simply did not take —
there is no error and nothing in the log. The touch grab differs from the
pointer one in a single respect: it is given no focus policy, because there is
no pointer left behind to decide about, so the grab settles focus itself.

## Idle

Advertising `idle-notify` is not a policy: with nothing listening, the screens
stay lit for ever and the session never locks. `"idle"` in the config is the
policy, so a machine left alone behaves without a daemon having to be installed
and configured alongside it. Set neither threshold and there is none, leaving
the field to swayidle or anything else.

The compositor runs the locker named in `lock_command` rather than locking the
screen itself. That is the whole point of `ext-session-lock`: the program
drawing the lock screen is separate, and can crash without unlocking anything.
Blanking turns the outputs off, which is what actually saves the panel.

Idle inhibitors are honoured, and only while their surface is mapped — a paused
video on a hidden workspace is not keeping anyone awake. An inhibitor counts as
activity rather than merely pausing the countdown, so releasing one starts the
clock again instead of firing immediately.

Key presses count as activity; releases do not. That is what makes blanking
from a keybinding possible at all — the chord fires on press, and letting go of
it would otherwise say someone is there and turn the screens straight back on.
A grace period would have to outlast however long the keys were held; not
counting the release avoids the question.

`Mod4+Shift+x` locks now and `Mod4+Shift+b` turns the screens off now — the
same two things the timer does, for when you are leaving rather than waiting to
be noticed leaving. The screens come back on the next keypress or mouse
movement, through the same path the timer's blanking uses.

`wlr-output-power-management-v1` is advertised too, so `wlopm` and settings
panels can turn a monitor off on request rather than only on a timer.

## What clients may ask for

`maximize` and `minimize` are declined, but *answered*: the protocol requires a
configure in response to the request, and a client that gets none waits for one
— GTK's own maximize button hangs the window rather than doing nothing.
`wm_capabilities` is set to fullscreen alone, so a client can stop drawing
buttons for things this compositor will not do. It is sent on the initial
commit rather than at creation, because the surface is not initialised before
then and wlroots asserts rather than ignoring a configure scheduled too early.

A client dragging itself by its own titlebar (`move`, `resize`) is honoured
only for floating windows. A tiled window's place belongs to the layout, and
letting a client pull itself out of it by holding its titlebar would be a
surprise. The drag runs through the same machinery as Mod4+drag, so there is
one implementation of "the pointer is moving a window".

## Keyboard shortcuts

A virtual machine, a nested compositor or a remote desktop client needs the
chords this compositor would otherwise swallow — `Mod4+Return` has to reach the
guest rather than opening a terminal here.
`keyboard-shortcuts-inhibit-v1` is how a client asks for that, and it applies
only while that client holds focus, so the bindings come back the moment focus
moves away.

## Status

The architecture is grounded in the real 2.52.5 headers: the `WPEDisplay` and
`WPEView` vfunc tables, `wpe_buffer_dma_buf_get_*`, `WebKitWebView`'s `display`
construct property, and wlroots 0.20's `wlr_buffer_impl` and
`wlr_scene_buffer_set_buffer_with_options`.

**Built and running on real hardware** — DRM/KMS on an RX 7900 XTX (radv, Mesa
26.1.5), dual 2560x1440, launched from a TTY via `./start.sh`. Confirmed
working:

- the web shell renders as the desktop; `xdg-shell` clients composite inside
  the frames it draws, one desktop per output
- keybindings: terminal, launcher, close, exit, shell reload
- `wmenu` and other layer-shell launchers, including keyboard focus
- server-side decorations, so clients drop their own titlebars
- VT switching away and back, with the outputs repainting on resume
- `grim` screenshots from inside the session
- clean shutdown: ~1s, no assertions

Earlier, nested inside sway:

- Vulkan renderer initialises; GBM allocator binds `/dev/dri/renderD128`
- `WPEDisplay` hands WebKit that same render node, and WebKit's frames arrive
  as `WPEBufferDMABuf` and composite through `wlr_scene` — the shell's HTML is
  visibly rendered as the desktop
- an `xdg-shell` client (`foot`) maps, is reported over IPC as
  `{"type":"view.added","id":1,"title":"foot","app_id":"foot"}`, and is
  composited inside the frame the shell drew for it
- the UNIX control socket serves `output.layout` with real mode data

Also verified: `wlr_scene` reports `Direct scan-out enabled` for the shell
buffer when no client overlays it, and keybindings parse and reject bad input
with precise errors.

### Protocols

Beyond `xdg-shell` and `linux-dmabuf`, several protocols turned out to be
load-bearing rather than optional, each for a non-obvious reason:

| Protocol | Why |
| --- | --- |
| `wlr-layer-shell` | every Wayland launcher is a layer-shell client |
| `xdg-activation` | `wmenu-run` *asserts* on its absence and aborts before drawing |
| `xdg-decoration` + KDE `server-decoration` | both are needed: GTK4 ignores the former and honours the latter, so a GTK client keeps its own titlebar unless you offer both |
| `ext-image-copy-capture` + `wlr-screencopy` | screenshots from inside the session — the only way to see the desktop once it owns the display |
| `virtual-keyboard` | `wtype`, accessibility tools, and injecting keystrokes in tests |
| `primary-selection` | middle-click paste |

## ext-workspace-v1

Implemented in `crates/viewport/src/workspace.rs`, and a relay rather than a
source: the workspaces belong to the shell, arrive over `workspace.list`, and a
client's request leaves as `workspace.request`. Wire bindings come from
`wayland-protocols`' staging set, so there is no XML here for it.

`ext_workspace_manager_v1.commit` is honoured as a batch — requests are held
against the manager that received them and forwarded in order — because a bar
that assigns a workspace and activates it in one commit means both.

Group membership is tracked per handle rather than set once at creation: a
workspace moves between monitors, and a client told only which group it was
made in draws it on the wrong screen for the rest of the session. It leaves the
old group before it enters the new one, since a handle in two groups at once is
a workspace a bar draws twice.

The shipped shell publishes its workspaces from `data/shell/outputs.js`; see
[Workspaces](ipc.md#workspaces) for the list it sends and for the waybar module
that reads it, which is `ext/workspaces` and not `sway/workspaces`.

## wp-drm-lease-v1

`crates/viewport/src/workspace.rs`'s neighbour, `crates/viewport/src/udev.rs`
plus the handler in `handlers/mod.rs`. A connector with the `non-desktop`
property is never made into an output: it is offered for lease, and a client
that takes one is given the connector, a CRTC that is not driving a desktop
output, and that CRTC's primary plane with a claim on it.

Smithay's pre-flight for the global opens the primary node and drops master,
tolerating EINVAL as "this fd never had master". On kernel 7.1 with amdgpu that
case answers EACCES instead, so the fork widens it — without that, no lease
global exists at all on this machine.

Untested against a headset. `cargo run -p viewport --example leases` shows what
a client sees: the DRM fd, and the connector list terminated with `done`.

## wp-fifo-v1 and wp-commit-timing-v1

`ViewportState::release_frame_barriers` and `arm_barrier_tick` in `state.rs`.
Both block a client's commit until the compositor releases it, which is why
they were withdrawn once: this compositor draws on damage, and a blocked commit
makes none.

Three things are needed to honour them, and missing any one of them freezes the
client on its first frame:

1. `CompositorClientState::blocker_cleared` after signalling. Signalling only
   sets a flag; nothing re-examines the held commit until the compositor says
   so.
2. A CLOCK_MONOTONIC deadline for the commit timer. Time since the compositor
   started is smaller by the machine's uptime, and every deadline stays in the
   future for ever.
3. A clock that runs while a barrier is outstanding, so a barrier is released
   even on a frame with nothing to draw. It stops after a second of releasing
   nothing.

`VIEWPORT_FIFO=0` withdraws both globals.

## The rest, by delegation

`xdg-toplevel-icon` (the name reaches the shell on `view.props`),
`xdg-foreign`, `security-context` (a client through a sandbox's socket is
tagged, and cannot create sandboxes of its own) and `xwayland-keyboard-grab`
(advertised only to Xwayland, by Smithay's own filter, so its absence from
`wayland-info` is not a fault).

## xdg-toplevel-drag-v1

Not advertised. The protocol describes moving a toplevel with the cursor
during a drag, and here the shell owns placement: a compositor-driven
`xdg_toplevel.move` would fight the layout rather than inform it, which is why
`move` itself is a no-op in this compositor.

Advertising it anyway would be worse than absence. A browser that finds the
global takes the tear-out path expecting the compositor to carry the window,
and with neither a move nor a message to the shell the torn tab stays where it
was dropped. Without the global Firefox and Chromium keep the paths that work
here. If the drag is ever forwarded to the shell — a message carrying the
attached toplevel and the offset hint, which is the shape the rest of
placement already takes — this note should be replaced by that implementation
and its entry in `docs/ipc.md`.

## wlr-export-dmabuf-v1 and ext-transient-seat-v1

Not advertised. Both describe capabilities this compositor has not got:
zero-copy export of the scanout buffers, and a second input seat with a
virtual-input back end to feed it. Capture is served by
`ext-image-copy-capture` and `wlr-screencopy`, which are implemented, and the
one seat is the real one.

Answering every request with `cancel(permanent)` or `denied` would be
spec-legal and was tried. It is still the wrong trade: a client that probes
for the global and does not carry a fallback is left worse off than if the
global had never been there, while a client that does carry one falls back
either way. Absence tells both of them the same thing. Should either
capability arrive — an export path off the scanout planes, or a virtual-input
back end — the global comes back with it.
