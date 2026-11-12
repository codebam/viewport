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
  if (ensureWorkspace(n) === null) return newSplit('horizontal');
  if (!workspaces.has(n)) workspaces.set(n, newSplit('horizontal'));
  return workspaces.get(n);
}

/* Whether a window opened from inside a tabbed or stacked container joins it
 * rather than splitting beside it. Hyprland's `group:auto_group`.
 *
 * `null` is "the config file said nothing", which keeps the behaviour this has
 * always had — join when the group's axis matches the pending split, wrap
 * otherwise. `true` always joins and `false` always splits beside, so setting
 * the key is what changes anything. See insertLeaf. */
let autoGroupEnabled = null;

/* Hyprland's other group options, with Hyprland's own defaults: dragging a
 * window onto a group joins it, dragging a group onto another merges the two,
 * and moving a window to a workspace leaves it there as its own window unless
 * the rule says otherwise. */
let dragIntoGroupEnabled = true;
let mergeGroupsOnDragEnabled = true;
let groupOnMoveToWorkspaceEnabled = false;

function applyGroup(group) {
  if (group === undefined || group === null) return;
  if (typeof group.auto_group === 'boolean') autoGroupEnabled = group.auto_group;
  if (typeof group.drag_into_group === 'boolean') {
    dragIntoGroupEnabled = group.drag_into_group;
  }
  if (typeof group.merge_groups_on_drag === 'boolean') {
    mergeGroupsOnDragEnabled = group.merge_groups_on_drag;
  }
  if (typeof group.group_on_movetoworkspace === 'boolean') {
    groupOnMoveToWorkspaceEnabled = group.group_on_movetoworkspace;
  }
}

/* The element each group was rendered as, by node. Rebuilt every layout pass:
 * the tree and its elements are both thrown away and made again, so a stale
 * entry would be a rectangle nobody is drawing. */
let groupEls = new Map();

function resetGroupElements() {
  groupEls = new Map();
}

/* Whether `node` is `target` or has it somewhere underneath. */
function containsNode(node, target) {
  if (node === target) return true;
  if (!node || node.type === 'leaf') return false;
  return node.children.some((child) => containsNode(child, target));
}

/* The workspace a node belongs to, or null when it is not in any of them. */
function nodeWorkspace(node) {
  for (const [n, root] of workspaces) {
    if (containsNode(root, node)) return n;
  }
  return null;
}

/* The group under a point on a workspace — the element the group was rendered
 * as, so the tab strip counts as well as the windows. `source` is the group
 * being dragged: it and anything that contains or is contained by it are not
 * candidates, and a locked group refuses like it does for everything else. */
function groupAtPoint(x, y, workspace, source = null) {
  let found = null;
  for (const [node, el] of groupEls) {
    if (node === source || node.locked) continue;
    if (source && (containsNode(node, source) || containsNode(source, node))) {
      continue;
    }
    if (workspace === null || nodeWorkspace(node) !== workspace) continue;
    const rect = el.getBoundingClientRect();
    if (x < rect.left || x >= rect.left + rect.width
        || y < rect.top || y >= rect.top + rect.height) continue;
    found = node;
  }
  return found;
}

/* Merge one group into another — Hyprland's `merge_groups_on_drag`.
 *
 * The source's windows are appended to the target's and the source container
 * goes away, which is what a group merge is: one group with more tabs, not a
 * group holding another group. A group nested in either direction is refused,
 * and so is a locked one. */
function mergeGroups(source, target) {
  if (!isGroup(source) || !isGroup(target) || source === target) return false;
  if (source.locked || target.locked) return false;
  if (containsNode(source, target) || containsNode(target, source)) return false;
  const workspace = nodeWorkspace(source);
  if (workspace === null || nodeWorkspace(target) !== workspace) return false;
  const root = workspaces.get(workspace);
  const parent = findParentOf(root, source);
  if (!parent) return false;

  target.children.push(...source.children);
  target.active = target.children.length - 1;
  parent.children = parent.children.filter((child) => child !== source);
  collapse(root, true, workspace);
  treeGeneration++;
  relayoutAll();
  return true;
}

