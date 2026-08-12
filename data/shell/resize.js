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
    if (shiftWeightBetween(before, after, delta / extent)) relayoutAll();
  };

  const onUp = () => {
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
  const strip = event.currentTarget.parentElement;
  strip?.classList.add('dragging');

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
    relayoutAll();
  };

  const onUp = () => {
    strip?.classList.remove('dragging');
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
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

/* Mod4 + right drag, forwarded by the compositor as a pixel delta. */
/* Shift one axis of a tiled window's share of its container. */
function resizeAxis(id, axis, delta) {
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
  const usePrevious = index === parent.children.length - 1;
  return shiftWeight(parent, usePrevious ? index - 1 : index,
    (usePrevious ? -1 : 1) * (delta / extent));
}

/* Widen or narrow the column a window is in, as a fraction of the output.
 *
 * Columns do not share space: widening one does not take anything from its
 * neighbours, it makes the strip longer and shifts everything after it along.
 * That is the model — a column keeps the width it was given no matter what
 * happens elsewhere — so there is nothing here that resizes an adjacent
 * window, unlike a tiling split. */
function resizeColumn(workspace, id, dx) {
  const root = workspaceRoot(workspace);
  const column = root.children[columnIndexOf(workspace, id)];
  if (!column) return false;

  const area = windowsAreaOf(workspace);
  const extent = area ? area.right - area.left : 0;
  if (extent <= 0) return false;

  const next = (column.width ?? COLUMN_WIDTHS[1]) + dx / extent;
  column.width = Math.max(0.1, Math.min(next, 1));
  return true;
}

function resizeByDelta(id, dx, dy) {
  /* On the canvas a window's place is its size, and nothing shares space with
     it, so the drag simply changes that. Before the floating branch below,
     because on the canvas a floating window is on the plane like every other
     one and its own rect is not what gets drawn. */
  if (canvasResizeBy(id, dx, dy)) return;

  /* A floating window resizes by simply becoming that much bigger — there are
     no siblings to take the space from. Clamped so a drag cannot shrink it to
     nothing and leave a window that can no longer be grabbed. */
  const floating = floatingOf(id);
  if (floating) {
    const view = views.get(id);
    const minWidth = parseInt(view?.el?.style?.minWidth, 10) || 80;
    const minHeight = parseInt(view?.el?.style?.minHeight, 10) || 60;
    floating.width = Math.max(minWidth, floating.width + dx);
    floating.height = Math.max(minHeight, floating.height + dy);
    relayoutAll();
    return;
  }

  /* In the strip, horizontal means the column's own width — weights do
     nothing there, because columns are laid out at a fixed size rather than
     flexed. Vertical is still a share of the column, so it goes through the
     ordinary path. */
  if (layoutMode === 'scrolling') {
    const workspace = workspaceOf(id);
    if (workspace === null) return;
    let changed = false;
    if (dx !== 0) changed = resizeColumn(workspace, id, dx) || changed;
    if (dy !== 0) changed = resizeAxis(id, 'vertical', dy) || changed;
    if (changed) relayoutAll();
    return;
  }

  if (!findLeaf(id)) return;

  for (const [axis, delta] of [['horizontal', dx], ['vertical', dy]]) {
    if (delta === 0) continue;
    resizeAxis(id, axis, delta);
  }
  relayoutAll();
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
