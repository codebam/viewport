/* SPDX-License-Identifier: MIT
 *
 * Dynamic tiling arrangements: master-stack, spiral and bsp.
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

const TILING_MODES = ['manual', 'master-stack', 'spiral', 'bsp'];

/* workspace -> the window set the current shape was built for, so a relayout
 * that changed nothing does not throw away the weights. */
const arrangedFor = new Map();

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

/* The proportions of the output a workspace is showing, for bsp's first cut.
 *
 * Falls back to a landscape guess when the workspace is not on screen: an
 * arrangement still has to be built for it, and every desktop monitor this
 * runs on is wider than it is tall. */
function workspaceAspect(workspace) {
  for (const output of outputs.values()) {
    if (output.workspace === workspace && output.width > 0 && output.height > 0) {
      return [output.width, output.height];
    }
  }
  return [16, 9];
}

/* Rebuild one workspace's shape, if the mode asks for one and the windows have
 * changed since it was last built. */
function arrangeWorkspace(workspace) {
  if (layoutMode !== 'tiling' || tilingMode === 'manual') {
    arrangedFor.delete(workspace);
    return;
  }

  const root = workspaces.get(workspace);
  if (!root) return;

  const ids = dynamicOrder(root);
  /* Fullscreen takes the workspace on its own, and rearranging underneath it
     would be work nobody can see. */
  const signature = ids.join(',');
  if (arrangedFor.get(workspace) === signature) return;
  arrangedFor.set(workspace, signature);

  if (tilingMode === 'master-stack') {
    root.children = masterStack(ids, root);
  } else {
    const [w, h] = workspaceAspect(workspace);
    const pick = tilingMode === 'spiral' ? spiralPick : bspPick;
    /* The root is one split already, so the first cut is its direction and the
       nest continues underneath it. */
    root.dir = pick(w, h, ids.length);
    const [half, rest] = root.dir === 'horizontal' ? [w / 2, h] : [w, h / 2];
    root.children = ids.length === 0 ? []
      : ids.length === 1 ? [newLeaf(ids[0])]
        : [newLeaf(ids[0]), nest(ids.slice(1), pick, half, rest)];
  }
  root.layout = 'split';
  treeGeneration++;
}

/* Every workspace, on the way into a relayout. */
function arrangeAll() {
  if (layoutMode !== 'tiling' || tilingMode === 'manual') {
    if (arrangedFor.size > 0) arrangedFor.clear();
    return;
  }
  for (const workspace of workspaces.keys()) arrangeWorkspace(workspace);
}

/* Throw the arrangements away so the next relayout rebuilds them: the mode
 * changed, so the shape that matched the old one means nothing. */
function resetArrangements() {
  arrangedFor.clear();
}
