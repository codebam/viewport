/* SPDX-License-Identifier: MIT
 *
 * The tiling tree, and rendering it to nested flexboxes.
 *
 * Splitting, moving and collapsing restructure the tree; nothing here computes a
 * rectangle. The browser does that, and geometry.js measures the result.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */
/* ------------------------------------------------------------------------
 * Tiling tree
 *
 * A node is either { type:'split', dir, children:[] } or { type:'leaf', id }.
 * The tree defines structure only; flexbox turns it into pixels.
 * --------------------------------------------------------------------- */

/* `weight` is the child's share of its parent's axis, rendered as flex-grow.
 * Equal weights give equal sizes; resizing only ever changes these numbers and
 * lets the browser recompute the pixels. */
/* `layout` is how the children are presented: 'split' lays them out side by
 * side, 'tabbed' and 'stacked' show one at a time behind a strip of titles.
 * `active` is which child that is. */
function newSplit(dir) {
  return { type: 'split', dir, children: [], weight: 1, layout: 'split', active: 0 };
}

function newLeaf(id) {
  return { type: 'leaf', id, weight: 1 };
}

function workspaceRoot(n) {
  if (!workspaces.has(n)) workspaces.set(n, newSplit('horizontal'));
  return workspaces.get(n);
}

function* walk(node, parent = null) {
  if (node.type === 'leaf') {
    yield [node, parent];
    return;
  }
  for (const child of node.children) yield* walk(child, node);
}

function findLeaf(id) {
  for (const [n, root] of workspaces) {
    for (const [leaf, parent] of walk(root)) {
      if (leaf.id === id) return { leaf, parent, workspace: n };
    }
  }
  return null;
}

function leavesOf(n) {
  const root = workspaces.get(n);
  return root ? [...walk(root)].map(([leaf]) => leaf) : [];
}

/* The fullscreen window on a workspace, if any. */
function fullscreenOn(workspace) {
  return workspace !== null && workspace !== undefined
    ? fullscreens.get(workspace) ?? null : null;
}

/* Whether this window is the fullscreen one on its own workspace. */
function isFullscreen(id) {
  if (id == null) return false;
  return fullscreenOn(workspaceOf(id)) === id;
}

/* Which workspace a window is on, tiled or floating. */
function workspaceOf(id) {
  const floating = floatingOf(id);
  if (floating) return floating.workspace;
  return findLeaf(id)?.workspace ?? null;
}

/* Every window on a workspace, in stacking order: tiled first, floating over
 * the top. Used wherever "what is on this workspace" is the question rather
 * than "where does it sit in the tree". */
function idsOf(n) {
  const ids = leavesOf(n).map((leaf) => leaf.id)
    .filter((id) => !views.get(id)?.special);
  for (const [id, floating] of floatingEntries()) {
    if (floating.workspace === n) ids.push(id);
  }
  return ids;
}

function findParentOf(node, target) {
  if (node.type === 'leaf') return null;
  if (node.children.includes(target)) return node;
  for (const child of node.children) {
    const found = findParentOf(child, target);
    if (found) return found;
  }
  return null;
}

/* Drop empty splits and inline single-child ones, so the tree does not
 * accumulate meaningless nesting as windows come and go. */
function collapse(node, isRoot = false) {
  if (node.type === 'leaf') return;

  /* Explicit arrow: forEach would otherwise pass the index as `isRoot` and
     every child after the first would be treated as a root. */
  node.children.forEach((child) => collapse(child));
  node.children = node.children.filter(
    (c) => c.type === 'leaf' || c.children.length > 0);

  /* Inlining a lone child into the workspace root would destroy the strip: the
     root's children *are* the columns, so a single column holding three windows
     would be flattened into three columns. */
  if (node.children.length === 1 && node.children[0].type === 'split' &&
      !(isRoot && layoutMode === 'scrolling')) {
    const only = node.children[0];
    node.dir = only.dir;
    node.children = only.children;
  }

  /* A single remaining child inherits whatever fraction it was resized to.
   * That is meaningless once it has no sibling to share with, and leaves a
   * window looking wrong after its neighbour closes mid-resize. */
  if (node.children.length === 1) {
    node.children[0].weight = 1;
  }
}

function removeLeaf(id) {
  const found = findLeaf(id);
  if (!found) return;
  found.parent.children =
    found.parent.children.filter((c) => c.id !== id);
  collapse(workspaces.get(found.workspace), true);
}

