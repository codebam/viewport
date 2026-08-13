# The canvas layout

Every workspace an unbounded plane, panned and zoomed. Windows sit where you
put them and stay there; the view moves over them.

```
                    the plane, unbounded
   . . . . . . . . . . . . . . . . . . . . . . . . . . . . . .
   .                +--------+                               .
   .   +--------+   | notes  |        +---------+            .
   .   | editor |   +--------+        | browser |            .
   .   |        |     +===================+     |            .
   .   +--------+     ‖  the viewport     ‖-----+            .
   .                  ‖  (one monitor)    ‖                  .
   .        +-------+ ‖    +--------+     ‖                  .
   .        | logs  |-‖----| term   |     ‖                  .
   .        +-------+ ‖    +--------+     ‖                  .
   .                  +===================+                  .
   . . . . . . . . . . . . . . . . . . . . . . . . . . . . . .
```

Turn it on with `"layout": "canvas"` in the config file, `--layout canvas` on
the command line, or `shell layout.model canvas` at runtime.

## The model

**A workspace is a plane with no edges.** Windows have world coordinates and
keep them. Nothing an already-placed window does moves any other, and opening
one never reflows what is open — the two properties every tiling model gives
up something to have, and that this one has for free by not tiling.

**One plane per workspace, and a workspace lives on one monitor at a time**, so
this is a canvas per workspace per monitor, each panned and zoomed where you
left it. Switching workspaces switches planes; the view over each is remembered
separately.

Each plane's coordinates are its own — nothing relates workspace 1's origin to
workspace 7's — so a window sent to another workspace cannot keep its numbers.
What crosses is its position on the *screen*: the offset it had from the corner
of its old view, it has from the corner of the new one, so it lands where it
looked like it was. Carrying the numbers instead fails twice over. The window
arrives somewhere off the screen, so sending it away looks like losing it; and
then focus follows it into view and drags the destination plane across to find
it, so the windows already on that plane are the ones that disappear. Sending
one window away moved everything.

**Panning moves the view, not the windows.** Zooming out draws them smaller
*without resizing them*: the client keeps the size it was configured with and
the compositor scales the buffer. That is the same bargain the overview and
solar's outer orbit make, and the reason a pan costs nothing — no client is
asked to relayout itself, at any point, for any view change.

**What falls outside the view is not drawn** and is reported to the compositor
as off screen, so a plane holding fifty windows costs what a plane holding four
does. A window overlapping the edge is a different case: it is drawn and
cropped, by the same clip a scrolled column gets.

**Focus follows minimally.** Focusing a window already on screen moves the view
not at all; focusing one off the edge pans the smallest distance that brings it
in, leaving the configured gap behind it, and never changes the zoom.

## Why the zoom stops at 1.0

The cap is about what is above it, not below. A buffer drawn larger than it
was painted is a blurry one, and the way to avoid the blur — reconfiguring
every client on every zoom step — is exactly the per-frame resize storm this
layout exists to avoid. Zooming *out* costs nothing: the compositor shrinks a
buffer it already has.

Below 1.0 the canvas is fully usable, pointer included. That was not true when
the layout was first written, and it is worth knowing what changed. The three
hit tests — `surface_under`, `window_under` and `clipped_out`, all in
`crates/viewport/src/state.rs` — asked which window was under the pointer using
the rectangle the `Space` holds, which is the window's *full* size. A window
drawn at 0.5 is stored at full size and painted half, so a click landed twice
as far into the client as it looked, and dragging a window took hold of it from
somewhere that was not under the hand. The error grew with the distance from
the window's corner, which is why it read as "the left of the window works and
the right of it does not" rather than as an offset anyone would spot at once.

`ViewportState::unscaled` takes a screen position back into the window's own
coordinates, about the same corner `RescaleRenderElement` scales it about
(`element_geometry().loc` — the window's top-left, not the surface's, because a
client drawing its shadows outside its geometry starts the surface up and left
of the window). It is identity at 1.0, which is every window in every layout
that does not shrink one, so nothing else pays for it.

Finding the right window is only half of it, and the half that is easy to
mistake for the whole. `surface_under` also returns a position, and the pointer
works out what to tell the client by subtracting that from the *real* pointer
position — which is in screen coordinates, while the client thinks in its own.
Returning the surface's actual origin therefore finds the window correctly and
then hands it a coordinate off by the entire scale error: zero at the window's
corner and growing across it, so the top-left of a window works and nothing
else quite does. What is returned is the position that makes the subtraction
come out in the client's coordinates, which at 1.0 is the plain sum it always
was.

## Where the state lives

Two maps in `data/shell/canvas.js`, both outside the window records because
both outlive them:

- `canvasPlaces` — view id to `{ x, y, width, height }` in world units. Width
  and height are the *client's* size, never the drawn one.
- `canvasViewports` — workspace to `{ x, y, zoom }`, where x and y are the
  world coordinate at the top left of the visible area.