/* Drag a group by its tab strip and drop it on another to merge — the strip is
 * the groupbar, and dragging it is how Hyprland moves a group. A click is not
 * a drag: the hand has to move further than this before anything is merged. */
const GROUP_DRAG_PX = 6;

function beginGroupBarDrag(event, node) {
  if (mergeGroupsOnDragEnabled !== true) return;
  const startX = Number(event?.clientX);
  const startY = Number(event?.clientY);
  if (!Number.isFinite(startX) || !Number.isFinite(startY)) return;

  const workspace = nodeWorkspace(node);
  let dragging = false;
  let candidate = null;
  const clearCandidate = () => {
    if (candidate) groupEls.get(candidate)?.classList.remove('merge-target');
    candidate = null;
  };
  const onMove = (move) => {
    const x = Number(move?.clientX);
    const y = Number(move?.clientY);
    if (!Number.isFinite(x) || !Number.isFinite(y)) return;
    if (!dragging) {
      if (Math.hypot(x - startX, y - startY) < GROUP_DRAG_PX) return;
      dragging = true;
      groupEls.get(node)?.classList.add('dragging-group');
    }
    const next = groupAtPoint(x, y, workspace, node);
    if (next === candidate) return;
    clearCandidate();
    candidate = next;
    if (candidate) groupEls.get(candidate)?.classList.add('merge-target');
  };
  const onUp = () => {
    /* Taken before the highlight is cleared: clearCandidate is what forgets
       which group was under the hand. */
    const target = candidate;
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
    groupEls.get(node)?.classList.remove('dragging-group');
    clearCandidate();
    if (dragging && target) mergeGroups(node, target);
  };
  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
}

/* A window drag ended. A floating window dropped on a group becomes one of its
 * tabs — Hyprland's `group:drag_into_group`, and what makes dragging a window
 * over a group do something other than leave it floating on top. */
function finishWindowDrag() {
  const id = draggedWindowId;
  draggedWindowId = null;
  if (id === null || dragIntoGroupEnabled !== true) return false;
  if (!isFloating(id)) return false;
  const view = views.get(id);
  const workspace = workspaceOf(id);
  if (!view || workspace === null) return false;

  const rect = view.el.getBoundingClientRect();
  const target = groupAtPoint(
    rect.left + rect.width / 2, rect.top + rect.height / 2, workspace);
  if (!target) return false;

  /* Out of the floating plane and into the group, shown as its newest tab. */
  view.floating = null;
  Object.assign(view.el.style, { left: '', top: '', width: '', height: '' });
  view.el.classList.remove('floating');
  target.children.push(newLeaf(id));
  target.active = target.children.length - 1;
  treeGeneration++;
  relayoutAll();
  return true;
}

/* A tabbed or stacked container. The one child shown is `active`. */
function isGroup(node) {
  return node != null && (node.layout === 'tabbed' || node.layout === 'stacked');
}

/* The leaves that are actually on screen in a tree: a group shows exactly one
 * of its children, so everything behind it is hidden. What a workspace has on
 * display is this, not every window in its tree. */
function visibleLeaves(node) {
  if (!node) return [];
  if (node.type === 'leaf') return [node];
  if (isGroup(node)) {
    const active = node.children[Math.min(
      Math.max(node.active ?? 0, 0), node.children.length - 1)];
    return active ? visibleLeaves(active) : [];
  }
  return node.children.flatMap(visibleLeaves);
}

function* walk(node, parent = null) {
  if (node.type === 'leaf') {
    yield [node, parent];
    return;
  }
  for (const child of node.children) yield* walk(child, node);
}

