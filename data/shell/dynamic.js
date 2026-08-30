/* SPDX-License-Identifier: MIT
 *
 * Dynamic tiling arrangements: master-stack, spiral, bsp and grid.
 *
 * The manual tree is the default and is not touched by any of this. There, the
 * shape is whatever the splits you made say it is, and a window goes next to
 * the one in focus. A dynamic mode says the opposite: the shape is a function
 * of which windows are open, and opening one rearranges what is already there.
 *
 * So these are not new tree structures. They are one rule — given this
 * workspace's windows, in this order, what should the tree be — applied to the
 * same nodes the rest of the shell already understands. renderTree, resizing,
 * moving, the overview and the session format all keep working, because what
 * comes out is an ordinary tree.
 *
 * Rearranging is lazy. Doing it inside insertLeaf and removeLeaf would rebuild
 * the tree while session restore is walking it, so instead the window set is
 * compared against the last arrangement and the work happens on the way into a
 * relayout. That also leaves resize weights alone between arrangements: they
 * are reset when a window opens or closes, which is what every dynamic tiler
 * does, and survive everything else.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

const TILING_MODES = ['manual', 'master-stack', 'spiral', 'bsp', 'grid'];

/* A node's structure, with everything a rebuild would destroy left out.
 *
 * Two trees with the same shape hold the same windows, in the same nesting,
 * split the same ways — and differ only in the weights a divider drag put on
 * them. That is the test worth having: rebuilding is what resets weights, so
 * the arrangement should be swapped in only when it would actually look
 * different.
 *
 * This replaced a cache keyed on the window list. The window list is not the
 * whole input — the aspect of the screen is the other half, and bsp reads it —
 * so a workspace moved to a monitor of a different shape kept an arrangement
 * built for the old one until a window happened to open. Comparing what came
 * out instead of remembering what went in has no such gap, and it also keeps
 * weights across an aspect change that does not reach any of bsp's decisions,
 * which the old cache could not do either. */
function shapeOf(node) {
  if (node.type === 'leaf') return String(node.id);
  return `(${node.dir}:${node.children.map(shapeOf).join(' ')})`;
}

/* Every window id in a subtree, in tree order.
 *
 * Order is the whole input to an arrangement, and it is the tree's own —
 * which is what makes moving a window with Mod4+Shift do something sensible
 * in a dynamic mode. The move reorders the tree, and the next arrangement is
 * built from the order the move produced. */
function dynamicOrder(node, out = []) {
  if (node.type === 'leaf') {
    out.push(node.id);
    return out;
  }
  for (const child of node.children) dynamicOrder(child, out);
  return out;
}

/* A right-leaning nest: the first window takes half, and everything after it
 * shares the other half by the same rule.
 *
 * `pick` chooses each level's direction, and is the only difference between
 * spiral and bsp. Both produce this shape; they disagree about which way to
 * cut, which is the thing you can actually see.
 *
 * `w` and `h` are the region's proportions rather than pixels. The shell never
 * computes rectangles — the browser does that and geometry.js measures it —
 * but with equal weights a split halves one side exactly, so the aspect at any
 * depth is arithmetic rather than a measurement. That is what lets bsp cut
 * along the longer side without waiting for a render.
 */
function nest(ids, pick, w, h) {
  if (ids.length === 0) return null;
  if (ids.length === 1) return newLeaf(ids[0]);

  const dir = pick(w, h, ids.length);
  const split = newSplit(dir);
  const [half, rest] = dir === 'horizontal' ? [w / 2, h] : [w, h / 2];
  split.children = [newLeaf(ids[0]), nest(ids.slice(1), pick, half, rest)];
  return split;
}

/* Alternate, starting across. The classic fibonacci spiral: each window takes
 * half of what is left, turning ninety degrees every time. */
function spiralPick(_w, _h, remaining) {
  return remaining % 2 === 0 ? 'horizontal' : 'vertical';
}

/* Cut along the longer side, so no window is driven to a silly shape. This is
 * what separates bsp from the spiral: on a wide screen the first cut is
 * vertical in both, and from there bsp keeps answering the region in front of
 * it rather than following a fixed turn. */
function bspPick(w, h) {
  return w >= h ? 'horizontal' : 'vertical';
}

