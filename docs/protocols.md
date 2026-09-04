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

## Background effects

`ext-background-effect-v1` is advertised only when blur behind a surface is a
real render element. The nested and headless backends do that through GLES: the
element captures and downsamples the framebuffer already drawn behind it, runs a
fixed Gaussian blur shader, and draws only the committed protocol region before
the requesting surface is composited. Region add and subtract operations keep
their protocol order, and subsurfaces and popups get an effect at their own
place in the surface tree rather than one rectangle guessed from the toplevel.
One composited image admits at most 64 effects and 4,194,304 downsampled scratch
texels in total. A region accepts at most 256 add/subtract operations and 1,024
resolved rectangles. Requests past those bounds draw without blur rather than
allocating unbounded compositor memory or GPU work.

The global is created after the GLES context reports framebuffer blits and the
blur shader compiles. A GLES 2 context therefore gets no global. The DRM backend
gets no global either, including on a card that happened to fall back to GLES:
its renderer is selected independently per card, Vulkan is preferred, cards can
be mixed, and a later output can hotplug onto another renderer. A compositor-wide
global cannot honestly promise an effect that only some of those outputs draw.

The built-in shell is not such a surface. Both the in-process WPE view and the
out-of-process shell hand the compositor a DMA-BUF which is rendered as one
`render::Shell` texture; no `wl_surface` survives in that element for this
protocol to extend. Ordinary Wayland clients can request blur now. Giving the
shell's own translucent chrome the same effect still needs blur-region metadata
to travel with its frame and a framebuffer-effect element around that texture.

Capture uses the same effect elements as the displayed frame. Private windows
are replaced with black before anything above them captures the framebuffer.
An isolated-window capture includes its associated xdg popups and their effects,
clipped to the stream's fixed toplevel bounds; its blur samples only the
isolated window image and black outside it, never neighbouring desktop pixels.
While locked, frame construction returns the lock surface and pointer before it
can reach any desktop element, so a lock surface requesting blur can sample only
the black lock framebuffer, never the desktop underneath.

Vulkan support belongs in `viewport-vulkan`, not in a protocol flag here. Its
`VulkanFrame` currently offers neither `FrameContext` nor `BlitFrame`, so a
`RenderElement::capture_framebuffer` implementation cannot copy the active
render target into a sampleable image. It also has no renderer operation or
pipeline for applying a blur to that image. Supporting the protocol on DRM
requires both pieces: a render-pass-safe active-frame copy with synchronization,
and a blur operation producing a texture that `render_texture_from_to` can draw.
After that, Viewport must preflight the operation on every DRM renderer and keep
the global absent whenever any active or newly hotplugged renderer lacks it.

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

Two things a DMA-BUF cannot carry are inferred when the client says nothing. The
matrix comes from the picture's height — BT.601 at or below PAL's 576 active
lines, BT.709 above it, which is the rule every video stack uses — and the range
is taken as narrow, which is what broadcast and every hardware decoder default
to. A full-range buffer read as narrow comes out slightly washed out; the
reverse clips. Chroma siting is not guessed: it is whichever of the two the
device says it can reconstruct, preferring the one the MPEG family actually uses,
and where chroma cannot be filtered linearly the luma filter drops to nearest
with it, because Vulkan requires the two to agree.

Those guesses are the fallback, not the whole answer. `wp-color-representation-v1`
lets a client *say* what its Y′CbCr code words mean — the matrix, the quantisation
range and the chroma siting — so the height rule stops being load-bearing for
anyone that bothers to declare. The compositor advertises only the three matrices
its sampler conversion can be told (BT.709, BT.601 and BT.2020) across both
ranges, refuses any other at the request, and stores what a client declares on
the surface through the same double-buffered path `wp-color-management-v1` uses,
so a declaration lands with the commit that carries it. At that commit the
declared coefficients are checked against the buffer's format, and a Y′CbCr
declaration on an RGB buffer is the protocol error the request asks for rather
than a quiet substitution. The renderer reads the declaration when it imports
the buffer — the one place the buffer and its surface are both in hand — and
takes its matrix, range and siting from it in place of the inference.

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