/* Insert next to the focused window, splitting in the pending direction —
 * i3's behaviour, and why two terminals side by side then Mod4+v puts the
 * third underneath the second rather than beside it. */
function insertLeaf(workspace, id) {
  const root = workspaceRoot(workspace);
  const leaf = newLeaf(id);

  /* In the scrolling layout a new window is a new column, placed just right of
     the one in focus. Nothing already open changes size — that is the whole
     point of the model — so the strip simply gets longer. */
  if (layoutMode === 'scrolling') {
    root.dir = 'horizontal';
    leaf.width = COLUMN_WIDTHS[1];
    const index = root.children.findIndex(containsFocus);
    root.children.splice(index < 0 ? root.children.length : index + 1, 0, leaf);
    return;
  }

  const anchor = focusedId != null ? findLeaf(focusedId) : null;
  if (!anchor || anchor.workspace !== workspace) {
    if (root.children.length === 0) root.dir = pendingSplit;
    root.children.push(leaf);
    return;
  }

  const { parent } = anchor;
  const index = parent.children.findIndex((c) => c.id === focusedId);

  if (parent.dir === pendingSplit || parent.children.length === 1) {
    parent.dir = pendingSplit;
    parent.children.splice(index + 1, 0, leaf);
    return;
  }

  /* Different axis: wrap the focused window in a new split. */
  const wrapper = newSplit(pendingSplit);
  wrapper.children = [parent.children[index], leaf];
  parent.children[index] = wrapper;
}

/* Move within the tree. Along the parent's axis it swaps with its neighbour;
 * across it, the window is promoted into the grandparent — which is what lets
 * repeated moves walk out of a nested split instead of getting stuck. */
function moveLeaf(id, direction) {
  const found = findLeaf(id);
  if (!found) return false;

  const { parent, workspace, leaf } = found;
  const axis = (direction === 'left' || direction === 'right')
    ? 'horizontal' : 'vertical';
  const forward = direction === 'right' || direction === 'down';
  const index = parent.children.findIndex((c) => c.id === id);
  const root = workspaces.get(workspace);

  if (parent.dir === axis) {
    const target = index + (forward ? 1 : -1);
    if (target >= 0 && target < parent.children.length) {
      const sibling = parent.children[target];

      if (sibling.type === 'split') {
        /* Moving into a split means entering it, not stepping over it. */
        parent.children.splice(index, 1);
        sibling.children.splice(forward ? 0 : sibling.children.length, 0, leaf);
      } else {
        [parent.children[index], parent.children[target]] =
          [parent.children[target], parent.children[index]];
      }
      collapse(root, true);
      return true;
    }
  }

  const grandparent = findParentOf(root, parent);
  if (grandparent) {
    const parentIndex = grandparent.children.indexOf(parent);
    parent.children.splice(index, 1);
    grandparent.children.splice(parentIndex + (forward ? 1 : 0), 0, leaf);
    collapse(root, true);
    return true;
  }

  /* Nothing left to move past inside this workspace. Report failure so the
   * caller can carry the window to the next monitor, which is what sway does
   * at the edge. */
  return false;
}

function moveContainer(container, direction) {
  if (!container) return false;
  const ws = activeWorkspace();
  const root = workspaces.get(ws);
  if (!root || container === root) return false;

  const parent = findParentOf(root, container);
  if (!parent) return false;

  const axis = (direction === 'left' || direction === 'right') ? 'horizontal' : 'vertical';
  const forward = (direction === 'right' || direction === 'down');
  const index = parent.children.indexOf(container);

  if (parent.dir === axis) {
    const target = index + (forward ? 1 : -1);
    if (target >= 0 && target < parent.children.length) {
      [parent.children[index], parent.children[target]] =
        [parent.children[target], parent.children[index]];
      collapse(root, true);
      relayoutAll();
      return true;
    }
  }

  const grandparent = findParentOf(root, parent);
  if (grandparent) {
    const parentIndex = grandparent.children.indexOf(parent);
    parent.children.splice(index, 1);
    grandparent.children.splice(parentIndex + (forward ? 1 : 0), 0, container);
    collapse(root, true);
    relayoutAll();
    return true;
  }

  return false;
}

/* ------------------------------------------------------------------------
 * Rendering
 * --------------------------------------------------------------------- */