The tiling tree still says which windows exist and which workspace they are on.
This layout reads it and never writes it — the same bargain `solar.js` and
`matrix.js` make, and the reason all three are additions rather than rewrites.
`window.move`, the session format and the overview keep working without knowing
the canvas exists.

## Where a new window goes

Two ways, and the first is the interesting one.

A window that was on screen a moment ago — because the layout was tiling until
you pressed the key that got you here — has a `view.box`, which is where it was
last drawn. Adopting that **freezes the layout you were looking at onto the
plane**: switching into the canvas leaves the desktop looking identical and
merely makes it draggable. Seeding at an origin instead would collapse a
working tiling layout into a pile in the corner.

Before either, the place the application left behind last time, if the session
had one. That is what makes a reload survivable: `location.reload()` is a new
page, so both maps come back empty and every window is replayed — and because
the plane *is* the layout here, losing it costs more than a reload costs any
other model. The places go into the session file keyed by application, exactly
as saved floating rects are, and are claimed as the windows return. The
viewports are saved too, so a reload does not scroll every plane back to where
it started.

Failing all of that — a window opened while the canvas is already running — the
window is put at the middle of the visible area at the default size, cascaded
past anything already sitting there.

The cascade compares against places **on that plane only**, and treats anything
within one cascade step as occupied. Both matter on a second monitor: comparing
against every place in the map made the first window on an empty screen arrive
several steps down and to the right for no reason visible on that screen, and
comparing origins for equality caught nothing, since two windows a pixel apart
are one window with another hidden behind it.

**A dialog opens on the window it belongs to**, centred, rather than in the
middle of the view. The rectangle a dialog arrives with was chosen to centre it
on the *screen*, which is the right answer in every layout where the window it
belongs to is also on the screen. Here the parent can be anywhere on a plane
with no edges, so a dialog placed by that rectangle opens attached to nothing
while the window that raised it sits off to one side. It is the one placement
question a canvas has that a screen-sized layout never had to ask.

The compositor names the parent on `view.added` (`parent`, absent when there is
none). It reads the same link to decide the window floats at all —
`wants_floating` looks at an xdg parent and an X11 `transient_for` — so this
only says *whose* rather than discovering anything new. A dialog whose parent
the compositor cannot name, or whose parent is not on this plane, falls back to
the rectangle it came with.

**A dialog opens on the window it belongs to**, centred, rather than in the
middle of the view. The rectangle a dialog arrives with was chosen to centre it
on the *screen*, which is the right answer in every layout where the window it
belongs to is also on the screen. Here the parent can be anywhere on a plane
with no edges, so a dialog placed by that rectangle opens attached to nothing
while the window that raised it sits off to one side. It is the one placement
question a canvas has that a screen-sized layout never had to ask.

The compositor names the parent on `view.added` (`parent`, omitted when there
is none). It already reads the same link to decide the window floats at all —
`wants_floating` looks at an xdg parent and an X11 `transient_for` — so this
says *whose* rather than discovering anything new. A dialog whose parent the
compositor cannot name, or whose parent is not on this plane, falls back to the
rectangle it came with.

Not every dialog knows its parent by then. One an application opens itself does
— the parent is set before the window ever commits — but a file chooser is the
*portal's* window, in another process, and its parent reaches the compositor
over xdg-foreign after an export and an import have gone round. By that point
the window has mapped, been announced with no parent, and been given a place in
the middle of the view. So `view.parent` says it afterwards, and the canvas
throws the place away and makes it again now that there is something to make it
from. Only a place the layout chose: a window that has been dragged was put
there by a person, and a late message from a client is not a reason to move it.

**Floating windows are on the plane too**, which is where this parts company
with solar and the matrix. Both leave them out because a dialog floats exactly
so that it will not be tiled, and giving one a slot is that decision made
twice. There is no such argument here: a plane is not a division of space, and
every window on one is placed by hand at a rectangle of its own. Floating is
what they all are.

Leaving them out is not neutral either — it is a window nailed to the screen,
sitting still while everything around it pans, which no amount of panning or
zooming can undo. Which windows the compositor decides to float is not
something you can see from the outside (`views.rs`, `wants_floating`, plus any
rule in the config file), so the fault arrives as "this one window ignores
me". A floating window keeps the rect it was opened with as its first place,
so a rule that says where an application opens still decides.

## Tunables

In `CANVAS`, at the top of `data/shell/canvas.js`:

| Name | Default | What it is |
| --- | --- | --- |
| `minZoom` | `0.25` | how far out the view may go |
| `maxZoom` | `1` | and how far in. See above before raising it |
| `zoomStep` | `1.25` | multiplicative, so out and back in returns to where it started |
| `panStep` | `240` | how far one pan key moves the view, in *screen* pixels — divided by the zoom at use, so a key moves the same visible distance however far out you are |
| `moveStep` | `120` | how far one move key carries a window, in *world* units — moving a window is an edit to the plane, so the same key makes the same edit whatever the view is doing |
| `width` / `height` | `0.45` / `0.55` | a new window's size, as a fraction of the visible area |
| `cascade` | `48` | how far each successive new window is offset from the last |
| `margin` | `48` | space kept around the plane when fitting all of it on screen |