**Menus come from a second specification.** The tray one says nothing about
them: an item points at a `com.canonical.dbusmenu` object — Canonical's, written
for Unity, and what GTK and Qt both publish a menu through — or it implements
`ContextMenu` and draws its own window. Both are in use and an item may do
either, so the shell asks the same question for every item and the compositor
decides which it was: a menu object is read here and sent to the shell to draw,
and everything else is asked to draw its own.

Reading one is two calls. `AboutToShow` first, because a menu is usually built
when it is asked for and an application that populates lazily answers an empty
layout to anything that skips it. Then `GetLayout` at depth −1, which is the
whole tree in one call — a menu is small, the shell draws it in one pass, and a
round trip per submenu would be a menu that opens in stages while the
compositor is trying to hold a frame. Its answer is `(ia{sv}av)`, recursively:
an id, an open map of properties, and children that are *variants*, which is
what makes the recursion fall out of the type.

What the shell is handed is a label, whether the row can be chosen, whether it
is ticked and what is under it. Rows an application marked invisible are
dropped here rather than sent to be hidden, labels lose the mnemonic marker the
toolkit would have drawn — `_Quit` and `&Quit` are both in use — and a row's
icon is resolved exactly as an item's is, except that `icon-data` is already a
PNG and only needs wrapping.

Going back the other way, `Event` is sent for the row that was chosen and again
when the menu closes with nothing chosen. Applications rely on the second:
several rebuild their menu on close, and one that is never told keeps serving a
stale one.

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

The compositor runs the locker named in `lock_command` where one is named, and
its part is then being a correct `ext-session-lock` server for it — which is
the whole point of that protocol: the program drawing the lock screen is
separate, and can crash without unlocking anything. Blanking turns the outputs
off, which is what actually saves the panel.

With no `lock_command` the lock screen is the shell's, and no client and no
protocol is involved at all — the compositor takes the lock itself, tells the
page to draw, and draws the page's buffer and nothing else until it is
unlocked. The property `ext-session-lock` exists for is kept by hand there, and
it has to be, because the thing drawing the lock screen is the same page that
draws the desktop out of the same buffer: none of that buffer reaches a locked
screen until the page has said it painted a lock screen for that lock *and*
painted a frame after saying so, so a page that crashed, hung or reloaded is a
black screen rather than a desktop. The two never run together — a locker
asking for the session while the built-in screen is drawing is refused by
`lock()` exactly as a second swaylock is, and one asking while it is *not*
drawing is granted, which is the same documented escape from a lock screen that
has stopped painting.

Idle inhibitors are honoured, and only while their surface is mapped — a paused
video on a hidden workspace is not keeping anyone awake. An inhibitor counts as
activity rather than merely pausing the countdown, so releasing one starts the
clock again instead of firing immediately.

Wayland's inhibitor is not the one most software uses, so it is not the only
one honoured. A browser playing video, a video player, a presentation tool: all
of them hold the screen awake over D-Bus, because that interface predates
Wayland and every toolkit already had code for it. Two are answered here, both
ending in the same registry the idle timer reads —
`org.freedesktop.ScreenSaver`, which is what Firefox and mpv reach for, served
at `/org/freedesktop/ScreenSaver` and at `/ScreenSaver` because software asks
at both; and `org.freedesktop.impl.portal.Inhibit`, which is where a sandboxed
application's request arrives from the portal frontend. Without them a film on
this desktop is watched with the screen blanking under it, and the fix somebody
finds is to turn the idle policy off for everything.

A bus hold is released when its owner's connection goes, not only when the
program remembers to give it back. A player killed mid-film never calls
`UnInhibit`, and a compositor waiting for one would keep the screens lit for
the rest of the session with nothing on screen to say why. A cookie may only be
released by the connection that took it: the session bus is reachable by every
process in the session, and one program guessing another's cookie would turn
the screen off in the middle of somebody's film.

`SimulateUserActivity` is answered too, and goes through the path a keypress
takes — including bringing blanked screens back, because a program saying
somebody is there means what somebody being there means. The portal interface
is version 1 deliberately: version 2 adds `CreateMonitor` and
`QueryEndResponse`, which exist to put a "you are about to be logged out"
dialog on screen and wait for the answer, and there is no logout here to be
about. `GetActive` answers false rather than erroring, because a client that
gets an error on it sometimes concludes the whole interface is missing and
stops inhibiting with it — this compositor never draws a screensaver, and its
lock screen is not one.

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

