# Layouts as an extension point

The compositor has no layout policy. A window maps only after the shell sends
`view.layout`. Adding a sixth model is writing a file that draws holes and
measures them — not teaching Rust where windows go.

This is the contract a layout implements. Five ship: tiling, scrolling, solar,
matrix, canvas. A sixth is a new file, a `<script>` tag, and three name lists
that must stay in agreement. There is no plugin loader: the shell is classic
scripts on a `file://` page, and dynamic import would fight packaging, tests
and the shared globals the rest of the desktop is built on.

## How a layout is chosen

Every layout file is loaded, always, in the order `index.html` lists. Selection
is a name, not a load:

- `"layout"` in the config file, or `--layout NAME`
- `shell layout.model [name]` at runtime — a name, or no argument to cycle

`LAYOUT_MODES` in `data/shell/state.js`, `LAYOUTS` in
`crates/viewport/src/state.rs` (`apply_config`), and the `--layout` help text
are the same five strings. A name the compositor does not know is warned and
ignored; a name the compositor knows and the shell does not is a keymap built
for a model that never draws.

`tests/shell.test.js` reads the `<script>` tags out of `index.html` rather
than keeping its own list. Do not add a second one.

## The shared world

All five read one tree (`workspaces`: split nodes and leaves). Tiling and
scrolling write it. Solar, matrix and canvas must not. Window move, session
restore and the overview stay layout-agnostic because of that.

`relayoutAll()` in `geometry.js` is the pipeline:

1. `arrangeAll()` — tiling dynamic modes only
2. Clear the models that are not running (`clearSolarState`,
   `clearMatrixState`, `clearCanvasState`)
3. If this model plans: `planSolar` / `planMatrix` / `planCanvas` — a
   `Map(outputName → placements)` *before* any draw, so a second monitor is
   not an afterthought
4. Per output: `renderOverview`, or this model's `render*`, or `renderStrip`,
   or `renderTree`
5. Toggle the class on `.windows` (`scrolling` / `solar` / `matrix` /
   `canvas`)
6. Floating windows appended after the tree — except canvas, which already
   placed every window, and the overview
7. `reportGeometry` measures the holes and sends `view.layout`

## Required surface

Not a class. Functions in the shared global scope, named for the model.

| Hook | What it does |
| --- | --- |
| `planX()` | Return a `Map` from output name to the items that output will draw. Called before any DOM write. |
| `renderX(items, output)` | Build or reuse the container, write style (`left`/`top`/`width`/`height` or flex), set `renderedIds`, stash `view.x` for scale / lift / clip. Return the container element. |
| `clearXState()` | Called when this model is not the one running. Undo inline geometry, `hidden`, dimming, and the class on `.windows`. Canvas keeps its places: those are the plane, not the presentation. |

Optional, and worth having: a pure kernel (`solarPlacements`, `calculateLayout`,
`canvasProject`) that takes numbers and returns rectangles. `tests/shell.test.js`
exports those on `__shell` and never stubs the arithmetic.

Tiling and scrolling have no `plan*` — the browser is the kernel. They still
render (`renderTree`, `renderStrip`) and they still measure.

## The wire

Write style. Measure. Send. Do not invent a compositor rectangle.

`reportGeometry(id)`:

- `getBoundingClientRect` on `.viewport`
- `scale` = `view.overview?.scale ?? view.solar?.scale ?? view.canvas?.scale ?? 1`
- size **÷ scale** (the client's size, not the drawn size)
- `clip` against the output `.windows`, or `view.overview.cell` / `view.solar.cell`
- `floating` if the window is lifted (a float, the solar sun, a focused canvas window)
- optional `frame`, `square`
- `view.layout` `{id,x,y,width,height}` plus those extras
- zero size → `view.visible: false`

Arithmetic layouts measure `.windows` via `*AreaOf` and subtract `edgeGapPx`
(inner + outer, honouring smart gaps). The origin is the padding corner, not
the page: a second monitor offset by the first is the bug that rule exists to
prevent.

Opacity of client contents is `view.opacity`, never CSS. CSS opacity on a hole
dims a rectangle the compositor is about to paint over.

## Invariants

**Measure, never assume.** A hole moves for reasons no message announces —
transitions, font load, a reflow three ancestors up. `ResizeObserver` is what
notices.

**No transform-as-effect on a measured node.** Frames, `.viewport`, thumbnails,
the strip, anything handed to `setOverlay`. A transform is a new geometry every
frame. FLIP may invert then clear. The strip's `translateX` *is* the scroll
position and is measured. Decorative spin or scale on a hole is forbidden.

**Do not animate size through FLIP.** That reconfigures clients sixty times a
second. Position only.

**No compositor-side geometry.** No `recalculate_*_layout` in Rust. Config,
binds, and the wire fields (`scale`, `opacity`, `floating`, `clip`) only.

**Do not write the tree** unless you *are* tiling or scrolling. Solar, matrix
and canvas read it.

**Do not leave inline rects, `hidden`, or dim behind** when the user switches
away. That is why `clear*State` exists.

**Drawn scale, not client resize,** when a window should look smaller without
reflowing (overview, solar's outer orbit, canvas zoom). Canvas zoom stops at
1:1: past that is blur or a resize storm.

**Floating is `relayoutAll`'s job** unless the model absorbs it. Canvas does:
every window already has a place on the plane, and writing `floating` on one
fights those coordinates.

**Opening a window must not move the others** unless that *is* the model
(tiling, dynamic tiling). Scrolling, solar, matrix and canvas exist so it
does not.

**No forever loops.** An idle paint is a desktop cost.

## Session

The compositor stores an opaque blob. The shell's format is `version: 1`.

Always written: the tree (slots claimed by `app_id` / `title`), floating rects,
output → workspace. 45 seconds, then unclaimed slots drop.

`saved.layout` is written and **not** restored. The live config / `layoutMode`
wins, so switching model and reloading rearranges the same tree.

| Layout | Extra persist |
| --- | --- |
| tiling | weights, `dir`, `layout` (split / tabbed / stacked), `active` |
| scrolling | `width` on nodes (column fraction) |
| solar | none (sun, spin and field are not saved) |
| matrix | `focusStack` is **not** saved; unknown ids last, tree order |
| canvas | `canvas: {places, viewports}` whenever non-empty, even off-canvas |

Places survive a layout switch for the same reason the tree does. An old file
without a `canvas` key is a session that never ran canvas.

## Commands and binds

The compositor keymap is layout-conditional: a few chords only exist in one
model. A new chord needs `binding.rs` and the same name on `LAYOUTS`, or the
key is dead in every model that is not the one you tested.

## Tests

- Export the kernel on `__shell`
- `node tests/shell.test.js data/shell NAME` and `… NAME session`
- Do not stub rectangles for arithmetic. Structure tests live here; pixels
  live in `tests/layout.test.js` against a real compositor

## Adding a sixth

Today, without a loader:

1. Write `data/shell/foo.js` — `planFoo`, `renderFoo`, `clearFooState`, and a
   pure kernel if the model is arithmetic
2. Add `<script src="foo.js"></script>` to `index.html` in load order (after
   the tree, before `session.js`)
3. Append `'foo'` to `LAYOUT_MODES` in `state.js`
4. Append `"foo"` to `LAYOUTS` in `state.rs` and to `--layout` help
5. Branch `relayoutAll` for plan / render / clear / the class on `.windows`
6. Bindings, if the model has chords of its own
7. A page in `docs/` and a row in `docs/configuration.md`
8. `node tests/shell.test.js data/shell foo` and the `session` variant

A registry (`layouts.set('foo', {plan, render, clear})`) would replace the
nested ternary. It is not a user-drop-in plugin, and it is not needed to write
the sixth file. Do not invent a loader until the three allow-lists hurt more
than they document.

## Anti-patterns

- A timer that paints forever
- A rectangle assumed from the tree, the last `view.layout`, or the config
- A transform used as decoration on a measured node
- A reader layout mutating the tree
- Inline `left` / `top` left behind after a switch
- Reconfiguring clients to draw them smaller
- A second list of layout names anywhere but the three above