The gap left when *following* focus is not among these: it is the configured
gap (`gaps` plus `gaps.outer`), read at the time of the pan, because the desktop
already has an answer to how much space belongs between a window and the edge of
the screen. Turning gaps off leaves a followed window flush against the edge.

## Keys

Bound only when the compositor is started in this layout; each is a no-op in
the others rather than an error.

| Key | What it does |
| --- | --- |
| `Mod4+[` / `Mod4+]` | pan left / right |
| `Mod4+PageUp` / `Mod4+PageDown` | pan up / down |
| `Mod4+-` / `Mod4+=` | zoom out / in (stops at 1:1) |
| `Mod4+Shift+f` | fit the whole plane on screen |
| `Mod4+Home` | back to 1:1 on the focused window |
| `Mod4+r` | size the focused window to the screen, less the gaps, without fullscreen |
| `Mod4+Shift+h/j/k/l` | move the focused window across the plane; the view follows |

`Mod4+r` is the resize chord every other layout spends on a resize mode, which
the canvas has no use for: a window here takes space from nothing, so there is
nothing to enter a mode about. What it does instead is the one resize that is
tedious by hand — give the focused window the screen, inset by the configured
gap, so it stops where the gaps begin and gets the edge a tiled window would
have had. Smart gaps do not apply: they are about a tiled workspace with nothing
to divide, which says nothing about a plane, and a lone window on one would
otherwise fill to the bare edge of the monitor.

It is not fullscreen (`Mod4+f`): the frame stays on, the windows behind
it stay where they are, and panning away leaves it the size it was. The size is
in world units, so at 0.5 the window is given twice the screen's width and
covers the screen at that zoom too.

And with the pointer:

| Gesture | What it does |
| --- | --- |
| `Mod4` + left drag on a window | move it across the plane |
| `Mod4` + right drag on a window | resize it |
| `Mod4` + left drag on the desktop | pan the plane |

All three arrive from the compositor as deltas in *screen* pixels — it knows
where the pointer went and nothing about the plane — so all three divide by the
zoom on the way in. That is what makes a drag track the pointer at any zoom
rather than running away from it by a factor of four when the view is out at
0.25.

The move is not clamped. A floating window is held against the edge of its
screen so that one dragged off it can be got back; a plane has no edge to hold
anything against, so dragging a window a long way away is a thing you are
allowed to do, and `Mod4+Shift+f` is how you find it again. The resize *is*
clamped, at `CANVAS.minSize`: a rectangle too small to take hold of is one that
cannot be grown again.

A place is never allowed below the client's own minimum size, because a place
*is* the size the client is asked to be — and asking is not the same as
getting. A client configured below its minimum may ignore it, so the compositor
raises the configure to that minimum instead of sending a request it knows will
be refused. It says so with `view.configured`, and the place takes the
correction; without it the shell holds a rectangle for a window of a different
size and every sum built on that rectangle is wrong by the difference, a dialog
centred on its parent by half of it. The minimum the shell knows is updated at
the same time, on the axis that was raised — a client may raise its minimum long
after it mapped, and the number from `view.added` goes stale. Below it the client keeps the size it
had while the frame does not, and the two then disagree about where everything
in the window is — a click lands where the page is laid out rather than where
it is drawn.

The same minimum is scaled with the picture. `addView` puts it on the element
so flexbox enforces it, which is right in a layout made of flexboxes and wrong
in one that draws a window smaller than it is: `min-width` is in drawn pixels
and beats `width`, so an unscaled minimum stops the element shrinking partway
through a zoom out. `reportGeometry` then divides the measured size by the
scale and asks the compositor to make the *client* bigger — so zooming out grows
every client instead of shrinking its picture, which is the resize storm this
layout exists to avoid arriving through the stylesheet. Chrome shows it first,
having the largest minimum of anything most people run.

The resize is the one gesture on the canvas that reconfigures a client — the
place is the client's size. That is what the gesture means, and it is a resize
done by hand rather than one the layout is doing sixty times a second, which is
the cost everything else here is arranged to avoid.

`Mod4+h/j/k/l` still moves focus, and panning deliberately does not share those
chords: reaching the window beside you should not depend on how far the view
has drifted.

## What is not done yet

- **Pinch to zoom.** The compositor already delivers pinch (`input.rs`, the
  `scale:` field) and three-finger swipe; neither is routed here yet.
- **A window carried to another monitor.** `window.move` slides a window across
  the plane and the view follows, so it never falls through to
  `moveViewToOutput` the way the tiling layouts do. Changing workspace is the
  only way across at the moment, which is how the other layouts got there
  before directional move existed.