`maximize` fills the usable workspace while keeping the bar, gaps, border, and
the window's prior tiled or floating place. `minimize` removes the window from
layout while retaining its tree place and taskbar entry; activating that entry
restores it. The compositor owns this state, mirrors it to X11
`_NET_WM_STATE_HIDDEN`, and publishes it through foreign-toplevel management.
xdg-shell has no minimized state enum, so its request is answered with a
configure while the state is carried to the shell. `wm_capabilities` advertises
fullscreen and maximize, the capabilities xdg-shell can represent. It is sent on the initial
commit rather than at creation, because the surface is not initialised before
then and wlroots asserts rather than ignoring a configure scheduled too early.

A client dragging itself by its own titlebar (`move`, `resize`) is honoured for
both native Wayland and XWayland after its seat, initiating button, implicit
grab, and pointer focus are validated. Native requests additionally validate
their serial. The compositor captures the pointer until that button is released
and forwards deltas plus any of the eight requested resize edges to the shell.
The shell then interprets the gesture for the active layout, just as it does for
Mod4+drag; the compositor never invents a rectangle or silently pulls a tiled
window out of its layout. A target that unmaps during an X11 drag ends the grab.

## Keyboard shortcuts

A virtual machine, a nested compositor or a remote desktop client needs the
chords this compositor would otherwise swallow — `Mod4+Return` has to reach the
guest rather than opening a terminal here.
`keyboard-shortcuts-inhibit-v1` is how a client asks for that, and it applies
only while that client holds focus, so the bindings come back the moment focus
moves away.

## Global shortcuts

`org.freedesktop.impl.portal.GlobalShortcuts` is how an application hears a
chord it does not have focus for — push-to-talk in a chat program, start and
stop in a recorder. On X11 those were server-side key grabs, which is why they
worked and also why they were removed: a client that can grab one chord can
grab every chord. The portal replaces the grab with a question, and the
compositor is the only thing in the session that can answer it, because the
chord has to be resolved before the focused client is offered the key.

A grant is asked for once and then remembered, which is the opposite of what
the RemoteDesktop backend here does with an input grant, so the difference is
worth stating. A remembered remote-desktop grant is a process that can type
anything into this machine on the strength of a file. A remembered shortcut is
one chord reaching one application, only while that application is running and
holding a session — and the alternative is asking the same question at every
login for the same push-to-talk key, which teaches somebody to agree to
dialogues without reading them. What was agreed to is written to
`~/.local/state/viewport/shortcuts.json`, by application and by chord, so it
can be read and deleted.

The desk's own keymap wins. A shortcut is matched only after the compositor's
built-in chords and everything in `binds` have declined the key, so an
application asking for `Mod4+Return` gets a grant that never fires rather than
a terminal that stops opening. A trigger this keymap cannot read — an
unknown modifier, a key xkb does not have, a mouse button — is refused before
the question is asked, because agreeing to it would be agreeing to something
that can never happen. An empty trigger, which is the portal's way of saying
"you choose", is refused for the same reason: choosing needs a shortcut editor
to choose in, and there is none here.

Version 1: `CreateSession`, `BindShortcuts`, `ListShortcuts`, and the
`Activated`/`Deactivated` signals, which carry the session, the application's
own id for the shortcut and the timestamp of the key that moved. A
push-to-talk key is the reason both halves are sent — the application is
holding a microphone open on the strength of the press, and nothing else will
tell it the key came back up.

## Input capture

`org.freedesktop.impl.portal.InputCapture` serves the opposite direction from
RemoteDesktop. RemoteDesktop accepts input from a sender libei context and
performs it on this seat; InputCapture creates a receiver context and gives it
physical keyboard and pointer events after the pointer crosses a validated
exterior screen-edge barrier. Touch is not advertised.

Every `Start` opens the trusted shell consent prompt. Grants are not persisted,
and the unsafe no-consent screen-share fallback never applies. A session must
fetch the current output zones, install barriers against that exact zone set,
connect its receiver EI socket, and enable itself before a crossing can
activate it. Output geometry changes invalidate the barriers.

