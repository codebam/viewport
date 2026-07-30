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

Only the features wlroots 0.20 actually implements are advertised: parametric
image descriptions, PQ and BT.2020. sRGB is deliberately not advertised — the
protocol treats it as always supported.

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

## Tablets

A stylus reports pressure, tilt, distance, its own buttons and whether it is
hovering or touching. `tablet-v2` carries all of it, and a drawing program that
gets only x and y has a pressure-sensitive brush reduced to one width of line.

The stylus also drives the cursor, which is what makes it work in the many
programs that only understand a pointer. Tablets are absolute devices, so the
cursor jumps to where the pen lands rather than moving relative to where it was,
and touching the tablet focuses what is under the pen — otherwise drawing in a
window would mean clicking it with a mouse first.

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