/* One large window, the rest in a column beside it.
 *
 * The master is the first in order, which is the one that has been there
 * longest unless something moved. Mod4+Shift+h and l reorder, so promoting a
 * window to master is moving it to the front — no separate command needed. */
function masterStack(ids, root) {
  root.dir = 'horizontal';
  if (ids.length <= 1) {
    return ids.length === 1 ? [newLeaf(ids[0])] : [];
  }
  const stack = newSplit('vertical');
  stack.children = ids.slice(1).map((id) => newLeaf(id));
  return [newLeaf(ids[0]), stack];
}

/* Every window the same size, in rows.
 *
 * The one arrangement here that gives no window preference over any other:
 * master-stack picks a master, the nest hands each window half of what the one
 * before it left, and both are the right answer when one thing matters more
 * than the rest. A wall of terminals watching four services is the case where
 * none of them does, and there the nest is actively wrong — the fourth log gets
 * an eighth of the screen for no reason anybody chose.
 *
 * Rows of equal cells rather than a fixed lattice: with N not a multiple of the
 * row count, the last row holds fewer windows and they are wider. The
 * alternative is a lattice with a hole in it, and a hole is screen nobody can
 * use. Every row is the same height either way, so the sizes stay within a
 * factor of two of each other however ragged the count is.
 *
 * Order is the tree's, so Mod4+Shift+h and l move a window through the grid in
 * reading order, exactly as they reorder the stack in master-stack.
 */

/* What a cell is aimed at being, as width over height.
 *
 * Slightly wide rather than square: a window is a piece of text or a terminal
 * far more often than it is a photograph, and the shape that ruins one of those
 * is the tall narrow one. 4:3 is also close enough to square that the row count
 * it picks is the one you would draw by hand on an ordinary screen — four
 * windows on 16:9 come out 2x2, nine come out 3x3 — and it is only on a 32:9,
 * where a square cell would mean five rows of stamps, that it visibly differs. */
const GRID = { cellAspect: 4 / 3 };

/* How many rows this many windows divide into on a screen of these proportions.
 *
 * Every row count from one to N is tried and the one whose cells come out
 * closest to GRID.cellAspect wins. `w` and `h` are the tiling area's
 * proportions, the same pair bsp cuts along, so the answer follows the screen:
 * four windows are 2x2 on 16:9, one row of four on 32:9 and three rows on a
 * monitor turned on its end.
 *
 * Compared in logs, so that a cell twice as wide as it should be and one half
 * as wide score the same. A ratio of ratios compared linearly does not — it
 * bottoms out at zero on one side and runs to infinity on the other, which
 * silently biases every choice towards the taller of two equally wrong shapes.
 *
 * The loop is N iterations of arithmetic, run when the window set changes. A
 * closed form exists — rows is about sqrt(cellAspect * h * N / w) — but it has
 * to be rounded to an integer afterwards, and rounding the square root is not
 * always the integer that scores best. */
function gridRows(count, w, h) {
  if (count <= 1) return 1;

  let best = 1;
  let bestScore = Infinity;
  for (let rows = 1; rows <= count; rows++) {
    /* count/rows windows in each row, so a cell is w/(count/rows) wide and
       h/rows tall. Proportions only: the shell computes no rectangles. */
    const aspect = (w * rows * rows) / (h * count);
    const score = Math.abs(Math.log(aspect / GRID.cellAspect));
    /* Strictly better, so a tie keeps the fewer rows — which on a landscape
       screen is the wider row, and the one worth having. */
    if (score < bestScore - 1e-9) {
      bestScore = score;
      best = rows;
    }
  }
  return best;
}

/* How many windows go in each row, as evenly as they divide.
 *
 * The remainder is spread over the first rows rather than piled into the last,
 * so a grid of five on two rows is three then two — the reading order most
 * grids have — and never four then one. */
function gridCounts(count, rows) {
  const base = Math.floor(count / rows);
  const extra = count % rows;
  return Array.from({ length: rows },
    (_, row) => base + (row < extra ? 1 : 0));
}

/* The rows, as tree nodes.
 *
 * A row holding one window is that window rather than a split around it. Both
 * render the same, but the tree is what focus_parent walks and what the session
 * writes down, and a container with one child in it is a level of nesting that
 * means nothing to anybody reading either. */