Only local libinput or nested-backend events enter this path. RemoteDesktop EI
events cannot be captured and reflected into another remote connection. Lock,
VT pause, output topology changes, portal frontend loss, session close, and EI
disconnect all stop capture; `Ctrl+Alt+Escape` is the compositor-owned emergency
release chord. Captured presses are tracked so neither side receives an
unmatched release across an activation boundary.

## wlr-layer-shell policy

`wlr-layer-shell`'s client-provided namespace is retained on Smithay's mapped
`LayerSurface` and resolved through the ordered `layer_rules` configuration.
Resolved opacity, capture permission, blur intent and same-layer `z_index` are
attached to that mapped object and copied into renderer-neutral `Frame` data.
Configuration reload replaces the attachment in place; it does not unmap the
surface, disturb its exclusive zone, or send a configure unrelated to protocol
state.

Protocol layers remain the hard stacking boundary: `overlay`, `top`, `bottom`
and `background` keep that order, while `z_index` orders only peers. Pointer hit
testing consumes the same resolved order. Session lock bypasses the ordinary
frame entirely, so no layer rule can place a client over a lock screen.

Capture-denied layer trees are replaced with opaque black at their complete
bounding rectangle, including subsurfaces and xdg popups. The deny state is
resolved before mapping and refreshed synchronously on config reload, including
maps retained by disabled outputs. A missing policy attachment is initialized
from the active rules rather than an unrelated permissive default.

`blur` uses the GLES framebuffer effect described above on nested and headless
backends. It remains intent only on DRM/Vulkan, where Viewport advertises no
background-effect capability rather than silently accepting client requests it
cannot render.

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

## linux-dmabuf-v1

`ViewportState::advertise_dmabuf` in `state.rs` for the global, `udev.rs` for
what each device and each output says, and `handlers/mod.rs` for what happens
when a client hands a buffer over. What a client is told comes in three layers,
and on a machine with more than one graphics card they do not all say the same
thing:

| Layer | What it names | Where from |
| --- | --- | --- |
| The global's default feedback | the primary card's render node, and a format list | `advertise_dmabuf`, once at startup |
| Per device | that card's render node and everything its renderer can import | `device_feedback` |
| Per output | the same, plus a `Scanout` tranche of the formats *this output's primary plane* accepts | `output_feedback`, per surface |

The per-output feedback is the one a mapped window actually gets, and it names
the card the window is being shown on. That is what makes two cards work: a
client that honours per-surface feedback — everything on Mesa does —
reallocates when its window moves to a monitor on the other card. The scanout
tranche is intersected with what the renderer can import, because a format the
plane takes and the compositor cannot sample has nowhere to go the moment a
notification overlaps that window.

The default feedback can only name one device, because the protocol's default
tranche is one device. Which formats it carries is a preference, and
`cross_gpu` is it: `native` (the default) advertises everything the primary can
read, `portable` only what every card can read. See
[Graphics cards](configuration.md#graphics-cards).

**An import is judged by every card, not by the primary.** A buffer is accepted
if any online card's renderer can read it, and refused only when none can — a
refusal over `linux-dmabuf` is a protocol error, so getting this wrong
disconnects clients. Judging by the primary alone did exactly that on a two-card
machine: a window on the second card's monitor was told by its per-surface
feedback to allocate for the second card, and the buffer that came back was
handed to the first card's renderer to be approved. A buffer that some cards
read and others do not is accepted, and the window is missing from the screens
on the cards that refused it; that is logged once per format, modifier and card.

## wp-drm-lease-v1

`crates/viewport/src/workspace.rs`'s neighbour, `crates/viewport/src/udev.rs`
plus the handler in `handlers/mod.rs`. A connector with the `non-desktop`
property is never made into an output: it is offered for lease, and a client
that takes one is given the connector, a CRTC that is not driving a desktop
output, and that CRTC's primary plane with a claim on it.

One global per graphics card, not one per session. The global carries the DRM
node the client should open, and the connector, CRTC and plane handles that
travel through it only mean anything on the device that issued them — so a
request is answered from the card whose node it names, and the free-CRTC search
looks only at that card's CRTCs. Handles are small integers handed out per
device, so a card mix-up would not fail cleanly: the "free" CRTC found on the
wrong card is very likely a real one there, possibly one scanning out the
desktop. A card that goes takes its global with it, and one that comes back gets
a new one on whichever node it came back as.

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
tagged, cannot create sandboxes of its own, and is not offered direct capture;
it uses the consent-bearing portal instead) and `xwayland-keyboard-grab`
(advertised only to Xwayland, by Smithay's own filter, so its absence from
`wayland-info` is not a fault).