/* Leaf positions, keyed by `treeGeneration`. `findLeaf` walks every workspace
 * and every leaf, and `workspaceOf` is called once per window per geometry
 * pass — several times a frame while the layout is being measured, which makes
 * a full walk per call quadratic in the number of windows. Every structural
 * change bumps `treeGeneration`, so the index is rebuilt when it moves.
 *
 * The generation alone is not trusted: an entry is believed only while the
 * parent it names still holds the leaf under the root it names, so a change
 * that forgot to bump costs a walk rather than a wrong answer. That is the
 * same rule the compositor's own surface index uses, and for the same reason. */
let leafIndex = new Map();
let leafIndexGeneration = -1;

function rebuildLeafIndex() {
  leafIndex.clear();
  for (const [n, root] of workspaces) {
    for (const [leaf, parent] of walk(root)) {
      leafIndex.set(leaf.id, { leaf, parent, workspace: n, root });
    }
  }
  leafIndexGeneration = treeGeneration;
}

function findLeaf(id) {
  const cached = leafIndexGeneration === treeGeneration
    ? leafIndex.get(id) : null;
  if (cached && cached.leaf.id === id &&
      cached.parent.children.includes(cached.leaf) &&
      workspaces.get(cached.workspace) === cached.root) {
    return cached;
  }
  rebuildLeafIndex();
  return leafIndex.get(id) ?? null;
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

/* Forget a window's claim on fullscreen, whichever workspace recorded it.
 *
 * The map is keyed by workspace, but the two transitions that end a fullscreen
 * — a client un-fullscreening itself, and the window going away — are both
 * answering about a window, and either can arrive with the workspace no longer
 * resolvable for it. Clearing only under the workspace asked for then leaves
 * the entry behind, and an entry is not a memory: it hides the bar and keeps
 * notifications off the screen for the workspace that holds it, for the rest of
 * the session. The compositor plays a notification's sound on the bus thread
 * before the shell is asked to draw anything, so what that costs is a message
 * that is heard and never seen. */
function forgetFullscreen(id) {
  let forgotten = false;
  for (const [workspace, kept] of [...fullscreens]) {
    if (kept !== id) continue;
    fullscreens.delete(workspace);
    forgotten = true;
  }
  return forgotten;
}

function maximizedOn(workspace) {
  return workspace !== null && workspace !== undefined
    ? maximized.get(workspace) ?? null : null;
}

function isMaximized(id) {
  if (id == null) return false;
  return maximizedOn(workspaceOf(id)) === id;
}

function isMinimized(id) {
  return id != null && views.get(id)?.minimized === true;
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
    .filter((id) => views.has(id) && !views.get(id)?.special);
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
 * accumulate meaningless nesting as windows come and go.
 *
 * `workspace` is the workspace `node` is the root of, needed only for the
 * root guard below: `layoutModeOf()` with no argument answers for the
 * *rendering* workspace, and a window closing on a background workspace was
 * therefore collapsed under the foreground's layout mode. */
function collapse(node, isRoot = false, workspace = null) {
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
      !isGroup(node.children[0]) &&
      !(isRoot && layoutModeOf(workspace ?? undefined) === 'scrolling')) {
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
  collapse(workspaces.get(found.workspace), true, found.workspace);
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
  if (layoutModeOf() === 'scrolling') {
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

  /* A tabbed or stacked parent is a group. `auto_group` decides whether the
     new window becomes another tab or splits beside the group, and a locked
     group refuses either way — Hyprland's `deny_from_group`, said once for
     the whole container by `group.lock`. Unset leaves the historical rule
     below to answer, which is what keeps a config that never mentions groups
     behaving exactly as it did. */
  if (isGroup(parent) && (parent.locked || autoGroupEnabled !== null)) {
    if (!parent.locked && autoGroupEnabled === true) {
      parent.children.splice(index + 1, 0, leaf);
      return;
    }
    const grandparent = findParentOf(root, parent);
    const wrapper = newSplit(pendingSplit);
    wrapper.children = [parent, leaf];
    if (grandparent) {
      grandparent.children[grandparent.children.indexOf(parent)] = wrapper;
    } else {
      /* The group is the workspace root; the wrapper becomes the root and
         the group its first child. */
      workspaces.set(workspace, wrapper);
    }
    return;
  }

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
  dissolveSwallow(id);
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
      collapse(root, true, workspace);
      return true;
    }
  }

  const grandparent = findParentOf(root, parent);
  if (grandparent) {
    const parentIndex = grandparent.children.indexOf(parent);
    parent.children.splice(index, 1);
    grandparent.children.splice(parentIndex + (forward ? 1 : 0), 0, leaf);
    collapse(root, true, workspace);
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
      collapse(root, true, ws);
      relayoutAll();
      return true;
    }
  }

  const grandparent = findParentOf(root, parent);
  if (grandparent) {
    const parentIndex = grandparent.children.indexOf(parent);
    parent.children.splice(index, 1);
    grandparent.children.splice(parentIndex + (forward ? 1 : 0), 0, container);
    collapse(root, true, ws);
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
  const count = [...walk(node)].filter(([leaf]) => views.has(leaf.id)).length;
  return `${count} window${count === 1 ? '' : 's'}`;
}

/* Does this subtree contain the focused window? Used to mark the active tab,
 * and to keep the selected tab in step with focus. */
function containsFocus(node) {
  if (focusedId == null) return false;
  if (node.type === 'leaf') return node.id === focusedId;
  return [...walk(node)].some(([leaf]) => leaf.id === focusedId);
}

function hasLayoutView(node) {
  if (node.type === 'leaf') return views.has(node.id) && !isMinimized(node.id);
  return [...walk(node)].some(([leaf]) => views.has(leaf.id) && !isMinimized(leaf.id));
}

/* Tabbed and stacked containers: one child visible, the rest reachable through
 * a strip of titles. The only difference between them is which way the strip
 * runs — tabs across the top, stacked titles one per row — which is exactly how
 * sway distinguishes them. */
function renderTabbed(node) {
  const el = document.createElement('div');
  el.className = `split ${node.layout}`;
  el.style.flexGrow = String(node.weight ?? 1);

  const children = node.children.filter(hasLayoutView);
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
  /* Dragging the strip drags the group: dropping it on another group merges
     the two. A click still selects a tab — see the threshold in
     beginGroupBarDrag. */
  tabs.addEventListener('mousedown', (event) => beginGroupBarDrag(event, node));

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
  groupEls.set(node, el);

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
    if (!view || isMinimized(node.id)) return null;
    renderedIds.add(node.id);
    const pseudo = view.pseudotile;
    const active = pseudo && !isFullscreen(node.id)
      && (layoutModeOf() === 'tiling' || layoutModeOf() === 'scrolling');
    view.el.classList.toggle('pseudotiled', Boolean(active));
    if (!active) {
      view.el.style.flexGrow = String(node.weight ?? 1);
      Object.assign(view.el.style, {
        width: '', height: '',
        /* A rule's `maxsize` is the window's ceiling wherever the layout puts
           it; empty is the layout's own. `minsize` likewise. */
        maxWidth: view.maxWidth > 0 ? `${view.maxWidth}px` : '',
        maxHeight: view.maxHeight > 0 ? `${view.maxHeight}px` : '',
        minWidth: view.minWidth > 0 ? `${view.minWidth}px` : '',
        minHeight: view.minHeight > 0 ? `${view.minHeight}px` : '',
      });
      return view.el;
    }

    const slot = document.createElement('div');
    slot.className = 'pseudotile-slot';
    slot.style.flexGrow = String(node.weight ?? 1);
    Object.assign(view.el.style, {
      flexGrow: '0',
      width: `${Math.max(pseudo.width, view.minWidth || 1)}px`,
      height: `${Math.max(pseudo.height, view.minHeight || 1)}px`,
      maxWidth: '100%', maxHeight: '100%', minWidth: '0', minHeight: '0',
    });
    slot.append(view.el);
    return slot;
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
