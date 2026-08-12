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
in, with a margin, and never changes the zoom.

## Why the zoom stops at 1.0

Not taste. `surface_under` in `crates/viewport/src/state.rs` hit-tests a click
against a window's mapped rectangle and there is no scale term in it, so at any
zoom but 1.0 a click lands somewhere other than where it looks like it landed.
The overview sidesteps this by taking every click for the shell while it is up;
a canvas cannot, because the windows on it are the thing you are using.

Above 1.0 there is a second problem, independent of the first: a buffer drawn
larger than it was painted is a blurry one, and the alternative — reconfiguring
every client on every zoom step — is exactly the per-frame resize storm the
layout exists to avoid.

So the cap is where the current compositor is correct:

| Zoom | What works |
| --- | --- |
| `1.0` | everything. Pan an endless plane, click and type into anything on screen — every surface is at its natural scale and the compositor's arithmetic is right |
| below `1.0` | looking, panning, fitting, moving windows. Clicks into a shrunken client are **not** to be trusted |

`Mod4+Home` is therefore the key that matters most: back to 1:1 on whatever is
focused. Lifting the cap means teaching `surface_under`, `window_under` and
`clipped_out` about `view.scale`, which is already stored per view in
`views.rs` and already ignored by all three.

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

Failing that — a restored session, a window opened while the canvas is already
running — the window is put at the middle of the visible area at the default
size, cascaded past anything already at that exact spot.

Floating windows are left out of the plane entirely, as they are in solar and
the matrix. A dialog is floating because the compositor judged that tiling it
is the wrong thing to do; putting one on the plane is that same mistake with
different arithmetic.

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
| `margin` | `48` | space kept between a followed window and the edge of the screen |

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
| `Mod4+Shift+h/j/k/l` | move the focused window across the plane; the view follows |

`Mod4+h/j/k/l` still moves focus, and panning deliberately does not share those
chords: reaching the window beside you should not depend on how far the view
has drifted.

## What is not done yet

- **Clicks below 1.0 zoom**, per the section above. The compositor change is
  scoped and separate.
- **Pointer panning and drag-to-move.** The keys are bound; grabbing the
  background to pan, or a titlebar to drag a window across the plane, is not
  wired up. `resize.js` already has the drag machinery a floating window uses.
- **Pinch to zoom.** The compositor already delivers pinch (`input.rs`, the
  `scale:` field) and three-finger swipe; neither is routed here yet.
- **Session persistence.** Places live for the session. A restart re-seeds
  them, which lands every window in a cascade at the middle of the view rather
  than where it was. The session format would need a per-window world rect.