## xdg-toplevel-drag-v1

Not advertised. The protocol describes moving a toplevel with the cursor
during a drag, and here the shell owns placement: a compositor-driven
`xdg_toplevel.move` would fight the layout if the compositor changed the
rectangle itself. Viewport instead forwards that older request's pointer deltas
to the shell. This protocol additionally describes a drag-and-drop attachment
and offset hint, which are not forwarded yet.

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

A connected libei client is also told when the seat's modifiers change, over
`ei_keyboard.modifiers`. It has to be: a client composes a capital as press
Shift, press the letter, release both, against its own idea of the state, and
the state has two sources it cannot see — the person at the machine pressing
Shift, and a key the client itself sent latching Caps Lock. A session that
drifted types capitals nobody asked for until something happens to resettle it.
Sent only on a change, because typing at the desk moves the modifier state
twice per shifted character; a client that has just bound a keyboard is told
the current state outright, since before that there was no device to tell.

A remote-desktop client does not need either. `org.freedesktop.impl.portal.
RemoteDesktop` is implemented at version 2, and it drives the one real seat:
the pointer, keyboard and touch a person is already using. There are two ways
in and both end there. The Notify calls hand each event to the `inject_*` path
in `crates/viewport/src/input.rs` that the control socket uses; ConnectToEIS
hands the application a libei socket instead, and the events off it go through
`process_input_event` — the same path libinput's do — because smithay's
`backend_libei` presents an EI client as an `InputBackend`. Which devices that
socket carries is decided when the client's devices are created, from what the
person at the machine granted, and closing the session closes the socket. See
`crates/viewport/src/libei.rs`. That is the
whole reason a transient seat is not wanted here — a second seat exists so a
remote session can have devices of its own, and a compositor that can inject
into the first one has nothing to put on the second. What it costs is that a
remote pointer and a local one fight over the same cursor, which is what
somebody sharing control of their machine expects to happen.

## Xwayland and HiDPI

X11 clients are left at 1x unless the config file says otherwise, and the
setting that says otherwise buys sharpness for some toolkits at the cost of
size for the rest. Both halves of that are decisions rather than gaps, so both
are written down here.

**What 1x means, and why it is not simply broken.** An X11 client draws into a
buffer whose pixels are Xwayland's pixels, Xwayland's screen is this
compositor's logical desk, and the compositor magnifies the result onto the
panel. On a 2x monitor an 800x600 X window is an 800x600 buffer stretched
across 1600x1200 physical pixels: the window is exactly the physical size it
should be, and it is blurry. Nothing is mispositioned, nothing is clipped, and
an application that has never heard of a scale factor — xterm, an SDL game, a
twenty-year-old Motif tool — behaves the way it always has. That is why this
is the default and why several compositors stop here.

**What `xwayland.scale` does.** Setting it to `2` (or to `"auto"`, which takes
the number from the monitors) makes two changes at once, and they only work
together:

- Xwayland's connection is given a *client scale*. Its outputs are reported
  at twice the pixels, so the X screen behind a 1600x900 desk is 3200x1800 X
  pixels, and everything Xwayland sends back — buffers, surface offsets, the
  geometry the window manager reads off an X window — is divided by two on the
  way in. An X window of 1600x1200 lands on the desk as 800x600 logical pixels
  with four times the detail.
- XSETTINGS are published on the X server: `Gdk/WindowScalingFactor` (the
  integer window scale GTK acts on), `Gdk/UnscaledDPI` (98304, i.e. 96dpi in
  the 1024ths XSETTINGS uses, so GTK does not scale the text twice) and
  `Xft/DPI` (96dpi times the scale). Xwayland is also started with
  `-dpi 96×scale` so that a client computing density off the screen itself
  gets the same answer.

