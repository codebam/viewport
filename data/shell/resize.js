/* SPDX-License-Identifier: MIT
 *
 * Resizing.
 *
 * By dragging the gap between two windows — which is a real element, so it needs
 * no compositor support at all — or by keyboard in resize mode.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */
/* ------------------------------------------------------------------------
 * Resizing
 * --------------------------------------------------------------------- */

const MIN_WEIGHT = 0.15;

/* Bumped whenever the tree changes shape. A divider drag captures the value it
 * started with and stops if it no longer matches: closing a window mid-drag
 * would otherwise leave the handler shifting weight between siblings that have
 * moved or ceased to exist, stranding a window at whatever fraction it held. */
let treeGeneration = 0;

/* Shift weight between two children, keeping their total constant so the rest
 * of the layout does not shuffle.
 *
 * By node rather than by index, because the two the user is dragging between
 * are not always adjacent in `children`: a leaf whose window is not on screen
 * — an unclaimed session slot, an empty split — renders nothing, so the two
 * sides of a gap can have anything in between them in the tree. */
function shiftWeightBetween(a, b, fraction) {
  if (!a || !b) return false;

  const total = (a.weight ?? 1) + (b.weight ?? 1);
  const next = Math.min(Math.max((a.weight ?? 1) + fraction * total,
    MIN_WEIGHT), total - MIN_WEIGHT);

  if (next === a.weight) return false;
  a.weight = next;
  b.weight = total - next;
  return true;
}

/* By index, for the callers that have one: the keyboard paths walk the tree
 * and know where in `children` they are. */
function shiftWeight(parent, index, fraction) {
  return shiftWeightBetween(
    parent.children[index], parent.children[index + 1], fraction);
}

/* `before` and `after` are the two nodes the gap is between, which is what the
 * renderer knows and an index into `children` is not. */
function beginDividerDrag(event, node, before, after) {
  event.preventDefault();
  event.stopPropagation();

  const horizontal = node.dir === 'horizontal';
  const container = event.currentTarget.parentElement;
  const extent = horizontal
    ? container.getBoundingClientRect().width
    : container.getBoundingClientRect().height;
  if (extent <= 0) return;

  let last = horizontal ? event.clientX : event.clientY;
  const generation = treeGeneration;

  const onMove = (move) => {
    if (generation !== treeGeneration) {
      onUp();
      return;
    }
    const now = horizontal ? move.clientX : move.clientY;
    const delta = now - last;
    if (delta === 0) return;
    last = now;
    if (shiftWeightBetween(before, after, delta / extent)) gestureRelayout();
  };

  const onUp = () => {
    endGesture();
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
  };

  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
}

/* Nearest ancestor split running along `axis`, with the child on the path.
 * Resizing "width" means adjusting the closest horizontal container, which is
 * what makes it feel local rather than reshaping the whole workspace. */
/* Drag the edge between two columns. Unlike a tiling divider this only ever
 * changes the column on its left: the one on the right keeps its width and the
 * strip shifts, which is what makes the model predictable — nothing you are not
 * touching changes size. */
function beginColumnDrag(event, workspace, column) {
  event.preventDefault();
  event.stopPropagation();

  const area = windowsAreaOf(workspace);
  const extent = area ? area.right - area.left : 0;
  if (extent <= 0) return;

  let last = event.clientX;
  const generation = treeGeneration;
  /* The element only for the frames before the first relayout: renderStrip
     builds a new strip every render, so what keeps the transitions off for the
     rest of the drag is the workspace it reads back out of state. */
  const strip = event.currentTarget.parentElement;
  strip?.classList.add('dragging');
  columnDragWorkspace = workspace;

  const onMove = (move) => {
    if (generation !== treeGeneration) {
      onUp();
      return;
    }
    const delta = move.clientX - last;
    if (delta === 0) return;
    last = move.clientX;

    const next = (column.width ?? COLUMN_WIDTHS[1]) + delta / extent;
    column.width = Math.max(0.1, Math.min(next, 1));
    gestureRelayout();
  };

  const onUp = () => {
    columnDragWorkspace = null;
    strip?.classList.remove('dragging');
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
    /* A relayout either way: the class is drawn from the state cleared just
       above, so a drag whose idle timer has already fired — a hand that
       stopped moving before it let go — still needs one render to put the
       transitions back. */
    if (isGesturing()) endGesture();
    else relayoutAll();
  };

  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
}

function ancestorOnAxis(id, axis) {
  const found = findLeaf(id);
  if (!found) return null;

  const root = workspaces.get(found.workspace);
  let child = found.leaf;
  let parent = found.parent;

  while (parent) {
    if (parent.dir === axis && parent.children.length > 1) {
      return { parent, index: parent.children.indexOf(child) };
    }
    child = parent;
    parent = findParentOf(root, parent);
  }
  return null;
}

