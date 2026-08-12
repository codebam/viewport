# IPC and writing a shell

## IPC

Two transports, one dispatch table.

The page uses the script message handler, which works regardless of the shell's
origin and is unaffected by CORS or mixed-content rules:

```js
window.webkit.messageHandlers.viewport.postMessage(JSON.stringify(msg));
window.addEventListener('viewport', e => handle(e.detail));
```

External tooling uses a UNIX socket of newline-delimited JSON:

```sh
socat - UNIX:$VIEWPORT_SOCKET
```

`viewport msg` is that socket with the message named rather than spelled, and is
installed with the compositor because it *is* the compositor — the same binary,
so the client and the protocol it speaks are never two versions:

```sh
viewport msg -t view.focus --id 12
viewport msg -t output.query --pretty
viewport msg -t output.configure --name DP-1 --mode.width 2560 --mode.height 1440
viewport msg -t shell.command --command output.focus --args right
viewport msg -t subscribe view.focused view.removed   # follow events until ^C
viewport msg -t quit
viewport msg --help                                   # every message and its fields
```

The type is the wire name and the fields are the wire fields, so anything in the
tables below can be sent without learning a second vocabulary. Values are read
as JSON where they parse as one and as text where they do not — `--id 12` is a
number, `--name DP-1` is a string, `--enabled false` is a boolean, and a flag
with nothing after it is `true`. A nested field is dotted (`--mode.width`), and
`--raw '{"type":"..."}'` sends an object verbatim.

It finds the session from `$VIEWPORT_SOCKET`, then from
`$XDG_RUNTIME_DIR/viewport-$WAYLAND_DISPLAY.sock`, then from the newest socket
in that directory — which is the session just started, and so the one being
escaped from on a second TTY. `--socket` names one outright.

A query prints its answer and stops; anything else exits once the compositor has
handled it, and a refusal is a non-zero exit with the reason on stderr. Handled
rather than after a wait: an accepted message is answered with nothing, so the
client sends an `output.query` behind it and reads until the `output.layout`
comes back — the connection is dispatched in order, so anything the compositor
had to say about the first message is already in front of it. Usage mistakes exit 2 without sending anything: a field the message does
not have is refused rather than dropped, because serde ignores what it does not
recognise and `--visable false` would otherwise report success for the opposite
of what was asked.

### Compositor → shell