Without the first half, the second is every X11 window at twice the size it
asked for — which is what setting `GDK_SCALE=2` by hand on an unpatched
compositor does, and why that advice always arrives with a patched Xwayland
attached. Without the second, the first is every X11 window crisp and half the
size. The compositor sets them together or not at all.

**What it reaches, and what it does not.** This is the partial answer, stated
plainly:

- GTK 3 and GTK 4 on X11 read `Gdk/WindowScalingFactor` and draw at the scale.
  These come out sharp and correctly sized.
- Qt 6 and Chromium (so also Electron) read `Xft/DPI` and scale from the
  density. Sharp and correctly sized, give or take each toolkit's own
  rounding.
- Qt 5 scales only when `QT_AUTO_SCREEN_SCALE_FACTOR` or `QT_SCALE_FACTOR` is
  in its environment, and this compositor does not put it there — see below.
- Java/AWT, SDL, GLFW, Tk, xterm, and anything else with no notion of a scale
  factor draw their 1x pixels into a screen whose pixels are now half the size
  of a logical one. They come out **sharp and half as large**. On a 2x desk
  with the key set to 2, an xterm is legible-if-small; on a 3x desk it is not.

That asymmetry is the whole cost of the setting, and it is why the default is
off. A desk whose X11 applications are a browser and a GTK image viewer wants
this on; a desk whose X11 application is a game or an old scientific tool
wants it off, and there is no third answer this compositor can pick on their
behalf.

**Mixed DPI has no right answer.** There is one X screen behind every monitor,
so there is one scale. `pick_xwayland_scale` takes the largest — a 2x laptop
panel beside a 1x monitor picks 2, which makes the panel sharp and leaves an X
window on the monitor drawing four times the pixels it needs into a rectangle
of the right size. Picking the smallest would leave the blur on exactly the
panel that was the reason to turn it on. Per-window scale, the thing that
would actually solve it, is the feature nobody has made look good: it means a
different X screen per monitor, which X11 does not have, or rescaling a live
window as it crosses a monitor edge, which the client is never told about.

**What was rejected, and why.**

- **Xwayland's own `-scale`.** Not in the pinned Xwayland (24.1.13, in the
  flake and in nixpkgs both — the option list has `-dpi` and `-hidpi` and no
  `-scale`), so it cannot be the mechanism here. It also moves the whole X
  coordinate space, which every compositor that uses it has to compensate for
  in its window manager. The client scale above is the same idea implemented
  on this side of the socket, works on the Xwayland everyone has, and needs no
  arithmetic in `handlers/xwayland.rs`.
- **`_XWAYLAND_GLOBAL_OUTPUT_SCALE`.** The property the `hidpi-xprop` patch
  set uses. It requires a patched Xwayland *and* a patched compositor, which
  is two out-of-tree builds to ask a user for, and it is the same mechanism
  reached by a worse road.
- **`GDK_SCALE`, `GDK_DPI_SCALE` and `QT_SCALE_FACTOR` in the child
  environment.** Refused, for two reasons that are each sufficient. The first
  is reach: `child_display_env` is handed to programs this compositor spawns
  and to nothing started from a terminal, an ssh session or a systemd unit,
  while a setting on the X server reaches every client that ever connects to
  it. The second is aim: nothing at spawn time knows whether the program about
  to start will turn out to be an X11 client or a Wayland one, and a Wayland
  toolkit that already scales itself from `wl_output` and then finds
  `GDK_SCALE=2` in its environment draws at four times. Writing them into this
  process's own environment instead is the `setenv`-against-live-threads
  hazard that `child_display_env` exists to avoid. The one environment
  variable that *is* set is `XCURSOR_SIZE`, and only in Xwayland's own
  environment, where it cannot reach a Wayland client — an X cursor is loaded
  in X pixels and would otherwise be half the size on screen.

**Limits worth knowing.** The scale is read once, when Xwayland starts: a
config reload moves the value and nothing on screen, because the X screen's
size in X pixels is settled at connect and X11 has no graceful way for a
window manager to resize the root window under a running client. A monitor
plugged in after startup does not change it either. Changing the setting means
restarting the session. `tests/xwayland-scale.test.sh` checks both halves
arrive, and checks that a config file which says nothing about X11 leaves the
X server exactly as it was.