/* Keyboard resize, one step at a time. sway's resize mode maps left/right to
 * shrink/grow width and up/down to shrink/grow height. */
/* How much one press of resize mode moves an edge of a floating window.
 *
 * Pixels rather than a fraction: a floating window has no container to take a
 * share of, so there is nothing for a fraction to be a fraction of. */
const FLOAT_RESIZE_STEP = 40;

function resizeFocused(direction) {
  if (focusedId == null) return;

  /* On the canvas a window's place is its size and nothing shares space with
     it, so resize mode changes that rather than a weight no renderer there
     reads — the same branch `window.move` has, and for the same reason.
     Before the floating check, because on the plane a floating window is on
     the plane like every other one. */
  if (layoutModeOf() === 'canvas') {
    const step = (direction === 'left' || direction === 'up')
      ? -FLOAT_RESIZE_STEP : FLOAT_RESIZE_STEP;
    const horizontal = direction === 'left' || direction === 'right';
    canvasResizeBy(focusedId, horizontal ? step : 0, horizontal ? 0 : step,
      false, false);
    return;
  }

  /* A floating window is not in the tree, so the lookup below finds nothing
     and resize mode did nothing at all for one — every press ignored, with no
     way to tell that from a binding that never fired. It simply grows: there
     are no siblings to take the space from. */
  if (floatingOf(focusedId)) {
    const step = (direction === 'left' || direction === 'up')
      ? -FLOAT_RESIZE_STEP : FLOAT_RESIZE_STEP;
    const horizontal = direction === 'left' || direction === 'right';
    resizeByDelta(focusedId, horizontal ? step : 0, horizontal ? 0 : step);
    return;
  }

  const axis = (direction === 'left' || direction === 'right')
    ? 'horizontal' : 'vertical';
  const grow = direction === 'right' || direction === 'down';

  const target = ancestorOnAxis(focusedId, axis);
  if (!target) return;

  const step = 0.05;
  const { parent, index } = target;

  /* The last child has no neighbour after it, so grow/shrink against the one
   * before it instead — otherwise resizing the rightmost window does nothing. */
  const usePrevious = index === parent.children.length - 1;
  const pairIndex = usePrevious ? index - 1 : index;
  const fraction = (usePrevious ? -step : step) * (grow ? 1 : -1);

  if (shiftWeight(parent, pairIndex, fraction)) relayoutAll();
}

/* Mod4 + right drag, forwarded by the compositor as a pixel delta and the
 * corner the hand took hold of.
 *
 * The corner is the compositor's answer to a question only it can see: which
 * half of the window the press landed in. Without it every drag was a pull on
 * the bottom right corner, so the two edges nearest the pointer in half the
 * grabs were the two that never moved. */
const RESIZE_EDGES = {
  'top': { west: false, north: true, horizontal: false, vertical: true },
  'top-left': { west: true, north: true, horizontal: true, vertical: true },
  'top-right': { west: false, north: true, horizontal: true, vertical: true },
  'bottom': { west: false, north: false, horizontal: false, vertical: true },
  'bottom-left': { west: true, north: false, horizontal: true, vertical: true },
  'bottom-right': { west: false, north: false, horizontal: true, vertical: true },
  'left': { west: true, north: false, horizontal: true, vertical: false },
  'right': { west: false, north: false, horizontal: true, vertical: false },
};

/* Anything unnamed is the bottom right, which is what a resize was before the
   corner was sent and what the keyboard paths still mean: `resize grow right`
   moves the right edge whichever half the pointer happens to be sitting in. */
function edgesOf(name) {
  return RESIZE_EDGES[name] ?? RESIZE_EDGES['bottom-right'];
}

/* Shift one axis of a tiled window's share of its container.
 *
 * `fromStart` is a pull on the left or top edge. Which sibling gives up the
 * space changes with it: the right edge trades with the window after this one
 * and the left edge with the one before, so that in both cases the edge under
 * the hand is the one that moves and the far edge stays where it was. */
function resizeAxis(id, axis, delta, fromStart) {
  const target = ancestorOnAxis(id, axis);
  if (!target) return false;

  const el = views.get(id)?.el?.parentElement;
  const extent = el
    ? (axis === 'horizontal'
      ? el.getBoundingClientRect().width
      : el.getBoundingClientRect().height)
    : 0;
  if (extent <= 0) return false;

  const { parent, index } = target;
  /* Growth in pixels: dragging the left edge left is a bigger window. */
  const growth = fromStart ? -delta : delta;

  /* The neighbour the edge under the hand faces, where there is one. At the
     ends of a container there is not — the last window has nothing to its
     right, the first nothing to its left — so it trades with the other side
     instead, which is what keeps a drag on the outermost edge from doing
     nothing at all. */
  const towardsNeighbour = fromStart ? index > 0 : index < parent.children.length - 1;
  const pairIndex = (fromStart === towardsNeighbour) ? index - 1 : index;
  /* shiftWeight grows `children[pairIndex]` and shrinks the one after it, so
     the sign is whether this window is the first of the pair. */
  const fraction = (pairIndex === index ? growth : -growth) / extent;

  return shiftWeight(parent, pairIndex, fraction);
}