| Message | Payload |
| --- | --- |
| `config` | `layout` (`"tiling"` or `"scrolling"`), `logo`, `tutorial`, optional `bar`, optional `rules[]`, optional `theme{}` |
| `modifiers` | `logo` (whether Mod4 is held; only sent while `bar` is `"auto"`) |
| `view.added` | `id`, `title`, `app_id`, `output` (name of the output it opened on), `replay`, `floating`, `width`, `height`, `min_width`, `min_height`, and `parent` when this window is a dialog of another — the same link `floating` is partly inferred from, named rather than reduced to a boolean, and omitted entirely when there is none |
| `view.props` | `id`, `title`, `app_id` |
| `view.configured` | `id`, `width`, `height` — the size the client was actually configured with, sent only when that is not the size the shell asked for and only when the answer changes. A client configured below its minimum may ignore it, so the compositor raises the configure to that minimum; a shell that is not told goes on holding a rectangle for a window of a different size |
| `view.parent` | `id`, and `parent` when it has one — whose dialog this is, said after the window was announced. `view.added` carries it when it is known by then; a portal window (a file chooser, in another process) is parented over xdg-foreign long after it maps, and this is the only way the shell hears about it |
| `view.removed` | `id` |
| `view.focused` | `id` (`0` means the shell itself holds focus) |
| `output.layout` | `outputs[]` with `name`, `make`, `model`, `serial`, `enabled`, `active` (the one the shell last named through `output.active`), `x`, `y`, `width`, `height`, `usable_x`, `usable_y`, `usable_width`, `usable_height`, `hdr`, `hdr_capable`, `scale`, `transform`, `modes[]` |
| `workspace.request` | `action` (`activate`, `deactivate`, `assign`, `remove`, `create`), optional `id`, `name`, `output` — a client outside the shell asked for something through `ext-workspace-v1`. See [Workspaces](#workspaces) |
| `shell.command` | `command`, `args[]` — a keybinding forwarded for the shell to act on |
| `session.restore` | `state` (whatever was last saved, or empty) |
| `status.update` | `cpu`, `memory`, `load`, `net_rx`, `net_tx`, `disk_free`, `disk_total` |
| `notification.add` | `id`, `app_name`, `icon`, `summary`, `body`, `urgency`, `timeout`, `actions[]` with `key` and `label` |
| `notification.close` | `id` — the application withdrew it |
| `screencast.pick` | `id`, `sources[]` with `kind` (`"output"`, `"window"`, `"all-outputs"`, `"follow-window"` or `"follow-output"`), `label`, `detail`, and `selected` — an application has asked to share the screen, and this is the list to draw with the highlight where the compositor is holding it. Re-sent whole every time the highlight moves |
| `screencast.pick.done` | `id` — the choice was made or abandoned; take the chooser down |
| `error` | `context`, `message` |

The chooser is drawn by the shell and steered by the compositor, which is the
same split the overview runs on. The shell receives no input of its own, so the
keys are routed here: the compositor takes the keyboard while a chooser is up
and re-sends the list with a new `selected` on each press.

Two of the five kinds name a particular thing. The other three do not, and are
re-resolved on every frame of the share:

| `kind` | What is captured |
| --- | --- |
| `output` | that monitor, for as long as it exists |
| `window` | that window, on whichever screen it is on, without whatever is covering it |
| `all-outputs` | every monitor at once, in one picture, laid out as they are on the desk |
| `follow-window` | whichever window has the keyboard, changing as focus does |
| `follow-output` | whichever monitor is being worked on, changing as that does |

The last three are offered only where they mean something: `follow-window`
needs a window to follow, and `all-outputs` and `follow-output` need a second
monitor — on a laptop both would be the row above them. A following source
whose target has gone away for a moment — focus on the desktop, a monitor
unplugged — feeds no new frames rather than tearing the share down, so the
consumer sees the last picture until there is something to point at again. It
also renegotiates its size whenever what it is following is a different shape,
which consumers handle but not always gracefully.

### Shell → compositor

Also accepted on the UNIX socket, which speaks the same message set.

| Message | Payload |
| --- | --- |
| `view.layout` | `id`, `x`, `y`, `width`, `height`, optional `clip{x,y,width,height}`, optional `scale`, optional `frame{x,y,width,height}`, optional `floating` |
| `view.visible` | `id`, `visible` |
| `view.fullscreen` | `id`, `fullscreen` — tells the client, which rearranges itself on the state |
| `view.focus` | `id` |
| `view.close` | `id` |
| `view.opacity` | `id`, `opacity` (0–1) |
| `view.query` | — replays `config` and a `view.added` for every mapped window |
| `shell.focus` | — |
| `background.focus` | — (toggles the keyboard onto the wallpaper terminal and back) |
| `shell.overview` | `active` |
| `shell.overlay` | `rects[]` of `x`, `y`, `width`, `height` — everywhere the shell has drawn something that belongs above the windows. Sent whole; an empty list means nothing does. See [Drawing in front of the windows](#drawing-in-front-of-the-windows) |
| `screencast.rect` | `x`, `y`, `width`, `height` — the older single-rectangle form of `shell.overlay`, still accepted; a zero size means nothing is above the windows |
| `session.save` | `state` (opaque string) |
| `session.query` | — |
| `notification.action` | `id`, `action` (the key the application supplied, not the label) |
| `notification.dismiss` | `id` |
| `notification.expire` | `id` |
| `output.configure` | `name`, `enabled`, `mode{width,height,refresh}`, `x`, `y`, `scale`, `transform`, `adaptive_sync` |
| `workspace.list` | `workspaces[]` with `id`, `name`, optional `output`, `active`, `urgent`, `hidden` — the whole list, whenever it changes. See [Workspaces](#workspaces) |
| `output.confirm` | — cancels the pending revert; see below |
| `output.hdr` | optional `name` (default: active output), optional `enabled` (absent toggles) |
| `output.active` | `name` — which output the shell considers active |
| `output.query` | — |
| `output.test_add` | — headless only; plugs in a virtual monitor for tests |
| `output.test_remove` | optional `name` (default: the first output); headless only |
| `bind.add` | `chord`, `action` |
| `shell.command` | `command`, optional `args[]` — re-emitted as the `shell.command` *event*; see below |
| `quit` | — |

`shell.command` is the one request that goes the other way. Everything else in
this table is the shell telling the compositor something; this one asks the
shell to act, and it does it by emitting the same `shell.command` event a bound
chord produces — so the shell cannot tell the two apart and needs no code for
it.

It exists because layout is entirely the shell's, and a keypress used to be the
only thing that could reach it. Anything wanting to switch a workspace, move
focus to the next monitor or change the layout model had to be a person
pressing a key, which leaves a benchmark unable to put a window on a chosen
screen and a test unable to drive any of it. `bind.add` is not a substitute: it
binds a chord, and nothing can press one.

The verb is not validated. The shell is the only thing that knows what it
understands — `handleShellCommand` warns about a name it does not recognise and
carries on — so a list here would be a second copy of
`data/shell/commands.js` to keep in step, kept by something with no way to
check it.

The socket is a stream, not a datagram, and the compositor answers on it — so a
one-shot redirect will not do. `scripts/bench-vkcube.sh` opens it from Python
for exactly this reason:

```python
import json, socket
s = socket.socket(socket.AF_UNIX)
s.connect(f"{runtime}/viewport-{display}.sock")
s.sendall(json.dumps(
    {"type": "shell.command", "command": "output.focus", "args": ["right"]}
).encode() + b"\n")
```

A member that is present but of the wrong type is treated as absent, so it
takes the documented default rather than reaching the handler as a zero or a
null. An unknown message type, or one whose `type` is missing or not a string,
is answered with an `error` — to the socket client that sent it, or to the page
when it came from there.

`output.configure` runs `wlr_output_test_state` before committing, so a mode the
hardware cannot drive is reported back as an `error` instead of blanking the
screen you are configuring from. A configuration that *does* commit is still
provisional: it reverts after twelve seconds unless an `output.confirm` arrives,
because a wrong mode blanks the very screen you would need in order to undo it.

### Drawing in front of the windows

Everything the shell paints is *underneath* every client surface: the shell is
one buffer spanning the layout, and each window is a hole in it. That is what
makes the design work — the browser computes the layout and the compositor
paints real clients into the rectangles it measures — and it has one
consequence worth knowing before drawing anything that overlaps a window.

A tiled border is never noticed, because it falls in the gap between two
windows where there is no surface to cover it. A floating window's border lands
*inside* the window beneath it, where that client's surface covers it, and a
floating window drawn that way has no border at all. A dialog the shell puts up
over the desktop is the same thing at full size.

Two messages name the pieces that have to be in front, and the compositor draws
that part of the shell's buffer a second time, above the windows:

- `frame` on `view.layout` — the frame around one window, drawn immediately
  above *that* window, so two floating windows keep their real stacking. Send
  it for a floating window and leave it off a tiled one, which needs nothing.
  The compositor draws the four sides and not the middle: the middle of a
  frame is the desktop's own background in the buffer — `.viewport` has no
  background, but the wallpaper behind it does — and drawing it over the client
  turns the window into a block of wallpaper.
- `floating` on `view.layout` — that this window is floating rather than tiled.
  Layout is the shell's, but the *stack* is the compositor's: it is what the
  renderer draws from and what a click is tested against, and focusing a window
  raises it. Without the flag, focusing a tiled window puts it over the dialog
  that was deliberately placed on top, where it is not merely hidden but
  unclickable. Sent as `true` for a floating window and left off a tiled one;
  absent means tiled. Floating windows keep their order among themselves, and
  an X11 menu or tooltip still sits above all of them.
- `shell.overlay` — everything else that floats: a notification, a bar that is
  not docked, the screen-share chooser. It carries the whole list every time
  rather than one rectangle, because several can be up at once and a message
  that named one would take the others down. An empty list says nothing floats
  now. `screencast.rect` is the older single-rectangle form and still works.

Keep the rectangles tight, and send a fresh list the moment an element goes
away. They are not only about drawing: **a rectangle named here takes the
pointer too.** Inside one, the compositor reports no client under the pointer,
so the click goes to the shell — which is what makes a notification's close
button work, and what makes a stale or oversized rectangle swallow clicks meant
for the window beneath it. The shipped shell measures each element with
`getBoundingClientRect` and drops the entry when the element is gone or has
collapsed to nothing; anything else risks a region that is invisible and still
eating input.

A rectangle belonging to a monitor is the case to watch. The shipped shell
keys them per output — `notifications:DP-1`, `bar:DP-1` — and reports them by
walking the outputs it has, so an output that goes is an output whose last
rectangle is never revisited. A page that does the same has to clear those
entries when `output.layout` stops naming a monitor, because a DisplayPort
screen coming back from DPMS drops and reconnects: without it, a notification
that was up when the screens slept comes back as a rectangle of shell drawn
over the windows that nothing on screen accounts for and no click can dismiss.
The compositor clears the whole list on its side when the desktop page's
toplevel goes away, so a crash or a reload does not leave the *next* page
wearing the last one's rectangles.

That applies to focus as well as to clicks. Clicking an overlay does not raise
or focus the window behind it, and `Mod4`-dragging inside one does not drag
that window — the click never reached it, so neither should its consequences.
Asking for something by name is unaffected: a binding on `shell output.focus`
still works, because it is not a click landing anywhere.

Before this, input and drawing disagreed. A notification was painted on top and
every click went through it to whatever was underneath, so its close button did
nothing unless the notification happened to land on empty desktop.

A window is a real Wayland surface, so nothing the shell draws can crop it —
CSS `overflow` bounds the shell's own painting and no more. `clip` on
`view.layout` is how a window is cropped to the part of it that is on its
output, which is what keeps a column scrolled off the left of one monitor from
being drawn on the monitor beside it. Only the surface is clipped, never the
container: a popup is entitled to extend past the window it belongs to.

## Writing a shell

Style the frame however you like; leave the hole alone.

```css
.viewport { background: transparent; }  /* the compositor paints here */
```

```js
const rect = viewportEl.getBoundingClientRect();
send({ type: 'view.layout', id,
       x: Math.round(rect.left),  y: Math.round(rect.top),
       width: Math.round(rect.width), height: Math.round(rect.height) });
```

Measure with a `ResizeObserver` rather than pushing rects when you think
something moved — CSS transitions, font loading and ancestor reflows all change
that rect without firing anything you can subscribe to. `data/shell/` is a
working reference.

## Remembering the layout

Restarting the compositor kills every client with it — they are its clients,
and nothing survives that. So what is preserved is not the session but the
*places* in it: the tree is written down with each window replaced by the
application that was in it, and as those applications come back they are put
where they were rather than piling up in the order they happen to start. Which
matters most when the compositor is the thing being worked on.

A remembered place is an ordinary leaf whose id is negative. No real window has
one, so every walk of the tree skips it and the renderers already drop leaves
with no window — the structure, the column widths and the weights survive
without a second representation to keep in step. A place nothing comes back for
is dropped after 45 seconds, long enough for a browser restoring its own
session and short enough that a workspace is not permanently shaped around
something that is gone.

The state is stored in `$XDG_STATE_HOME/viewport/session.json`, written through
a temporary file and renamed so a compositor that dies mid-write leaves the
previous layout intact rather than half of a new one. Its contents are the
shell's own format: the compositor stores and returns the blob without
interpreting it, because the layout model belongs to the shell and the
compositor should not gain an opinion about workspaces just to store them.

Restoring only happens into an empty session — restoring over windows that are
already open would move them somewhere they were never asked to be — and the
shell asks for it before replaying its windows, so the places exist before the
windows that fill them arrive.

### Logging

`--debug` mirrors the shell's console into the compositor log and serves the
shell uncached. Without it a JavaScript error in the shell is completely
silent and an edited shell appears to have no effect, so it is worth leaving
on — it costs a page fetch.

`--trace` is the expensive tier: every placement, clip and scale, which during
an animation is one line per window per frame. That is formatted I/O inside the
layout path and a log that grows by megabytes a minute, so it is separate. Pass
it when chasing a geometry bug and not otherwise.

### Overview

`Mod4+o` shows every workspace at once. Each thumbnail contains the workspace's
real tree — the same renderer, at output size, scaled down — so a miniature *is*
the layout rather than a picture of it.

The windows inside are real surfaces, and they are drawn shrunk rather than
resized: a thumbnail is smaller than many windows' minimum size, so asking
clients to resize would be refused about as often as it was honoured, and
slow when it wasn't. `view.layout` carries the window's real size plus a
`scale`, and the compositor scales the buffers the client already produced.

The scale has to be re-applied once per frame, before compositing.
`wlr_scene_surface` recomputes each buffer's destination size from its surface
on every commit, so a window with live content snaps back to full size the
moment it paints. Doing it per commit is not enough either: a client that
paints through subsurfaces — Firefox — commits on surfaces the toplevel has no
listener for, so its content stayed full size while simpler windows shrank.
Once a frame catches every case, and the write is skipped when the value already
matches, so it does not damage the scene by itself.

Crop and scale are applied together, in that order, every time. They are not
independent — the scale is computed from what the crop left — so applying them
at different moments lets a client commit land in between, leaving a
destination that describes the whole window and a source that describes the
strip which survived the crop. The strip is then stretched to fill it. The
scale is also cleared on every window when the overview closes rather than
waiting for the shell to send each one a new rect: a window whose rect has not
changed gets no message, and an idle window never repaints, so it kept the
overview's size until something made it draw again.

What gets scaled is the buffer's *destination size*, because whatever wlroots
last put there already accounts for the surface's buffer scale, its transform,
any viewport it set, and the crop. Computing from the buffer's own dimensions
gets all of that wrong — most visibly for a client that renders at 2x and says
so, which then draws at double size.

It cannot simply be read back each frame either: wlroots recomputes the
destination only when the surface commits, so in between, reading it returns
what this code last wrote and multiplying again shrinks the window a little
further every frame. Each buffer's unscaled size is remembered alongside the
value written from it — if the destination still matches what was written the
remembered size still stands, and if it has changed then wlroots recomputed it
and the new value is the unscaled one.

Two consequences worth knowing. Only the offsets *between* buffers are left
unscaled, so a client painting through subsurfaces — a browser compositing
video, mostly — shows those parts misplaced while shrunk; it is exact again the
moment the scale returns to 1. And input is routed to the shell for the duration
(`shell.overview`), because a click on a miniature means "take me there" rather
than reaching the client underneath. That routing is also what makes dragging
possible: press a window and release it over another thumbnail to move it to
that workspace, or release it where it started to go there. The overview stays
open after a move, so several windows can be arranged in one visit.

Visibility works differently while it is up. A window is normally on screen
only if some monitor is displaying its workspace; the overview draws every
workspace at once, including the ones no monitor is showing, so there the
thumbnail's own render is the whole answer. Without that exception a window on
an off-screen workspace stayed hidden and its thumbnail came out labelled
empty.

Every workspace is shown, not only the occupied ones — an overview is how you
get somewhere, and an empty workspace has to be visible to be a target. They are
dealt out across the monitors rather than crowded onto one: a window element
exists once in the DOM, so a workspace can only be drawn on one screen, and
rendering all of them everywhere would have each grid steal the windows from the
last. Each output keeps the workspace it was already showing.

### Animation

Window frames are DOM, so moving and resizing them animates in CSS and costs
the compositor nothing. What does not come free is the window's *contents*: a
real surface drawn at whatever rect the compositor was last told. Sampling
geometry once after a relayout would slide the frame smoothly and snap the
contents straight to the destination — worse than not animating at all.

So the shell resamples every window's rect each frame until the layout stops
moving, self-terminating a few idle frames later rather than listening for
`transitionend`, which is unreliable while dragging. Two things keep the cost
down: the compositor skips the client reconfigure when only the position
changed, so a window sliding across the screen never asks its client to resize;
and drags set a class that disables the transition, because interpolating
toward a pointer lags it by the whole duration.

Fading a window in cannot be done in CSS for the same reason, so it is tweened
in the shell and sent as `view.opacity`, which the compositor applies to the
surface itself. `prefers-reduced-motion` disables all of it.

Switching workspace is the one moment where nothing moves and everything
changes: one set of windows is replaced by another between two frames, so there
is no motion to interpolate. The arriving windows are faded in through the same
`view.opacity` tween. Only the arrivals — fading the departures out would mean
keeping them rendered after their workspace stopped being the one on screen,
and two workspaces' windows visible at once looks worse than a switch that is
merely quick. It is the difference against what was on screen before, not every
window on the destination, so a window the two workspaces share stays put
instead of blinking.

Closing a window hands focus to its neighbour rather than to whatever is first
on the workspace, and the two layouts want different neighbours. In the strip it
is the column to the left — the strip has a direction, and being dropped at its
start after closing something in the middle means scrolling back to where you
were. In a tiling tree it is whatever shared a container with it: the split it
was part of, which is what "the parent" amounts to. A window stacked in the
same column comes before either, since closing one of a pair should not move you
to a different column. The choice is made before the window is removed, because
afterwards the tree has collapsed around the hole and nothing records where it
was.

A window takes focus when it opens, however it was launched. `replay` marks the
copies sent when the shell reloads or asks for the window list — those describe
windows that have been open for a while, and focusing on every `view.added`
would move focus to whichever window came last in the list every time the shell
reloaded, which is every time it is edited. The other exception is a window a
rule deliberately placed on another workspace: that was an instruction to leave
it there, not to be taken there.

### Reporting a problem

```sh
./scripts/collect-report.sh
```

Writes one file with the log and the facts needed to read it: which commit,
which renderer, which GPU and connectors, and what the config actually
contained. A log on its own rarely settles anything, because the same line
means different things on different hardware or against a different build —
and "which binary was running" is usually the first question, since an
installed copy and a checkout build diverge the moment one is ahead.

Long logs are trimmed to their ends. A failure shows up either where it started
or where everything stopped; the middle of a long run is frame timing. Nothing
is uploaded — it is a plain file, worth reading before sending on.

### Testing the shell

The layout engine lives in `data/shell/`, and running it under a headless
compositor proves nothing: the web view renders, but nothing drives the layout,
so a broken tree looks exactly like a working one. `tests/shell.test.js` stubs
the DOM far enough to run the real file unmodified and checks structure — four
windows make four columns, consume and expel are inverses, a tabbed container
shows exactly one window and it is the focused one.

```sh
timeout 20 node tests/shell.test.js data/shell/shell.js tiling
timeout 20 node tests/shell.test.js data/shell/shell.js scrolling
```

`timeout` because the shell sets a live-reload interval and so never exits.

### Testing window capture

Whether a shared window really is that window can only be answered from
outside, by a client that asks for one and looks at what comes back. Every
symptom of the broken version had to be reported by a person sharing a window
in a browser, and every wrong guess cost them their session.

```sh
meson test -C build
```

That starts the compositor headless, opens a window painted one colour inside
its window geometry and another in the decoration margin around it, captures
it, and checks every pixel. Anything from a neighbouring window, the shell
behind, or the margin outside shows up as a colour that does not belong, with
coordinates. It runs in both layouts; the scrolling one crowds the window under
test to the edge of a strip that is being laid out again on every frame.

`tests/capture-client.c` says what each check is for, including the one that
does not currently fail against the broken code and why it is still there.

Outputs come up at whatever mode the display says it prefers. That is the right
default — it is the timing the manufacturer chose — but it is not always the
fastest, since plenty of high-refresh monitors nominate a 60Hz timing and
running a 240Hz panel at a quarter of its rate is easy to miss. So it can be
overridden per output:

```jsonc
"outputs": {
  "*":    { "max_refresh": true },              // fastest at the preferred size
  "DP-3": { "mode": "2560x1440@239.760" }       // or name one outright
}
```

`max_refresh` only maximises the refresh rate: the highest refresh overall may
belong to a lower resolution, and a sharper picture is worth more than a faster
one. A named `mode` may leave the refresh off to match on resolution alone. An
exact output name beats `"*"` whichever order they appear in.

The chosen mode is logged at startup with its refresh rate, which the line used
to omit — the omission is how a monitor sits at 60Hz unnoticed. Applied when an
output appears; `output.configure` or any `wlr-output-management` client
changes it afterwards.


## Workspaces

Workspaces are the shell's. The compositor does not create them, name them or
switch them, and until `workspace.list` arrives it does not know they exist —
which is why `ext_workspace_manager_v1` on a fresh session publishes an empty
world rather than a wrong one.

The shell sends the whole list whenever it changes:

```json
{"type":"workspace.list","workspaces":[
  {"id":"1","name":"one","output":"DP-1","active":true},
  {"id":"2","name":"two","output":"DP-1"},
  {"id":"3","name":"three","output":"DP-3","active":true,"urgent":true}]}
```

`id` is the shell's own and has to be stable for the life of the workspace: it
goes out as `ext_workspace_handle_v1.id`, which is how a bar tells one
workspace from another across its own restart. `output` names the screen, and
becomes the group the workspace sits in; a workspace with no output is in no
group, which the protocol allows. Whole rather than a diff, because the shell
already has the list and reconciling two halves of one is how they drift.

Requests come back the other way. A bar that clicks a workspace produces:

```json
{"type":"workspace.request","action":"activate","id":"2"}
```

and the shell does whatever that means to it — nothing has happened in the
compositor yet. Whatever the shell decides shows up in the next
`workspace.list`, which is what the bar redraws from. A shell that ignores
these messages publishes a list that cannot be changed from outside, which is
an honest description of a shell that has not implemented them.

`create` carries `name` and `output` instead of `id`, because the workspace it
asks for does not exist yet. `assign` carries both `id` and the `output` to
move it to.

### What the shipped shell does

`data/shell/outputs.js` publishes the list from `relayoutAll()`, which is where
everything that can change a workspace ends up — a switch, a window opening, the
last one on a workspace closing — and sends only when the list differs from the
one last sent. Workspaces are numbered, so `id` and `name` are both the number,
and the ones published are the ones the bar's own buttons draw: on screen, or
holding a window.

`output` is the monitor showing it, or the one that showed it last. Nothing in
the shell needs that second half — a workspace goes wherever it is asked for —
but every workspace has to name a screen or a bar has no group to draw it in.

Of the requests, `activate` and `assign` are honoured; both mean "show this
workspace", on the monitor already showing it or on the one named. `deactivate`,
`create` and `remove` are declined by doing nothing: there are nine workspaces,
always, and a monitor is always showing one of them.

### Watching it from outside

To see what a bar sees:

```sh
cargo run -p viewport --example workspaces
cargo run -p viewport --example workspaces -- --activate 2
```

Waybar reads this through its `ext/workspaces` module. Its `sway/workspaces` and
`hyprland/workspaces` modules speak those compositors' own IPC sockets and find
nothing here, so a bar configured with one of those shows no workspaces at all
whatever the compositor publishes:

```json
"modules-left": ["ext/workspaces"],
"ext/workspaces": { "all-outputs": true, "format": "{name}" }
```
