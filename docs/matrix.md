# The matrix layout

The focused window large on the left, the focus history halving away down the
right.

```
+--------------------------+-----------+
|                          |  MRU 1    |   50% of the column
|                          |           |
|                          +-----------+
|        FOCUSED           |  MRU 2    |   25%
|         60%              +-----------+
|                          |  MRU 3    |   12.5%
|                          +-----------+
|                          | MRU 4 (+7)|   the rest, stacked
+--------------------------+-----------+
```

Turn it on with `"layout": "matrix"` in the config file, `--layout matrix` on
the command line, or `shell layout.model matrix` at runtime.

## The model

**Order is focus history.** Not the tree, not the order windows opened in.
Index 0 of the history is what you are typing into, index 1 is what you were
typing into before that — the order Alt+Tab steps through, which is exactly
what the right-hand column is showing you. The window you keep coming back to
keeps a big slot; the one you have not touched in an hour sinks.

**The primary takes 60% of the width**, full height, on the left. It is
whatever is focused, so the window being worked in is never the small one.

**The column halves.** The second-most-recent window takes half the column, the
third half of what is left, the fourth half of that. Each slot is about half
its neighbour above.

**The halving stops at a minimum height** (100px), and that bound is what makes
the depth logarithmic: a 1050px column holds four slots and never more, whether
five windows are open or fifty. Windows past the bound are stacked in the last
slot, most recent on top, with a count badge; the ones underneath are kept in
the DOM so their clients stay alive, are drawn nowhere, and are reported to the
compositor as off screen so their surfaces are not painted into a hole that is
not there.

**The last slot placed takes the remainder** rather than half of it — either
because the column ran out of depth or because there is no further window to
give the other half to. So the column is always full to the bottom, and two
windows means 60/40 rather than 60/20-and-a-hole.

## Why it is arithmetic

Every other tiling layout here renders to nested flexboxes and lets the browser
compute the rectangles. This one cannot: what a window gets is a function of
how recently it was focused, and there is no arrangement of rows and columns
that says "each one takes half of what the one before it left". `solar.js` is
the other exception, for the same kind of reason.

So `matrix.js` computes rectangles and writes them as inline style — and they
are then *measured* by `geometry.js` like every other window's, so a window
mid-transition is reported at wherever it actually got to rather than at where
it was aimed.

## The kernel

```js
calculateLayout(windows, screen, options?) -> WindowGeometry[]
```

`windows` is ids (or records with an `id`) in MRU order, index 0 focused.
`screen` is `{ x, y, width, height }`. Out comes one
`{ id, tier, slot, x, y, width, height, hidden }` per window, in absolute
pixels. `tier` is `primary`, `slot` or `stacked`; `slot` is `-1` for the
primary and the depth otherwise; `hidden` marks a window buried under the top
of the stack — it still carries the rectangle it would occupy, so coming
forward is a change of visibility rather than a second calculation.

Pure, total and deterministic: no DOM, no globals, no clock. The same two
arguments always give the same rectangles, every window handed in comes back
out with one, and it is O(N) in a single pass. `tests/shell.test.js` checks the
arithmetic directly for that reason — `node tests/shell.test.js data/shell
matrix`.

Nothing here runs on a timer. The layout is recomputed on exactly four events:
a window opened, a window closed, focus moved, and the screen resized.
`motion.js` tweens between the rectangles it produces, as it does for every
other layout.

## State

One array, `focusStack`, for the whole session — not one per workspace. A
window carried to another workspace keeps its place in the history, so the
thing you were just in stays the thing you were just in. `matrixFocused()` is
one splice and an unshift; that is the entire state transition for a focus
change.

A workspace's order is that array filtered to the windows on it. Windows the
history has never heard of come last in tree order, which is what a session
restore looks like: every window is replayed before anything is focused, so
tree order is the only order there is until it is.

Floating windows are left out entirely, as they are in solar. A dialog is
floating because the compositor judged that tiling it is the wrong thing to do,
and giving one a slot is that same mistake with different arithmetic.

## Tunables

In `MATRIX`, at the top of `data/shell/matrix.js`:

| Name | Default | What it is |
| --- | --- | --- |
| `primaryRatio` | `0.60` | the focused window's share of the width |
| `minSlotHeight` | `100` | how short a slot may get before the column stops dividing — the bound on the depth |
| `gap` | `8` | between the primary and the column, and between slots; matches `--gap` in `shell.css` |

## Keys

None of its own. Focus is the only input the layout has, so `Mod4+h/j/k/l`,
`Mod4+Tab` and clicking a window are all the interaction there is: focus a
window and it becomes the primary, with the one it displaced at the top of the
column.