/* Widen or narrow the column a window is in, as a fraction of the output.
 *
 * Columns do not share space: widening one does not take anything from its
 * neighbours, it makes the strip longer and shifts everything after it along.
 * That is the model — a column keeps the width it was given no matter what
 * happens elsewhere — so there is nothing here that resizes an adjacent
 * window, unlike a tiling split. */
function resizeColumn(workspace, id, dx, west) {
  const root = workspaceRoot(workspace);
  const column = root.children[columnIndexOf(workspace, id)];
  if (!column) return false;

  const area = windowsAreaOf(workspace);
  const extent = area ? area.right - area.left : 0;
  if (extent <= 0) return false;

  /* Dragging the left edge left widens the column: the strip has no fixed
     origin to pin the far edge against, so what the corner changes here is
     only which way the pointer has to go. */
  const growth = west ? -dx : dx;
  const next = (column.width ?? COLUMN_WIDTHS[1]) + growth / extent;
  column.width = Math.max(0.1, Math.min(next, 1));
  return true;
}

function resizeByDelta(id, dx, dy, edge) {
  const { west, north, horizontal, vertical } = edgesOf(edge);
  if (!horizontal) dx = 0;
  if (!vertical) dy = 0;

  /* On the canvas a window's place is its size, and nothing shares space with
     it, so the drag simply changes that. Before the floating branch below,
     because on the canvas a floating window is on the plane like every other
     one and its own rect is not what gets drawn. */
  if (canvasResizeBy(id, dx, dy, west, north)) return;

  /* A floating window resizes by simply becoming that much bigger — there are
     no siblings to take the space from. Clamped so a drag cannot shrink it to
     nothing and leave a window that can no longer be grabbed.

     A pull on the left or top edge moves the window as well as sizing it: the
     opposite edge is the one that has to stay put, and it only does if the
     corner behind the hand travels with the pointer. The clamp is applied to
     the size first and the move worked out from what the size actually became,
     so a drag that runs into the minimum stops rather than sliding the window
     across the desktop from an edge that can no longer move. */
  const floating = floatingOf(id);
  if (floating) {
    const view = views.get(id);
    const minWidth = parseInt(view?.el?.style?.minWidth, 10) || 80;
    const minHeight = parseInt(view?.el?.style?.minHeight, 10) || 60;
    const width = Math.max(minWidth, floating.width + (west ? -dx : dx));
    const height = Math.max(minHeight, floating.height + (north ? -dy : dy));
    if (west) floating.x += floating.width - width;
    if (north) floating.y += floating.height - height;
    floating.width = width;
    floating.height = height;
    gestureRelayout();
    return;
  }

  /* In the strip, horizontal means the column's own width — weights do
     nothing there, because columns are laid out at a fixed size rather than
     flexed. Vertical is still a share of the column, so it goes through the
     ordinary path. */
  if (layoutModeOf() === 'scrolling') {
    const workspace = workspaceOf(id);
    if (workspace === null) return;
    let changed = false;
    if (dx !== 0) changed = resizeColumn(workspace, id, dx, west) || changed;
    if (dy !== 0) changed = resizeAxis(id, 'vertical', dy, north) || changed;
    if (changed) gestureRelayout();
    return;
  }

  if (!findLeaf(id)) return;

  for (const [axis, delta, fromStart] of
    [['horizontal', dx, west], ['vertical', dy, north]]) {
    if (delta === 0) continue;
    resizeAxis(id, axis, delta, fromStart);
  }
  gestureRelayout();
}

function toggleLayout() {
  if (focusedId == null) return;
  const found = findLeaf(focusedId);
  if (!found) return;

  /* Leaving a tabbed or stacked container puts its windows back side by side,
     which is what sway's `layout toggle split` does from either of them. */
  if (found.parent.layout !== 'split') {
    found.parent.layout = 'split';
  } else {
    found.parent.dir =
      found.parent.dir === 'horizontal' ? 'vertical' : 'horizontal';
  }
  relayoutAll();
}

/* sway's `layout tabbed` and `layout stacking`. Applied to the container the
 * focused window is in; a window alone on a workspace has only the workspace
 * root, so tabbing it is a no-op until there is something to tab between. */
function setContainerLayout(layout) {
  if (focusedId == null) return;
  const found = findLeaf(focusedId);
  if (!found) return;

  /* Asking for the layout it already has turns it back into a split, so the
     same key toggles rather than sticking. */
  found.parent.layout = found.parent.layout === layout ? 'split' : layout;
  relayoutAll();
}

/* The area of the output a window may be drawn in, in page coordinates. Used
 * to work out how much of a window is actually on screen. */