/* Windows the current render actually put on screen. A window in a tabbed
 * container that is not the selected tab is in the DOM but displays nothing, so
 * it must be reported to the compositor as invisible — otherwise its surface
 * keeps being painted into a hole that is no longer there. */
let renderedIds = new Set();

/* Title for a tab. A container gets a count instead: there is no single title
 * for four windows, and "4 windows" is what sway shows too. */
function tabLabel(node) {
  if (node.type === 'leaf') {
    const view = views.get(node.id);
    return view ? (view.title || view.app_id || `view ${node.id}`) : '';
  }
  const count = [...walk(node)].length;
  return `${count} window${count === 1 ? '' : 's'}`;
}

/* Does this subtree contain the focused window? Used to mark the active tab,
 * and to keep the selected tab in step with focus. */
function containsFocus(node) {
  if (focusedId == null) return false;
  if (node.type === 'leaf') return node.id === focusedId;
  return [...walk(node)].some(([leaf]) => leaf.id === focusedId);
}

/* Tabbed and stacked containers: one child visible, the rest reachable through
 * a strip of titles. The only difference between them is which way the strip
 * runs — tabs across the top, stacked titles one per row — which is exactly how
 * sway distinguishes them. */
function renderTabbed(node) {
  const el = document.createElement('div');
  el.className = `split ${node.layout}`;
  el.style.flexGrow = String(node.weight ?? 1);

  const children = node.children.filter(
    (child) => child.type === 'split' || views.has(child.id));
  if (children.length === 0) return null;

  /* Focus wins over the stored selection: focusing a window inside a collapsed
     tab has to bring that tab forward, or the focused window stays hidden. A
     fullscreen window wins outright — it covers the output, so it has to be in
     the DOM whatever the tab strip says. */
  let active = children.findIndex((child) => child.type === 'leaf'
    ? isFullscreen(child.id)
    : [...walk(child)].some(([leaf]) => isFullscreen(leaf.id)));
  if (active < 0) active = children.findIndex(containsFocus);
  if (active < 0) active = Math.min(node.active ?? 0, children.length - 1);
  node.active = active;

  const tabs = document.createElement('div');
  tabs.className = 'tabs';

  children.forEach((child, i) => {
    const tab = document.createElement('button');
    tab.className = 'tab' + (i === active ? ' active' : '');
    tab.textContent = tabLabel(child);
    tab.addEventListener('mousedown', () => {
      node.active = i;
      /* Clicking a tab focuses what is in it, the way clicking a window does. */
      const first = child.type === 'leaf'
        ? child.id : [...walk(child)][0]?.[0]?.id;
      if (first != null) send({ type: 'view.focus', id: first });
      relayoutAll();
    });
    tabs.append(tab);
  });

  el.append(tabs);

  const body = document.createElement('div');
  body.className = 'tab-body';
  const childEl = renderTree(children[active]);
  if (childEl) body.append(childEl);
  el.append(body);

  return el;
}

function renderTree(node) {
  if (node.type === 'leaf') {
    const view = views.get(node.id);
    if (!view) return null;
    view.el.style.flexGrow = String(node.weight ?? 1);
    renderedIds.add(node.id);
    return view.el;
  }

  if (node.layout === 'tabbed' || node.layout === 'stacked') {
    return renderTabbed(node);
  }

  const el = document.createElement('div');
  el.className = `split ${node.dir}`;
  el.style.flexGrow = String(node.weight ?? 1);

  const rendered = [];
  for (const child of node.children) {
    const childEl = renderTree(child);
    if (childEl) rendered.push([child, childEl]);
  }

  rendered.forEach(([child, childEl], i) => {
    if (i > 0) {
      /* A real element in the gap, so the edge between two windows can be
       * dragged. The gap is shell-drawn pixels, which is why edge resizing
       * needs no compositor support at all.
       *
       * The two nodes it resizes, not where they sit in `node.children`: a
       * sibling that rendered nothing — an unclaimed slot after a restart, an
       * empty split — is in the tree and not in this list, and an index taken
       * from here would name the wrong pair on the other side of it. */
      const divider = document.createElement('div');
      divider.className = 'divider';
      divider.addEventListener('mousedown', (event) =>
        beginDividerDrag(event, node, rendered[i - 1][0], child));
      el.append(divider);
    }
    el.append(childEl);
  });

  return rendered.length > 0 ? el : null;
}