function grid(ids, w, h) {
  if (ids.length === 0) return { dir: 'horizontal', children: [] };

  const rows = gridRows(ids.length, w, h);
  if (rows <= 1) {
    return { dir: 'horizontal', children: ids.map((id) => newLeaf(id)) };
  }

  const children = [];
  let at = 0;
  for (const take of gridCounts(ids.length, rows)) {
    const row = ids.slice(at, at + take);
    at += take;
    if (row.length === 1) {
      children.push(newLeaf(row[0]));
      continue;
    }
    const split = newSplit('horizontal');
    split.children = row.map((id) => newLeaf(id));
    children.push(split);
  }
  return { dir: 'vertical', children };
}

/* The proportions of the area a workspace's windows are laid out in, for bsp's
 * cuts and the grid's row count.
 *
 * The *tiling area*, not the output. The bar and any panel's exclusive zone
 * come off it before a window is placed anywhere, and on a screen that is close
 * to square those thirty pixels are the difference between a first cut across
 * and a first cut down. Measured, for the same reason everything else here is:
 * what those insets are is in the stylesheet and in a layer-shell client, and
 * `.windows` is the element they have already been taken out of.
 *
 * Falls back to a landscape guess when the workspace is not on screen — an
 * arrangement still has to be built for it, and there is nothing to measure.
 * Only the ratio is ever used, so the units do not matter and 16:9 is as good
 * a way to write the guess as 1920x1080.
 *
 * This read `output.width` and `output.height`, which no output record has ever
 * had: syncOutputs stores them under `output.rect`. Both comparisons were
 * `undefined > 0`, so the guess was not a fallback, it was the only answer bsp
 * ever got. On 16:9 that is invisible — bspPick compares w against h and
 * nothing else, so the guess and the truth agree at every level — which is why
 * it survived. On a rotated monitor it inverts the very first cut, and on a
 * 32:9 it is wrong by a factor of two from the start. */
function workspaceAspect(workspace) {
  const area = windowsAreaOf(workspace);
  if (area) {
    const w = area.right - area.left;
    const h = area.bottom - area.top;
    if (w > 0 && h > 0) return [w, h];
  }
  return [16, 9];
}

/* What one workspace's shape should be, given its windows and its screen. */
function arrangementFor(workspace, root) {
  const ids = dynamicOrder(root);
  const mode = tilingModeOf(workspace);

  if (mode === 'master-stack') {
    /* masterStack sets the root's direction itself, which is why it is handed
       the root rather than asked for children alone. */
    return { dir: 'horizontal', children: masterStack(ids, root) };
  }

  const [w, h] = workspaceAspect(workspace);

  if (mode === 'grid') return grid(ids, w, h);

  const pick = mode === 'spiral' ? spiralPick : bspPick;
  /* The root is one split already, so the first cut is its direction and the
     nest continues underneath it. */
  const dir = pick(w, h, ids.length);
  const [half, rest] = dir === 'horizontal' ? [w / 2, h] : [w, h / 2];
  return {
    dir,
    children: ids.length === 0 ? []
      : ids.length === 1 ? [newLeaf(ids[0])]
        : [newLeaf(ids[0]), nest(ids.slice(1), pick, half, rest)],
  };
}

/* Rebuild one workspace's shape, if the mode asks for one and what it asks for
 * is not what is already there. */
function arrangeWorkspace(workspace) {
  if (layoutModeOf(workspace) !== 'tiling' || tilingModeOf(workspace) === 'manual') return;

  const root = workspaces.get(workspace);
  if (!root) return;

  const wanted = arrangementFor(workspace, root);

  /* Nothing to do if it already looks like this. Cheaper than it reads — the
     candidate is a couple of dozen small objects and is thrown away — and it
     is what keeps a divider drag from resetting the weight it is setting: a
     drag changes weights and nothing else, so the shape is unchanged on every
     one of the relayouts it runs. */
  const before = shapeOf(root);
  const after = shapeOf({ type: 'split', dir: wanted.dir, children: wanted.children });
  if (before === after) return;

  root.dir = wanted.dir;
  root.children = wanted.children;
  root.layout = 'split';
  treeGeneration++;
}

/* Every workspace, on the way into a relayout. */
function arrangeAll() {
  for (const workspace of workspaces.keys()) arrangeWorkspace(workspace);
}
