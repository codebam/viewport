/* SPDX-License-Identifier: MIT
 *
 * Reference shell: tiling, workspaces, and a status bar.
 *
 *   we receive   view.added / view.props / view.removed / view.focused
 *                output.layout / status.update / shell.command / error
 *   we send      view.layout / view.visible / view.focus / view.close
 *                view.query / output.query / shell.focus
 *
 * Three things are worth understanding before changing anything.
 *
 * Layout is CSS, not arithmetic. The tiling tree renders to nested flexboxes
 * and the browser computes every rectangle. Splitting, moving and fullscreen
 * only restructure the tree; no code here calculates a window position.
 *
 * Geometry is measured, never assumed. A hole's screen rect changes for
 * reasons no message announces — transitions, font loading, a reflow three
 * ancestors up — so a ResizeObserver watches each hole and reports what it
 * actually measures.
 *
 * Multi-monitor. The WebKit view is one canvas spanning the whole output
 * layout; two 2560x1440 screens are a single 5120x1440 page. Each output gets
 * an absolutely-positioned desktop and everything happens inside it.
 */

const bridge = window.webkit?.messageHandlers?.viewport;

function send(message) {
  if (!bridge) {
    console.warn('not running under viewport:', message);
    return;
  }
  bridge.postMessage(JSON.stringify(message));
}

/* ------------------------------------------------------------------------
 * Configuration
 * --------------------------------------------------------------------- */

const WORKSPACES = 9;

/* ------------------------------------------------------------------------
 * Session
 *
 * Restarting the compositor kills every client with it, so nothing here
 * preserves processes — it preserves *places*. The tree is written down with
 * each window replaced by the application that was in it, and as those
 * applications come back they are put into the slot they left rather than
 * appended wherever there is room.
 *
 * A slot is an ordinary leaf whose id is negative: no real view ever has one,
 * so everything that walks the tree skips it (views.get returns undefined and
 * the renderers already drop leaves with no view). That means the structure,
 * the column widths and the weights all survive without a parallel
 * representation to keep in step.
 * --------------------------------------------------------------------- */

/* How long an unclaimed slot is held before the layout gives up on it. Long
 * enough for a slow application — a browser restoring its own session — and
 * short enough that a workspace is not permanently shaped around something
 * that is never coming back. */
const SLOT_TIMEOUT_MS = 45_000;

let nextSlotId = -1;
let slotsPending = 0;
/* Places kept for floating windows. Not slots in the tree — a floating window
 * has no position in it — so they are held separately and matched the same
 * way, by application. */
let floatSlots = [];

/* Which workspace a monitor starts on, by output name. Anything unlisted gets
 * the lowest workspace not already on screen. */
const OUTPUT_WORKSPACE = {
  'DP-1': 1,  // left
  'DP-3': 9,  // right
};

/* ------------------------------------------------------------------------
 * State
 * --------------------------------------------------------------------- */

const outputs = new Map(); // name -> desktop elements + workspace + barHidden
const views = new Map();   // id -> { el, viewport, title, app_id, box }
const workspaces = new Map(); // number -> tiling tree root

/* Floating windows sit outside the tiling tree entirely: they keep their own
 * position and size and overlap whatever is tiled underneath. Dialogs land here
 * automatically because tiling them squeezes the window they belong to, and the
 * dialog itself usually cannot be resized to fill the slot it was given. */
const floats = new Map(); // id -> { workspace, x, y, width, height }

let focusedId = null;
let activeOutput = null;
/* Direction the next new window splits in, like sway's splith/splitv. */
let pendingSplit = 'horizontal';
/* Fullscreen is per workspace, not per session: a workspace lives on one
 * monitor at a time, so two monitors can each have something fullscreen and
 * neither cancels the other. A single global here meant fullscreening on the
 * second monitor silently un-fullscreened the first. */
const fullscreens = new Map(); // workspace -> view id
let lastStatus = {};
let currentMode = 'default';
/* 'tiling' (i3-style splits) or 'scrolling' (niri's strip of columns). Set by
 * the compositor from the config file; the shell implements both. */
let layoutMode = 'tiling';
/* Rules from the config file, applied to a window when it opens. Matched on
 * app_id, or on title where an application gives everything the same app_id. */
let windowRules = [];
/* Notifications on screen, newest last. The compositor owns the D-Bus side and
 * hands them here; what they look like and how long they stay is the shell's. */
const notifications = new Map(); // id -> { el, timer }
/* Horizontal scroll offset per workspace, in pixels, for the scrolling layout.
 * Only ever adjusted to bring the focused column into view. */
const scrollOffsets = new Map();
/* While a three-finger swipe is in progress the strip follows the fingers
 * rather than the focused column, and the transition is off so it tracks them
 * exactly. Cleared on settle, when focus is moved to whichever column the
 * gesture landed on and the ordinary follow logic takes over again. */
let gestureWorkspace = null;
/* The overview: every workspace at once, scaled down. Windows are drawn shrunk
 * by the compositor rather than resized, so no client is asked to relayout
 * itself into a thumbnail — which many would refuse anyway, having a minimum
 * size larger than one. */
let overviewActive = false;
/* Scale currently applied to each window, so reportGeometry can undo it: the
 * compositor wants the window's real size plus a factor, not the size it
 * appears at. */
const overviewScales = new Map(); // view id -> scale
/* The thumbnail each window is drawn in, so its clip can be taken from that
 * rather than from the whole output. */
const overviewCells = new Map(); // view id -> thumbnail element
/* Thumbnails by workspace, so a drag can find what it was released over. */
const overviewThumbs = new Map(); // workspace -> thumbnail element

const outputsEl = document.getElementById('outputs');
const notificationsEl = document.getElementById('notifications');
const desktopTemplate = document.getElementById('desktop-template');
const windowTemplate = document.getElementById('window-template');

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
  const floating = floats.get(id);
  if (floating) return floating.workspace;
  return findLeaf(id)?.workspace ?? null;
}

/* Every window on a workspace, in stacking order: tiled first, floating over
 * the top. Used wherever "what is on this workspace" is the question rather
 * than "where does it sit in the tree". */
function idsOf(n) {
  const ids = leavesOf(n).map((leaf) => leaf.id);
  for (const [id, floating] of floats) {
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
       * needs no compositor support at all. */
      const divider = document.createElement('div');
      divider.className = 'divider';
      divider.addEventListener('mousedown', (event) =>
        beginDividerDrag(event, node, i - 1));
      el.append(divider);
    }
    el.append(childEl);
  });

  return rendered.length > 0 ? el : null;
}

/* ------------------------------------------------------------------------
 * Scrolling layout (niri)
 *
 * A workspace is an endless horizontal strip of columns. Each column holds one
 * or more windows stacked vertically, and columns keep the width they were
 * given: opening a window never reflows the ones already there, it just makes
 * the strip longer. The view scrolls to keep the focused column on screen.
 *
 * The tree still holds the structure — the root's children are the columns —
 * so everything that walks or edits it keeps working. What differs is the
 * rendering: fixed widths and a scroll offset instead of flex-grow.
 * --------------------------------------------------------------------- */

/* Column widths as a fraction of the output, cycled by layout.column.width.
 * Same set as niri's default preset list. */
const COLUMN_WIDTHS = [1 / 3, 1 / 2, 2 / 3, 1];

/* Width of the divider between columns, which is --gap. Read from the
 * stylesheet so the two cannot drift apart, with a fallback for the case where
 * computed styles are unavailable (the test harness has no layout engine). */
function gapPx() {
  const raw = typeof getComputedStyle === 'function'
    ? getComputedStyle(document.documentElement).getPropertyValue('--gap')
    : '';
  const value = parseInt(raw, 10);
  return Number.isFinite(value) ? value : 8;
}
const COLUMN_HEIGHTS = [1 / 3, 1 / 2, 2 / 3, 1];

/* `area` may be supplied by the caller. The overview renders a strip into a
 * container that is not in the document yet, and an unattached element measures
 * zero — which collapsed every column to nothing and left the windows sized by
 * their own content instead of by the layout. */
function renderStrip(root, output, area = null) {
  const strip = document.createElement('div');
  strip.className = 'strip';

  if (area === null) {
    /* The measured rect is the border box, and `.windows` is padded by a gap
       on every side. Sizing columns against it makes a full-width column two
       gaps wider than the space it can actually occupy — it overflowed the
       right edge and never sat centred. The strip lives inside the padding, so
       that is what it has to be measured against. */
    const rect = output.windowsEl.getBoundingClientRect();
    const pad = gapPx();
    area = { width: rect.width - pad * 2, height: rect.height - pad * 2 };
  }
  const columns = root.children.filter(
    (child) => child.type === 'split' || views.has(child.id));

  let offset = 0;
  let focusedStart = null;
  let focusedWidth = 0;

  /* Dividers come out of the columns' own share rather than being added on
     top. Otherwise two half-width columns plus the divider between them are
     wider than the screen, and focusing the second one has to scroll by a gap
     — the windows visibly shift when switching between two halves that ought
     to sit still. Taking (N-1) gaps and spreading them across N columns makes
     fractions that sum to 1 fill the width exactly. */
  const share = columns.length > 1
    ? gapPx() * (columns.length - 1) / columns.length : 0;

  columns.forEach((column, i) => {
    const width = Math.max(1,
      Math.round(area.width * (column.width ?? 1 / 2) - share));

    if (i > 0) {
      /* Grabbable edge, same as between tiled windows. It drags the column to
         its left; the gap it sits in is shell-drawn, so no compositor support
         is needed. */
      const divider = document.createElement('div');
      divider.className = 'divider';
      divider.addEventListener('mousedown', (event) =>
        beginColumnDrag(event, output.workspace, columns[i - 1]));
      strip.append(divider);
      /* Counted, or the scroll offset drifts by one gap per column and the
         focused column stops lining up with the edge of the screen. */
      offset += gapPx();
    }

    const el = document.createElement('div');
    el.className = 'column';
    el.style.width = `${width}px`;

    /* A column is itself a vertical stack, so the existing renderer handles
       what is inside it. */
    const inner = renderTree(column);
    if (inner) el.append(inner);
    strip.append(el);

    if (containsFocus(column)) {
      focusedStart = offset;
      focusedWidth = width;
    }
    offset += width;
  });

  /* Scroll the least amount that brings the focused column fully into view. A
     column wider than the screen is aligned to the left edge instead, since it
     cannot be fully shown either way. Suppressed mid-gesture: the fingers are
     driving, and snapping back to the focused column would fight them. */
  const dragging = gestureWorkspace === output.workspace;
  const workspace = output.workspace;
  const lastScroll = scrollOffsets.get(workspace) ?? 0;
  let scroll = lastScroll;
  if (focusedStart !== null && !dragging) {
    if (focusedWidth >= area.width || focusedStart < scroll) {
      scroll = focusedStart;
    } else if (focusedStart + focusedWidth > scroll + area.width) {
      scroll = focusedStart + focusedWidth - area.width;
    }
  }
  /* Never scroll past either end of the strip. */
  scroll = Math.max(0, Math.min(scroll, Math.max(0, offset - area.width)));
  scrollOffsets.set(workspace, scroll);

  /* The strip element is new every render, so it would simply appear already
     scrolled. Start it where the last one ended and move it in the same frame
     the windows are flipped, so the two animations run together. */
  const previous = scrollOffsets.has(workspace) ? lastScroll : scroll;
  strip.style.transform = `translateX(${-(dragging ? scroll : previous)}px)`;
  strip.dataset.scroll = String(scroll);
  if (dragging) strip.classList.add('dragging');

  return columns.length > 0 ? strip : null;
}

/* Reshape existing workspaces when the layout model changes at runtime.
 *
 * The two models share the tree but read it differently: the strip needs the
 * root to be horizontal with one child per column, and each column to carry a
 * width. Coming back the other way, those widths are meaningless and the tabbed
 * containers a strip never has are left alone. Without this a switch mid-session
 * renders a tree the new model cannot make sense of. */
function normaliseForLayout() {
  for (const root of workspaces.values()) {
    if (layoutMode === 'scrolling') {
      root.dir = 'horizontal';
      root.layout = 'split';
      for (const column of root.children) {
        if (column.width === undefined) column.width = COLUMN_WIDTHS[1];
        if (column.type === 'split') column.dir = 'vertical';
      }
    } else {
      for (const column of root.children) delete column.width;
    }
  }
  scrollOffsets.clear();
}

/* Every workspace at once, laid out as a grid of miniature desktops.
 *
 * Each thumbnail renders the workspace's real tree — the same renderTree and
 * renderStrip used for the live layout — inside a container sized to the output
 * and then scaled down. That is what makes the miniatures accurate rather than
 * an approximation of the layout: they *are* the layout, drawn smaller.
 *
 * The windows inside are real surfaces. The compositor shrinks them for us,
 * told through the scale on view.layout, so nothing is asked to resize. */
/* Which workspaces each output shows in the overview.
 *
 * A window element exists once in the DOM, so a workspace can only be drawn on
 * one screen — rendering every workspace on every output would have each grid
 * steal the windows from the last. Dealing them out instead means both monitors
 * are used, which is the point of having two.
 *
 * Each output keeps the workspace it is currently displaying, so the thumbnail
 * of what you were just looking at is on the screen you were looking at. The
 * rest are dealt round-robin in order. */
function overviewAssignment() {
  /* Every workspace, not only the occupied ones. An overview is how you get
     somewhere, and the empty workspace you want to move a window to has to be
     visible to be a target. It also keeps the grid a grid: with one workspace
     in use the alternative is a single cell at almost full size, which looks
     like nothing happened. */
  const names = [...outputs.keys()];
  const assignment = new Map(names.map((name) => [name, []]));

  const remaining = [];
  for (let n = 1; n <= WORKSPACES; n++) remaining.push(n);
  for (const name of names) {
    const own = outputs.get(name).workspace;
    const at = remaining.indexOf(own);
    if (at >= 0) {
      remaining.splice(at, 1);
      assignment.get(name).push(own);
    }
  }

  let turn = 0;
  for (const n of remaining) {
    assignment.get(names[turn % names.length]).push(n);
    turn++;
  }

  for (const list of assignment.values()) list.sort((a, b) => a - b);
  return assignment;
}

function renderOverview(output, list) {
  const grid = document.createElement('div');
  grid.className = 'overview';

  if (list.length === 0) return grid;
  const area = output.windowsEl.getBoundingClientRect();

  /* Squarish grid: enough columns that the thumbnails stay wide rather than
     tall, since an output is wider than it is high. */
  const columns = Math.ceil(Math.sqrt(list.length)) || 1;
  const rows = Math.ceil(list.length / columns);
  const cellWidth = area.width / columns;
  const cellHeight = area.height / rows;
  const scale = Math.min(cellWidth / area.width, cellHeight / area.height) * 0.9;

  grid.style.gridTemplateColumns = `repeat(${columns}, 1fr)`;

  for (const n of list) {
    const cell = document.createElement('div');
    cell.className = 'thumb'
      + (n === output.workspace ? ' current' : '')
      + (idsOf(n).length === 0 ? ' empty-workspace' : '');

    const label = document.createElement('span');
    label.className = 'thumb-label';
    label.textContent = String(n);
    cell.append(label);

    const inner = document.createElement('div');
    inner.className = 'thumb-inner';
    inner.style.width = `${area.width}px`;
    inner.style.height = `${area.height}px`;
    inner.style.transform = `scale(${scale})`;

    const root = workspaces.get(n);
    const rendered = root
      ? (layoutMode === 'scrolling'
        ? renderStrip(root, { ...output, workspace: n },
          { width: area.width, height: area.height })
        : renderTree(root))
      : null;
    if (rendered) inner.append(rendered);

    /* Floating windows belong to the thumbnail of their own workspace too. */
    for (const [id, floating] of floats) {
      if (floating.workspace !== n) continue;
      const view = views.get(id);
      if (!view) continue;
      view.el.classList.add('floating');
      Object.assign(view.el.style, {
        left: `${floating.x}px`, top: `${floating.y}px`,
        width: `${floating.width}px`, height: `${floating.height}px`,
      });
      inner.append(view.el);
      renderedIds.add(id);
    }

    /* Everything drawn in this thumbnail is drawn at its scale, and clipped to
       the thumbnail rather than to the whole output — otherwise a window whose
       layout overflows spills across its neighbours, since the surfaces the
       compositor draws are not bounded by the thumbnail's `overflow: hidden`. */
    for (const id of idsOf(n)) {
      overviewScales.set(id, scale);
      overviewCells.set(id, cell);
    }

    overviewThumbs.set(n, cell);

    cell.append(inner);
    cell.addEventListener('mousedown', () => {
      /* Switch the output the thumbnail is on, not whichever was last active:
         clicking a workspace on the right monitor means you want it there. */
      setOverview(false);
      setActiveOutput(output.name);
      switchWorkspace(output.name, n);
    });
    grid.append(cell);
  }

  return grid;
}

/* ------------------------------------------------------------------------
 * Saving and restoring the layout
 * --------------------------------------------------------------------- */

/* Serialise a subtree, replacing each window with the application that was in
 * it. A leaf whose application is unknown — a slot that was never claimed — is
 * dropped rather than written back out, or an application that never returns
 * would haunt the layout across every future restart. */
function serialiseNode(node) {
  if (node.type === 'leaf') {
    const view = views.get(node.id);
    const app = view ? (view.app_id || view.title) : node.app;
    if (!app) return null;
    return {
      type: 'leaf', app,
      weight: node.weight ?? 1,
      ...(node.width !== undefined ? { width: node.width } : {}),
    };
  }

  const children = node.children.map(serialiseNode).filter(Boolean);
  if (children.length === 0) return null;
  return {
    type: 'split', dir: node.dir, layout: node.layout ?? 'split',
    weight: node.weight ?? 1, active: node.active ?? 0, children,
    ...(node.width !== undefined ? { width: node.width } : {}),
  };
}

function serialiseSession() {
  const saved = {
    version: 1, layout: layoutMode, workspaces: {}, outputs: {}, floating: [],
  };

  for (const [n, root] of workspaces) {
    const tree = serialiseNode(root);
    if (tree !== null) saved.workspaces[n] = tree;
  }

  /* Floating windows live outside the tree, so walking it misses them
     entirely — they came back tiled, in whatever order they happened to open.
     Their rect is the whole of their layout, so it is what gets written. */
  for (const [id, floating] of floats) {
    const view = views.get(id);
    const app = view ? (view.app_id || view.title) : null;
    if (!app) continue;
    saved.floating.push({
      app,
      workspace: floating.workspace,
      x: floating.x, y: floating.y,
      width: floating.width, height: floating.height,
    });
  }
  for (const [name, output] of outputs) {
    saved.outputs[name] = {
      workspace: output.workspace,
      ...(output.previous != null ? { previous: output.previous } : {}),
    };
  }
  return saved;
}

let saveTimer = null;

/* Debounced: the layout changes many times a second while dragging, and the
 * state only has to be right by the time the compositor next dies. */
function saveSession() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    send({ type: 'session.save', state: JSON.stringify(serialiseSession()) });
  }, 1000);
}

/* Rebuild the tree from a saved one, as slots waiting to be claimed. */
function reviveNode(node) {
  if (node.type === 'leaf') {
    const leaf = newLeaf(nextSlotId--);
    leaf.app = node.app;
    leaf.weight = node.weight ?? 1;
    if (node.width !== undefined) leaf.width = node.width;
    slotsPending++;
    return leaf;
  }

  const split = newSplit(node.dir === 'vertical' ? 'vertical' : 'horizontal');
  split.layout = node.layout ?? 'split';
  split.weight = node.weight ?? 1;
  split.active = node.active ?? 0;
  if (node.width !== undefined) split.width = node.width;
  split.children = (node.children ?? []).map(reviveNode);
  return split;
}

function restoreSession(text) {
  if (!text) return;

  let saved;
  try {
    saved = JSON.parse(text);
  } catch (error) {
    console.error('viewport: saved layout is not valid JSON, ignoring');
    return;
  }
  if (!saved || saved.version !== 1) return;

  /* Only into an empty session. Restoring over windows that are already open
     would move them somewhere they were never asked to be. */
  if (views.size > 0) return;

  for (const [n, tree] of Object.entries(saved.workspaces ?? {})) {
    const revived = reviveNode(tree);
    if (revived) workspaces.set(Number(n), revived);
  }
  floatSlots = (saved.floating ?? []).filter((slot) => slot && slot.app);

  for (const [name, state] of Object.entries(saved.outputs ?? {})) {
    const output = outputs.get(name);
    if (output && Number.isFinite(state.workspace)) {
      output.workspace = state.workspace;
      if (Number.isFinite(state.previous)) output.previous = state.previous;
    }
  }

  /* Nothing is showing yet — every leaf is a slot — but the workspace
     assignment and the shape are in place for the windows to arrive into. */
  if (slotsPending > 0 || floatSlots.length > 0) {
    setTimeout(dropUnclaimedSlots, SLOT_TIMEOUT_MS);
  }
  relayoutAll();
}

/* Give up on slots nothing came back for. */
function dropUnclaimedSlots() {
  floatSlots = [];
  if (slotsPending === 0) return;
  slotsPending = 0;

  for (const [n, root] of workspaces) {
    let removed = false;
    for (const [leaf] of walk(root)) {
      if (leaf.id < 0) {
        removeLeaf(leaf.id);
        removed = true;
      }
    }
    if (removed) collapse(root, true);
  }
  treeGeneration++;
  relayoutAll();
}

/* The slot a newly opened window belongs in, if it left one behind.
 *
 * Matched on application, in tree order, preferring a workspace that is not
 * currently occupied by another instance — three terminals reopening should
 * land in the three places three terminals were, and which of them goes where
 * is not something the layout can know or needs to. */
function claimSlot(id, app) {
  if (slotsPending === 0 || !app) return false;

  for (const [n, root] of workspaces) {
    for (const [leaf] of walk(root)) {
      if (leaf.id < 0 && leaf.app === app) {
        leaf.id = id;
        delete leaf.app;
        slotsPending--;
        return true;
      }
    }
  }
  return false;
}

/* How long a notification stays when the sender does not say.
 *
 * The specification lets an application pass -1 for "you decide" and 0 for
 * "never expire". Critical notifications are the case that matters: the
 * specification says they must not expire on their own, and an application
 * marking something critical has usually decided it needs an answer. */
const NOTIFICATION_TIMEOUT_MS = 5000;

function showNotification(message) {
  dropNotification(message.id, false);

  const el = document.createElement('div');
  el.className = 'notification urgency-' + (message.urgency ?? 1);

  const head = document.createElement('div');
  head.className = 'notification-head';

  const app = document.createElement('span');
  app.className = 'notification-app';
  app.textContent = message.app_name || 'notification';
  head.append(app);

  const close = document.createElement('button');
  close.className = 'notification-close';
  close.textContent = '×';
  close.addEventListener('mousedown', (event) => {
    event.stopPropagation();
    send({ type: 'notification.dismiss', id: message.id });
    dropNotification(message.id, false);
  });
  head.append(close);
  el.append(head);

  if (message.summary) {
    const summary = document.createElement('div');
    summary.className = 'notification-summary';
    summary.textContent = message.summary;
    el.append(summary);
  }
  if (message.body) {
    const body = document.createElement('div');
    body.className = 'notification-body';
    /* textContent, not innerHTML: a notification body is text from an arbitrary
       program, and the shell is a web page. Rendering it as markup would let
       any application that can send a notification run script in the desktop. */
    body.textContent = message.body;
    el.append(body);
  }

  const actions = (message.actions ?? []).filter((a) => a.key !== 'default');
  if (actions.length > 0) {
    const row = document.createElement('div');
    row.className = 'notification-actions';
    for (const action of actions) {
      const button = document.createElement('button');
      button.textContent = action.label || action.key;
      button.addEventListener('mousedown', (event) => {
        event.stopPropagation();
        send({ type: 'notification.action', id: message.id, action: action.key });
        dropNotification(message.id, false);
      });
      row.append(button);
    }
    el.append(row);
  }

  /* Clicking the body invokes the default action if there is one, which is what
     every notification daemon does and what applications expect. */
  el.addEventListener('mousedown', () => {
    const fallback = (message.actions ?? []).find((a) => a.key === 'default');
    if (fallback) {
      send({ type: 'notification.action', id: message.id, action: 'default' });
    } else {
      send({ type: 'notification.dismiss', id: message.id });
    }
    dropNotification(message.id, false);
  });

  notificationsEl.append(el);

  const timeout = message.timeout;
  const critical = (message.urgency ?? 1) >= 2;
  const ms = timeout > 0 ? timeout
    : (timeout === 0 || critical ? 0 : NOTIFICATION_TIMEOUT_MS);

  notifications.set(message.id, {
    el,
    timer: ms > 0
      ? setTimeout(() => dropNotification(message.id, true), ms)
      : null,
  });
}

/* Remove one from the screen. `expired` distinguishes a timer running out from
 * a user acting, because the sending application is told which happened. */
function dropNotification(id, expired) {
  const entry = notifications.get(id);
  if (!entry) return;

  clearTimeout(entry.timer);
  entry.el.remove();
  notifications.delete(id);

  if (expired) send({ type: 'notification.expire', id });
}

/* The first rule matching a window, or null.
 *
 * app_id is the identity worth matching: it is what the application calls
 * itself rather than what it happens to be showing. Title is offered as well
 * because some applications — a browser, a terminal multiplexer — give every
 * window the same app_id and differ only in what they display. Both are
 * substring matches, since an exact one would need the user to know the
 * application's internal name exactly. */
function ruleFor(appId, title) {
  const haystackApp = (appId || '').toLowerCase();
  const haystackTitle = (title || '').toLowerCase();

  return windowRules.find((rule) => {
    if (rule.app_id && !haystackApp.includes(String(rule.app_id).toLowerCase())) {
      return false;
    }
    if (rule.title && !haystackTitle.includes(String(rule.title).toLowerCase())) {
      return false;
    }
    /* A rule with neither would match everything, which is never what someone
       meant to write. */
    return Boolean(rule.app_id || rule.title);
  }) ?? null;
}

/* The place a floating window left behind, if it had one. Returns the rect to
 * reopen it at, or null. */
function claimFloatSlot(id, app) {
  if (floatSlots.length === 0 || !app) return null;

  const at = floatSlots.findIndex((slot) => slot.app === app);
  if (at < 0) return null;

  const [slot] = floatSlots.splice(at, 1);
  return slot;
}

/* Move one window to a workspace, without it having to be focused first. The
 * overview can act on any window on screen, not just the current one. */
function moveViewToWorkspace(id, n) {
  if (n < 1 || n > WORKSPACES) return false;
  if (workspaceOf(id) === n) return false;

  const floating = floats.get(id);
  if (floating) {
    floating.workspace = n;
  } else {
    removeLeaf(id);
    if (layoutMode === 'scrolling') {
      const root = workspaceRoot(n);
      root.dir = 'horizontal';
      const leaf = newLeaf(id);
      leaf.width = COLUMN_WIDTHS[1];
      root.children.push(leaf);
    } else {
      workspaceRoot(n).children.push(newLeaf(id));
    }
  }

  treeGeneration++;
  return true;
}

/* Pressing a window in the overview either takes you to it or moves it.
 *
 * Which one is decided on release, by where the pointer ended up: released over
 * the thumbnail it started in, it is a click and you go there; released over a
 * different one, the window is moved to that workspace and the overview stays
 * open so you can keep arranging. Dragging is the only gesture available here
 * — the compositor routes all input to the shell while the overview is up, so
 * the window under the pointer never sees any of this. */
function beginOverviewDrag(event, id) {
  event.preventDefault();
  event.stopPropagation();

  const from = overviewCells.get(id);
  const view = views.get(id);
  view?.el.classList.add('dragging-overview');

  const thumbAt = (x, y) => {
    for (const [n, cell] of overviewThumbs) {
      const r = cell.getBoundingClientRect();
      if (x >= r.left && x < r.left + r.width &&
          y >= r.top && y < r.top + r.height) {
        return { workspace: n, cell };
      }
    }
    return null;
  };

  const onUp = (up) => {
    window.removeEventListener('mouseup', onUp);
    view?.el.classList.remove('dragging-overview');

    const target = thumbAt(up.clientX, up.clientY);
    if (target !== null && target.cell !== from) {
      if (moveViewToWorkspace(id, target.workspace)) {
        send({ type: 'view.focus', id });
        relayoutAll();
      }
      return;
    }

    /* A click: go to the window. The output showing the overview is the one
       that takes you there. */
    const workspace = workspaceOf(id);
    setOverview(false);
    if (workspace !== null) {
      const name = activeOutputName();
      setActiveOutput(name);
      switchWorkspace(name, workspace);
    }
    send({ type: 'view.focus', id });
  };

  window.addEventListener('mouseup', onUp);
}

function setOverview(active) {
  if (overviewActive === active) return;
  overviewActive = active;

  if (!active) {
    overviewScales.clear();
    overviewCells.clear();
    overviewThumbs.clear();
  }

  /* The compositor routes input to the shell while this is up: the windows on
     screen are miniatures, and a click on one means "go there". */
  send({ type: 'shell.overview', active });
  relayoutAll();
}

/* Move the strip under the fingers. The compositor sends a delta per touchpad
 * event; the shell owns where the limits are. */
function gestureScroll(dx) {
  if (layoutMode !== 'scrolling') return;

  const output = outputs.get(activeOutputName());
  if (!output) return;
  const workspace = output.workspace;

  gestureWorkspace = workspace;
  const at = scrollOffsets.get(workspace) ?? 0;
  /* Clamped in renderStrip against the real strip length, which is only known
     once the columns have been measured. */
  scrollOffsets.set(workspace, Math.max(0, at + dx));
  relayoutAll();
}

/* The gesture ended. Focus whichever column the strip was left on, so the
 * ordinary follow logic agrees with where the user put it — otherwise the next
 * relayout would scroll back to wherever focus happened to be. */
function gestureSettle() {
  const workspace = gestureWorkspace;
  gestureWorkspace = null;
  if (workspace === null) return;

  const root = workspaceRoot(workspace);
  const area = windowsAreaOf(workspace);
  if (!area) return;

  const scroll = scrollOffsets.get(workspace) ?? 0;
  const centre = scroll + (area.right - area.left) / 2;

  /* Whichever column contains the middle of the screen. */
  let offset = 0;
  let landed = null;
  for (const column of root.children) {
    const width = (area.right - area.left) * (column.width ?? COLUMN_WIDTHS[1]);
    if (centre >= offset && centre < offset + width) {
      landed = column;
      break;
    }
    offset += width + gapPx();
  }
  if (landed === null) landed = root.children[root.children.length - 1];
  if (!landed) return;

  const id = landed.type === 'leaf' ? landed.id : [...walk(landed)][0][0].id;
  send({ type: 'view.focus', id });
  relayoutAll();
}

/* Step to the next or previous workspace on the active output, for a vertical
 * three-finger swipe. */
function stepWorkspace(delta) {
  const name = activeOutputName();
  const output = outputs.get(name);
  if (!output) return;

  const next = output.workspace + delta;
  if (next < 1 || next > WORKSPACES) return;
  switchWorkspace(name, next);
}

/* The column holding a window, as an index into the strip. */
function columnIndexOf(workspace, id) {
  const root = workspaceRoot(workspace);
  return root.children.findIndex((column) => column.type === 'leaf'
    ? column.id === id
    : [...walk(column)].some(([leaf]) => leaf.id === id));
}

function focusedWorkspace() {
  return focusedId != null ? workspaceOf(focusedId) : null;
}

/* Move focus along the strip, or up and down inside the focused column. The
 * compositor cannot do this itself here: the column you are moving to is
 * usually scrolled off screen, and directional focus works from what is on it. */
function scrollFocus(direction) {
  const workspace = focusedWorkspace();

  /* Nothing focused, or nothing on this workspace: the keypress still means
     "go that way", so it falls through to the monitor in that direction — the
     same thing the compositor's own directional focus does when it finds no
     window. */
  if (workspace === null) {
    focusOutputDirection(direction);
    return;
  }

  const root = workspaceRoot(workspace);
  const columns = root.children;
  if (columns.length === 0) {
    focusOutputDirection(direction);
    return;
  }

  const firstOf = (column) =>
    column.type === 'leaf' ? column.id : [...walk(column)][0][0].id;

  if (direction === 'first' || direction === 'last') {
    send({ type: 'view.focus',
      id: firstOf(columns[direction === 'first' ? 0 : columns.length - 1]) });
    return;
  }

  const index = columnIndexOf(workspace, focusedId);

  if (direction === 'left' || direction === 'right') {
    const next = index + (direction === 'right' ? 1 : -1);
    /* Off the end of the strip is not a dead end: carry on to the next
       monitor, which is what the same keys do when tiling. Without this the
       leftmost and rightmost columns trapped focus on one screen. */
    if (next < 0 || next >= columns.length) {
      focusOutputDirection(direction);
      return;
    }
    send({ type: 'view.focus', id: firstOf(columns[next]) });
    return;
  }

  /* Up and down stay inside the column, and fall through to the monitor above
     or below once there is nothing left to step onto. */
  const column = columns[index];
  const leaves = column && column.type !== 'leaf'
    ? [...walk(column)].map(([leaf]) => leaf) : [];
  const at = leaves.findIndex((leaf) => leaf.id === focusedId);
  const next = at + (direction === 'down' ? 1 : -1);

  if (at < 0 || next < 0 || next >= leaves.length) {
    focusOutputDirection(direction);
    return;
  }
  send({ type: 'view.focus', id: leaves[next].id });
}

/* Move the focused window along the strip, or within its column. Moving left or
 * right carries the whole window into the neighbouring position as its own
 * column, which is what niri does. */
function scrollMove(direction) {
  const workspace = focusedWorkspace();
  if (workspace === null) return false;

  const root = workspaceRoot(workspace);
  const index = columnIndexOf(workspace, focusedId);
  if (index < 0) return false;

  if (direction === 'left' || direction === 'right') {
    const target = index + (direction === 'right' ? 1 : -1);
    if (target < 0 || target >= root.children.length) return false;
    const [column] = root.children.splice(index, 1);
    root.children.splice(target, 0, column);
    treeGeneration++;
    relayoutAll();
    return true;
  }

  const column = root.children[index];
  if (column.type === 'leaf') return false;

  const at = column.children.findIndex((child) =>
    child.type === 'leaf' && child.id === focusedId);
  const target = at + (direction === 'down' ? 1 : -1);
  if (at < 0 || target < 0 || target >= column.children.length) return false;

  [column.children[at], column.children[target]] =
    [column.children[target], column.children[at]];
  treeGeneration++;
  relayoutAll();
  return true;
}

/* Pull the first window of the next column into this one, stacking it below the
 * focused window. The inverse of expel, and the pair is how columns are built
 * up and taken apart without a tree to split. */
function consumeWindow() {
  const workspace = focusedWorkspace();
  if (workspace === null) return;

  const root = workspaceRoot(workspace);
  const index = columnIndexOf(workspace, focusedId);
  if (index < 0 || index + 1 >= root.children.length) return;

  const next = root.children[index + 1];
  let moved;
  if (next.type === 'leaf') {
    moved = next;
    root.children.splice(index + 1, 1);
  } else {
    moved = next.children.shift();
    if (next.children.length === 0) root.children.splice(index + 1, 1);
  }
  if (!moved) return;

  let column = root.children[index];
  if (column.type === 'leaf') {
    /* A single-window column becomes a real stack the moment it holds two. */
    const stack = newSplit('vertical');
    stack.width = column.width ?? COLUMN_WIDTHS[1];
    stack.children = [column];
    root.children[index] = stack;
    column = stack;
  }
  column.children.push(moved);

  treeGeneration++;
  relayoutAll();
}

/* Push the focused window out of its column into one of its own, to the right. */
function expelWindow() {
  const workspace = focusedWorkspace();
  if (workspace === null) return;

  const root = workspaceRoot(workspace);
  const index = columnIndexOf(workspace, focusedId);
  if (index < 0) return;

  const column = root.children[index];
  if (column.type === 'leaf') return; // already alone

  const at = column.children.findIndex((child) =>
    child.type === 'leaf' && child.id === focusedId);
  if (at < 0) return;

  const [moved] = column.children.splice(at, 1);
  moved.width = column.width ?? COLUMN_WIDTHS[1];
  root.children.splice(index + 1, 0, moved);

  if (column.children.length === 1 && column.children[0].type === 'leaf') {
    /* One window left: collapse the stack back to a plain column. */
    const only = column.children[0];
    only.width = column.width;
    root.children[index] = only;
  }

  treeGeneration++;
  relayoutAll();
}

/* Step the focused column through the width presets. Widening a column pushes
 * the rest of the strip along rather than taking space from a neighbour. */
function cycleColumnWidth() {
  const workspace = focusedWorkspace();
  if (workspace === null) return;

  const root = workspaceRoot(workspace);
  const column = root.children[columnIndexOf(workspace, focusedId)];
  if (!column) return;

  const current = COLUMN_WIDTHS.indexOf(column.width ?? COLUMN_WIDTHS[1]);
  column.width = COLUMN_WIDTHS[(current + 1) % COLUMN_WIDTHS.length];
  relayoutAll();
}

/* The same for the focused window's share of its column's height. */
function cycleWindowHeight() {
  const workspace = focusedWorkspace();
  if (workspace === null) return;

  const found = findLeaf(focusedId);
  if (!found || found.parent.children.length < 2) return;

  const current = COLUMN_HEIGHTS.indexOf(found.leaf.weight);
  const next = COLUMN_HEIGHTS[(current + 1) % COLUMN_HEIGHTS.length];
  /* Weights are relative within the column, so this is a share rather than a
     fraction of the screen — the same number reads as roughly the same size. */
  found.leaf.weight = next * found.parent.children.length;
  relayoutAll();
}

/* ------------------------------------------------------------------------
 * Resizing
 * --------------------------------------------------------------------- */

const MIN_WEIGHT = 0.15;

/* Bumped whenever the tree changes shape. A divider drag captures the value it
 * started with and stops if it no longer matches: closing a window mid-drag
 * would otherwise leave the handler shifting weight between siblings that have
 * moved or ceased to exist, stranding a window at whatever fraction it held. */
let treeGeneration = 0;

/* Shift weight between two adjacent children, keeping their total constant so
 * the rest of the layout does not shuffle. */
function shiftWeight(parent, index, fraction) {
  const a = parent.children[index];
  const b = parent.children[index + 1];
  if (!a || !b) return false;

  const total = (a.weight ?? 1) + (b.weight ?? 1);
  const next = Math.min(Math.max((a.weight ?? 1) + fraction * total,
    MIN_WEIGHT), total - MIN_WEIGHT);

  if (next === a.weight) return false;
  a.weight = next;
  b.weight = total - next;
  return true;
}

function beginDividerDrag(event, node, index) {
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
    if (shiftWeight(node, index, delta / extent)) relayoutAll();
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
function resizeFocused(direction) {
  if (focusedId == null) return;

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
  /* A floating window resizes by simply becoming that much bigger — there are
     no siblings to take the space from. Clamped so a drag cannot shrink it to
     nothing and leave a window that can no longer be grabbed. */
  const floating = floats.get(id);
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
function windowsAreaOf(workspace) {
  const name = hostOfWorkspace(workspace);
  const output = name !== null ? outputs.get(name) : null;
  if (!output) return null;

  const rect = output.windowsEl.getBoundingClientRect();
  return {
    left: rect.left, top: rect.top,
    right: rect.left + rect.width, bottom: rect.top + rect.height,
  };
}

function reportGeometry(id) {
  const view = views.get(id);
  if (!view) return;

  const rect = view.viewport.getBoundingClientRect();
  /* In the overview the element is inside a scaled container, so the measured
     rect is where it appears but not the size the client should be. The
     compositor is told the real size and the factor to draw it at. */
  const scale = overviewScales.get(id) ?? 1;
  const box = {
    x: Math.round(rect.left),
    y: Math.round(rect.top),
    width: Math.round(rect.width / scale),
    height: Math.round(rect.height / scale),
  };

  if (box.width <= 0 || box.height <= 0) {
    send({ type: 'view.visible', id, visible: false });
    return;
  }

  /* How much of the window falls inside its output.
   *
   * A scrolled strip pushes columns past the edge, and `overflow: hidden`
   * does not help: it bounds what the *shell* paints, and the window is a real
   * Wayland surface the compositor draws itself. Left unclipped, a column
   * scrolled off the left of one monitor appears on the monitor beside it. The
   * compositor crops the surface to this rect. */
  /* In the overview a window is bounded by its thumbnail, not by the output. */
  const cell = overviewCells.get(id);
  const area = cell
    ? (() => {
      const r = cell.getBoundingClientRect();
      return { left: r.left, top: r.top,
        right: r.left + r.width, bottom: r.top + r.height };
    })()
    : windowsAreaOf(workspaceOf(id));
  let clip = null;
  if (area) {
    /* Intersect on screen, where the window is drawn — with a scale in effect
       the measured rect is much smaller than the window's real size, so
       intersecting the real size against screen coordinates would clip almost
       everything away and, at zero width, hide the window entirely.

       The result is then converted back into the window's own coordinates,
       which is the space the compositor expects and the only one that means
       anything to the client's buffer. */
    const left = Math.max(rect.left, area.left);
    const top = Math.max(rect.top, area.top);
    const right = Math.min(rect.left + rect.width, area.right);
    const bottom = Math.min(rect.top + rect.height, area.bottom);

    clip = {
      x: Math.round(box.x + (left - rect.left) / scale),
      y: Math.round(box.y + (top - rect.top) / scale),
      width: Math.max(0, Math.round((right - left) / scale)),
      height: Math.max(0, Math.round((bottom - top) / scale)),
    };
  }

  const prev = view.box;
  const prevClip = view.clip;
  if (prev && prev.x === box.x && prev.y === box.y &&
      prev.width === box.width && prev.height === box.height &&
      prev.scale === scale && sameBox(prevClip, clip)) {
    return false;
  }

  if (prev && prev.scale !== scale) {
    /* Scale alone changed — worth a message even when the rect did not. */
  }

  view.box = { ...box, scale };
  view.clip = clip;

  const message = { type: 'view.layout', id, ...box };
  if (scale !== 1) message.scale = scale;
  if (clip) message.clip = clip;
  send(message);
  return true;
}

/* Keep reporting geometry while the layout is still moving.
 *
 * Window frames are CSS, so they animate for free — but a window's *contents*
 * are a real surface the compositor draws at whatever rect it was last told.
 * Sampling once after a relayout would slide the frame smoothly and snap the
 * contents straight to the destination, which looks worse than no animation at
 * all. So geometry is resampled every frame until it stops changing.
 *
 * Self-terminating rather than tied to transition events: transitions get
 * interrupted, replaced and cancelled constantly while dragging, and a missed
 * `transitionend` would leave a window stranded mid-flight. A few extra frames
 * of sampling after everything settles is the cheaper mistake. */
const PUMP_IDLE_FRAMES = 3;
/* Hard ceiling, about a second. Rounding can leave two windows disagreeing by
 * a pixel forever, and a permanently spinning frame callback that sends IPC on
 * every tick is a worse bug than a window settling a frame late. */
const PUMP_MAX_FRAMES = 60;
let pumpRemaining = 0;
let pumping = false;

/* Animate the move from an old layout to a new one, by inverting it.
 *
 * The tree is rebuilt from scratch on every relayout, so a window's new
 * position arrives as a fresh set of parent elements — there is no property
 * change on a retained element for CSS to transition, which is why simply
 * declaring a transition animated nothing at all. The window elements
 * themselves *are* retained though, so the standard trick works: measure where
 * each one was, let the new layout land, then offset it back to where it came
 * from and release it.
 *
 * Position only, deliberately. A window's contents are a real surface, and the
 * compositor follows the frame by resizing the client — so animating size would
 * ask every client to relayout itself sixty times a second for the duration.
 * Sliding costs nothing extra; growing would cost a great deal. */
function flipFrom(before) {
  if (reducedMotion()) {
    /* Still has to reach its destination, just without the journey. */
    releaseStrips();
    return;
  }

  const strips = pendingStrips();
  const moved = [];
  for (const [id, view] of views) {
    if (view.el.hidden) continue;
    const from = before.get(id);
    if (!from) continue; // was hidden or is new: nothing to animate from

    const to = view.el.getBoundingClientRect();
    const dx = from.left - to.left;
    const dy = from.top - to.top;
    if (dx === 0 && dy === 0) continue;

    view.el.classList.add('flipping');
    view.el.style.transform = `translate(${dx}px, ${dy}px)`;
    moved.push(view.el);
  }
  if (moved.length === 0 && strips.length === 0) return;

  /* One forced reflow for the whole batch, so the browser sees the offset
     position before the transition is re-enabled. Reading a layout property
     per element would do the same thing N times over. */
  void document.documentElement?.offsetWidth;

  for (const el of moved) {
    el.classList.remove('flipping');
    el.style.transform = '';
  }
  releaseStrips();
}

/* Strips that were rendered at their previous scroll position and still need
 * to be moved to their new one. */
function pendingStrips() {
  const found = [];
  for (const output of outputs.values()) {
    const strip = output.windowsEl.children[0];
    if (strip?.classList?.contains('strip')) found.push(strip);
  }
  return found;
}

function releaseStrips() {
  for (const strip of pendingStrips()) {
    const target = Number(strip.dataset.scroll);
    if (Number.isFinite(target)) {
      strip.style.transform = `translateX(${-target}px)`;
    }
  }
}

function pumpGeometry() {
  pumpRemaining = PUMP_IDLE_FRAMES;
  if (pumping) return;
  pumping = true;

  let budget = PUMP_MAX_FRAMES;

  const step = () => {
    let changed = false;
    for (const [id, view] of views) {
      if (!view.el.hidden && reportGeometry(id)) changed = true;
    }

    pumpRemaining = changed ? PUMP_IDLE_FRAMES : pumpRemaining - 1;
    if (pumpRemaining > 0 && --budget > 0) {
      requestAnimationFrame(step);
    } else {
      pumping = false;
    }
  };

  requestAnimationFrame(step);
}

/* Whether the user has asked for less motion. Checked at the point of use so
 * changing the setting takes effect without a reload. */
function reducedMotion() {
  return typeof matchMedia === 'function' &&
    matchMedia('(prefers-reduced-motion: reduce)').matches;
}

/* Fade a newly opened window in.
 *
 * This one cannot be CSS. The frame is the shell's, but the window's contents
 * are a surface the compositor draws, and no style here touches it — so the
 * opacity is tweened in JS and sent over IPC, where the compositor applies it
 * to the surface itself. Short, and skipped entirely when motion is reduced. */
const FADE_MS = 120;

function fadeIn(id) {
  if (reducedMotion()) return;

  send({ type: 'view.opacity', id, opacity: 0 });

  const started = performance.now();
  const step = (now) => {
    const t = Math.min((now - started) / FADE_MS, 1);
    /* Ease-out, matching the CSS curve closely enough that a window opening
       beside one that is moving does not look like a different animation. */
    send({ type: 'view.opacity', id, opacity: 1 - Math.pow(1 - t, 3) });
    if (t < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}

function sameBox(a, b) {
  if (a === b) return true;
  if (!a || !b) return false;
  return a.x === b.x && a.y === b.y &&
    a.width === b.width && a.height === b.height;
}

const resizeObserver = new ResizeObserver((entries) => {
  for (const entry of entries) {
    const id = Number(entry.target.dataset.viewId);
    if (Number.isFinite(id)) reportGeometry(id);
  }
});

function relayoutAll() {
  /* Where everything was, before the tree is thrown away and rebuilt. */
  const before = new Map();
  for (const [id, view] of views) {
    if (!view.el.hidden) before.set(id, view.el.getBoundingClientRect());
  }

  /* workspace -> output showing it. A workspace appears at most once. */
  const shown = new Map();
  for (const [name, output] of outputs) shown.set(output.workspace, name);

  /* Render first, then decide what is visible. Which windows are on screen is
     now a result of rendering rather than something knowable in advance: a
     collapsed tab, or a column scrolled off the strip, is on its workspace and
     still not shown. */
  renderedIds = new Set();

  if (overviewActive) {
    overviewScales.clear();
    overviewCells.clear();
    overviewThumbs.clear();
  }
  const assignment = overviewActive ? overviewAssignment() : null;

  for (const [name, output] of outputs) {
    const root = workspaces.get(output.workspace);
    /* Only one output shows the overview: a window element exists once in the
       DOM, so two grids would fight over the same windows and the second would
       simply steal them. The others go blank for the duration. */
    const rendered = overviewActive
      ? renderOverview(output, assignment.get(name) ?? [])
      : (root
        ? (layoutMode === 'scrolling'
          ? renderStrip(root, output)
          : renderTree(root))
        : null);

    output.windowsEl.replaceChildren();
    output.windowsEl.classList.toggle('scrolling', layoutMode === 'scrolling');
    if (rendered) output.windowsEl.append(rendered);

    /* Floating windows are positioned rather than laid out, so they are
       appended after the tree and take their rect from their own record. CSS
       lifts them above the tiled windows; the compositor stacks the real
       surfaces to match when it is told their new rects. */
    for (const [id, floating] of floats) {
      if (overviewActive) break; // thumbnails place their own
      if (floating.workspace !== output.workspace) continue;
      const view = views.get(id);
      if (!view) continue;
      view.el.classList.add('floating');
      output.windowsEl.append(view.el);
      renderedIds.add(id);
      if (isFullscreen(id)) continue; // covers the output; rect ignored
      Object.assign(view.el.style, {
        left: `${floating.x}px`,
        top: `${floating.y}px`,
        width: `${floating.width}px`,
        height: `${floating.height}px`,
        flexGrow: '',
      });
    }

    output.emptyEl.hidden = idsOf(output.workspace).length > 0;

    /* A fullscreen window covers the whole output, bar included — that is what
     * fullscreen means, and a video with a status bar across the top is not
     * fullscreen. The bar also stays hidden while explicitly toggled off. */
    const fullscreenHere = fullscreenOn(output.workspace) !== null;
    output.el.classList.toggle('overview-active', overviewActive);
    output.el.classList.toggle('has-fullscreen',
      fullscreenHere && !overviewActive);
    output.el.classList.toggle('bar-hidden', output.barHidden);
    renderBar(name);
  }

  for (const [id, view] of views) {
    const workspace = workspaceOf(id);
    /* Normally a window is on screen only if its workspace is: `shown` maps
       each displayed workspace to the output showing it. The overview breaks
       that rule on purpose — it draws every workspace at once, including the
       ones no monitor is displaying — so there the thumbnail's own render is
       the whole answer. Without this exception a window on a workspace that
       happened to be off screen stayed hidden, and its thumbnail came out
       labelled empty. */
    const visible = overviewActive
      ? renderedIds.has(id)
      : (workspace !== null && shown.has(workspace) && renderedIds.has(id));

    view.el.hidden = !visible;
    if (!visible && view.box !== null) {
      view.box = null;
      send({ type: 'view.visible', id, visible: false });
    }
    view.el.classList.toggle('focused', id === focusedId);
    view.el.classList.toggle('fullscreen', isFullscreen(id));
  }

  /* Offset every window back to where it was and let it slide into place. */
  flipFrom(before);

  /* Measure after the browser has laid the new tree out, and keep measuring
     for as long as it is still moving. */
  pumpGeometry();
}

/* ------------------------------------------------------------------------
 * Outputs and workspaces
 * --------------------------------------------------------------------- */

function firstOutputName() {
  return outputs.keys().next().value ?? null;
}

function hostOfWorkspace(n) {
  for (const [name, output] of outputs) {
    if (output.workspace === n) return name;
  }
  return null;
}

function lowestFreeWorkspace() {
  for (let n = 1; n <= WORKSPACES; n++) {
    if (hostOfWorkspace(n) === null) return n;
  }
  return 1;
}

function startingWorkspace(name) {
  const preferred = OUTPUT_WORKSPACE[name];
  if (preferred !== undefined && hostOfWorkspace(preferred) === null) {
    return preferred;
  }
  return lowestFreeWorkspace();
}

/* Tracked explicitly rather than derived from focus: switching to an empty
 * workspace focuses nothing, and inferring it would act on the wrong monitor. */
function activeOutputName() {
  if (activeOutput && outputs.has(activeOutput)) return activeOutput;
  return firstOutputName();
}

function setActiveOutput(name) {
  if (!name || !outputs.has(name) || activeOutput === name) return;
  activeOutput = name;
  /* The compositor needs this to place new windows and layer surfaces: it
   * would otherwise decide from the cursor, which is wrong after a keyboard
   * focus move. */
  send({ type: 'output.active', name });
}

function syncOutputs(list) {
  const seen = new Set();

  for (const info of list) {
    seen.add(info.name);
    let output = outputs.get(info.name);

    if (!output) {
      const fragment = desktopTemplate.content.cloneNode(true);
      const el = fragment.querySelector('.desktop');
      output = {
        /* The output's own name, so code holding the record does not have to
           be handed the key separately. */
        name: info.name,
        el,
        windowsEl: el.querySelector('.windows'),
        emptyEl: el.querySelector('.empty'),
        workspacesEl: el.querySelector('.workspaces'),
        taskbarEl: el.querySelector('.taskbar'),
        modeEl: el.querySelector('.mode'),
        modules: {
          clock: el.querySelector('.clock'),
          cpu: el.querySelector('.cpu'),
          memory: el.querySelector('.memory'),
          load: el.querySelector('.load'),
          disk: el.querySelector('.disk'),
          net: el.querySelector('.net'),
        },
        barHidden: false,
        workspace: 0,
      };
      el.dataset.output = info.name;
      el.addEventListener('mouseenter', () => setActiveOutput(info.name));
      outputsEl.append(el);
      outputs.set(info.name, output);
      output.workspace = startingWorkspace(info.name);
      if (activeOutput === null) activeOutput = info.name;
    }

    Object.assign(output.el.style, {
      left: `${info.x}px`,
      top: `${info.y}px`,
      width: `${info.width}px`,
      height: `${info.height}px`,
    });

    output.hdr = info.hdr === true;
    output.hdrCapable = info.hdr_capable === true;
    output.el.classList.toggle('hdr', output.hdr);

    /* Panels reserve space through layer-shell exclusive zones; the compositor
       reports what is left. Expressed as insets so the bar and the tiling area
       shift together and everything downstream keeps measuring elements as
       before. Older compositor builds omit the fields — treat that as nothing
       reserved rather than collapsing the desktop to zero. */
    const usable = {
      x: info.usable_x ?? info.x,
      y: info.usable_y ?? info.y,
      width: info.usable_width ?? info.width,
      height: info.usable_height ?? info.height,
    };
    output.el.style.setProperty('--rsv-left', `${usable.x - info.x}px`);
    output.el.style.setProperty('--rsv-top', `${usable.y - info.y}px`);
    output.el.style.setProperty('--rsv-right',
      `${(info.x + info.width) - (usable.x + usable.width)}px`);
    output.el.style.setProperty('--rsv-bottom',
      `${(info.y + info.height) - (usable.y + usable.height)}px`);
  }

  for (const [name, output] of outputs) {
    if (seen.has(name)) continue;
    output.el.remove();
    outputs.delete(name);
    if (activeOutput === name) activeOutput = null;
  }

  relayoutAll();
}

/* A workspace lives on exactly one output at a time. Asking for one already
 * elsewhere moves focus there rather than creating a second copy — otherwise
 * each monitor grows its own "workspace 1". */
function switchWorkspace(name, n) {
  const output = outputs.get(name);
  if (!output || n < 1 || n > WORKSPACES) return;

  /* Asking for the workspace you are already on takes you back to the one
     before it — sway's workspace_auto_back_and_forth. The same key becomes a
     toggle between two workspaces, which is most of what switching is: you
     were somewhere, you looked at something else, you want to go back, and you
     should not have to remember where you came from to do it. */
  if (output.workspace === n && output.previous != null &&
      output.previous !== n) {
    n = output.previous;
  }

  const host = hostOfWorkspace(n);
  if (host !== null && host !== name) {
    setActiveOutput(host);
    focusFirstOn(host);
    return;
  }
  if (output.workspace === n) return;

  output.previous = output.workspace;
  output.workspace = n;
  setActiveOutput(name);
  relayoutAll();
  focusFirstOn(name);
}

/* Straight to the previous workspace, whatever it was. The same thing the
 * repeated switch does, for a binding of its own. */
function workspaceBack(name) {
  const output = outputs.get(name);
  if (!output || output.previous == null) return;
  switchWorkspace(name, output.previous);
}

function focusFirstOn(name) {
  const output = outputs.get(name);
  if (!output) return;
  const ids = idsOf(output.workspace);
  send(ids.length > 0
    ? { type: 'view.focus', id: ids[0] }
    : { type: 'shell.focus' });
}

/* Move to the monitor in a direction, even if it has no windows.
 *
 * The compositor falls through to this when directional focus finds no window
 * that way — matching sway, where Mod4+l from the rightmost window on the left
 * monitor lands you on the right monitor whether or not anything is open
 * there. Outputs are compared by their layout rects, which the compositor
 * already sends us. */
/* Nearest output in a direction, by layout rect. */
function adjacentOutput(direction) {
  const current = outputs.get(activeOutputName());
  if (!current) return null;

  const from = current.el.getBoundingClientRect();
  const axis = (direction === 'left' || direction === 'right') ? 'x' : 'y';
  const forward = direction === 'right' || direction === 'down';

  let best = null;
  let bestDistance = Infinity;

  for (const [name, output] of outputs) {
    if (output === current) continue;

    const rect = output.el.getBoundingClientRect();
    const delta = axis === 'x' ? rect.left - from.left : rect.top - from.top;
    if (forward ? delta <= 0 : delta >= 0) continue;

    const distance = Math.abs(delta);
    if (distance < bestDistance) {
      best = name;
      bestDistance = distance;
    }
  }
  return best;
}

function focusOutputDirection(direction) {
  const best = adjacentOutput(direction);
  if (best !== null) {
    setActiveOutput(best);
    focusFirstOn(best);
  }
}

/* Carry the focused window to the monitor in a direction, onto whatever
 * workspace that monitor is showing. Used when the window is already at the
 * edge of its own workspace's tree — sway's behaviour. */
function moveViewToOutput(id, direction) {
  const target = adjacentOutput(direction);
  if (target === null) return false;

  const output = outputs.get(target);
  if (!output) return false;

  removeLeaf(id);
  const leaf = newLeaf(id);
  workspaceRoot(output.workspace).children.push(leaf);
  treeGeneration++;

  setActiveOutput(target);
  relayoutAll();
  /* Focus follows the window, as it does when moving within a workspace. */
  send({ type: 'view.focus', id });
  return true;
}

function moveToWorkspace(n) {
  if (focusedId == null || n < 1 || n > WORKSPACES) return;
  const found = findLeaf(focusedId);
  if (!found || found.workspace === n) return;

  removeLeaf(focusedId);
  workspaceRoot(n).children.push(newLeaf(focusedId));
  relayoutAll();
}

/* ------------------------------------------------------------------------
 * Windows
 * --------------------------------------------------------------------- */

/* Make a window floating or tiled.
 *
 * The two are mutually exclusive: a floating window is not in the tree at all,
 * so switching means moving it between the two representations rather than
 * setting a flag. Its rect is remembered while tiled, so toggling back and
 * forth returns it to where it was. */
function setFloating(id, floating, rect = null) {
  const view = views.get(id);
  if (!view) return;

  const workspace = workspaceOf(id);
  if (workspace === null) return;

  if (floating) {
    if (floats.has(id)) return;
    removeLeaf(id);

    const output = outputs.get(hostOfWorkspace(workspace) ?? activeOutputName());
    const area = output
      ? output.windowsEl.getBoundingClientRect()
      : { width: 800, height: 600 };

    /* Positions are relative to the tiling area of the output, since that is
       the element a floating window is absolutely positioned inside — not the
       page. Using page coordinates here would offset every dialog on the second
       monitor by the width of the first. */
    const width = Math.min(rect?.width ?? view.naturalWidth ?? 640, area.width);
    const height = Math.min(rect?.height ?? view.naturalHeight ?? 480,
      area.height);

    floats.set(id, {
      workspace,
      /* Centred. A dialog that opens in a corner reads as a glitch, and the
         client never says where it wants to be. */
      x: rect?.x ?? Math.round((area.width - width) / 2),
      y: rect?.y ?? Math.round((area.height - height) / 2),
      width: Math.round(width),
      height: Math.round(height),
    });
  } else {
    if (!floats.has(id)) return;
    floats.delete(id);

    /* Inline geometry would otherwise fight flexbox once it is back in the
       tree. */
    Object.assign(view.el.style, { left: '', top: '', width: '', height: '' });
    view.el.classList.remove('floating');

    insertLeaf(workspace, id);
  }

  treeGeneration++;
  relayoutAll();
}

function toggleFloating(id) {
  if (id == null) return;
  setFloating(id, !floats.has(id));
}

/* Drag a floating window, in response to Mod4 + left drag reported by the
 * compositor. Tiled windows have no position of their own, so this is a no-op
 * for them rather than an error. */
function moveByDelta(id, dx, dy) {
  const floating = floats.get(id);
  if (!floating) return;

  /* Follow the pointer exactly for the duration of the drag; the class is
     dropped once the window stops moving. */
  const view = views.get(id);
  view?.el.classList.add('dragging');
  clearTimeout(view?.dragTimer);
  if (view) {
    view.dragTimer = setTimeout(
      () => view.el.classList.remove('dragging'), 120);
  }

  const output = outputs.get(hostOfWorkspace(floating.workspace) ?? '');
  const area = output?.windowsEl?.getBoundingClientRect();

  floating.x += dx;
  floating.y += dy;

  /* Leave a grabbable strip on screen. A window dragged fully off the edge
     cannot be dragged back, and there is no titlebar to alt-tab to it by. */
  if (area) {
    const margin = 40;
    floating.x = Math.max(margin - floating.width,
      Math.min(floating.x, area.width - margin));
    floating.y = Math.max(0, Math.min(floating.y, area.height - margin));
  }

  relayoutAll();
}

function addView({ id, title, app_id, output: outputName, min_width, min_height,
    floating, width, height, replay }) {
  /* view.added is replayed on load and on view.query, so the same view
   * legitimately arrives more than once. */
  if (views.has(id)) return;

  /* The shell's active output decides, not the compositor's hint: focus may
   * have moved by keyboard since. The hint is only a fallback for the first
   * window, before any output is active. */
  const name = outputs.has(activeOutputName())
    ? activeOutputName()
    : (outputs.has(outputName) ? outputName : firstOutputName());
  const output = outputs.get(name);
  if (!output) return; // no outputs yet; the replay covers us

  const fragment = windowTemplate.content.cloneNode(true);
  const el = fragment.querySelector('.window');
  const viewport = fragment.querySelector('.viewport');

  el.dataset.viewId = String(id);
  viewport.dataset.viewId = String(id);
  el.addEventListener('mousedown', (event) => {
    if (overviewActive) {
      beginOverviewDrag(event, id);
      return;
    }
    send({ type: 'view.focus', id });
  });

  /* A client's own minimum, enforced by flexbox. Without it a divider drag
   * happily shrinks the hole past what the client accepts: the client keeps
   * its real size, the frame does not, and the window overflows its slot. */
  if (min_width > 0) el.style.minWidth = `${min_width}px`;
  if (min_height > 0) el.style.minHeight = `${min_height}px`;

  views.set(id, {
    el, viewport, title, app_id, box: null,
    naturalWidth: width, naturalHeight: height,
  });
  resizeObserver.observe(viewport);

  /* A rule can send the window somewhere else entirely, float it, or set the
     width of the column it opens in. Applied before anything is inserted, so
     the window goes straight where it belongs rather than appearing in one
     place and jumping to another. */
  const rule = ruleFor(app_id, title);
  const target = rule && Number.isFinite(rule.workspace)
    ? rule.workspace : output.workspace;

  /* A window you just opened should be the one you are typing into, however it
     was started — a keybinding, a launcher, a link handler. The exceptions are
     narrow: a replayed window is not new, and a rule that deliberately put this
     one on another workspace was an instruction to leave it there, not to be
     taken there. */
  const focusIt = () => {
    if (!replay && target === output.workspace) {
      send({ type: 'view.focus', id });
    }
  };

  if (rule && rule.floating) {
    insertLeaf(target, id);
    setFloating(id, true, Number.isFinite(rule.width) && Number.isFinite(rule.height)
      ? { x: rule.x ?? 0, y: rule.y ?? 0, width: rule.width, height: rule.height }
      : null);
    fadeIn(id);
    focusIt();
    saveSession();
    return;
  }

  /* A window that was floating comes back floating, at the rect it had —
     including one the compositor would not have floated on its own, because
     the decision to float it was the user's and is worth keeping. */
  const floatSlot = claimFloatSlot(id, app_id || title);
  if (floatSlot) {
    insertLeaf(floatSlot.workspace, id);
    setFloating(id, true, floatSlot);
    fadeIn(id);
    focusIt();
    saveSession();
    return;
  }

  /* If this application left a slot in the tree, it goes back into it. */
  if (!floating && claimSlot(id, app_id || title)) {
    treeGeneration++;
    relayoutAll();
    fadeIn(id);
    focusIt();
    saveSession();
    return;
  }

  insertLeaf(target, id);

  if (rule && Number.isFinite(rule.width) && layoutMode === 'scrolling') {
    /* In the strip a rule's width is the column's share of the screen, which
       is the only width a tiled window has there. */
    const found = findLeaf(id);
    const column = found
      ? workspaceRoot(target).children[columnIndexOf(target, id)] : null;
    if (column) column.width = Math.max(0.1, Math.min(rule.width, 1));
  }

  /* The compositor decides this from what the client says about itself — a
     parent toplevel, an X11 dialog type, a fixed size. Applied after the leaf
     is inserted so setFloating has a workspace to read. */
  if (floating) {
    setFloating(id, true);
    fadeIn(id);
    focusIt();
    saveSession();
    return;
  }

  treeGeneration++;
  relayoutAll();
  fadeIn(id);
  focusIt();
  saveSession();
}

function removeView(id) {
  const view = views.get(id);
  if (!view) return;

  const wasFocused = focusedId === id;
  const workspace = workspaceOf(id);

  resizeObserver.unobserve(view.viewport);
  view.el.remove();
  views.delete(id);
  floats.delete(id);
  removeLeaf(id);
  treeGeneration++;
  const fullscreenWorkspace = workspace !== null && fullscreens.get(workspace) === id
    ? workspace : null;
  if (fullscreenWorkspace !== null) fullscreens.delete(fullscreenWorkspace);

  relayoutAll();
  /* A closed window means one fewer slot to restore next time. */
  saveSession();

  /* Keep something focused on the workspace the window left behind. Closing
   * with Mod4+Shift+q, or a terminal exiting on Ctrl-D, would otherwise drop
   * focus to the shell and leave the keyboard pointing at nothing. */
  if (wasFocused) {
    focusedId = null;
    const survivors = workspace !== null ? idsOf(workspace) : [];
    send(survivors.length > 0
      ? { type: 'view.focus', id: survivors[0] }
      : { type: 'shell.focus' });
  }
}

function setFullscreen(id, on) {
  const workspace = workspaceOf(id);
  if (workspace === null) return;

  /* Only this workspace's fullscreen window is displaced. Whatever the other
     monitor is showing is none of its business. */
  const previous = fullscreens.get(workspace) ?? null;
  if (on) {
    fullscreens.set(workspace, id);
  } else if (previous === id) {
    fullscreens.delete(workspace);
  }
  const current = fullscreens.get(workspace) ?? null;

  /* The client has to be told, not just resized: applications rearrange their
   * own layout on the fullscreen state rather than on size alone. */
  if (previous !== null && previous !== current) {
    send({ type: 'view.fullscreen', id: previous, fullscreen: false });
  }
  if (current !== null && current !== previous) {
    send({ type: 'view.fullscreen', id: current, fullscreen: true });
  }
  relayoutAll();
}

function toggleFullscreen() {
  if (focusedId == null) return;
  setFullscreen(focusedId, !isFullscreen(focusedId));
}

function toggleBar() {
  const output = outputs.get(activeOutputName());
  if (!output) return;
  output.barHidden = !output.barHidden;
  relayoutAll();
}

/* ------------------------------------------------------------------------
 * Bar
 * --------------------------------------------------------------------- */

function formatBytes(n) {
  if (!Number.isFinite(n) || n <= 0) return '0B';
  const units = ['B', 'K', 'M', 'G', 'T'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n < 10 ? n.toFixed(1) : Math.round(n)}${units[i]}`;
}

function renderBar(name) {
  const output = outputs.get(name);
  if (!output) return;

  /* Every workspace that exists anywhere, since they are global. */
  const occupied = new Set([output.workspace]);
  for (const n of workspaces.keys()) {
    if (leavesOf(n).length > 0) occupied.add(n);
  }
  /* A workspace holding only floating windows is still occupied. */
  for (const floating of floats.values()) occupied.add(floating.workspace);

  output.workspacesEl.replaceChildren();
  for (const n of [...occupied].sort((a, b) => a - b)) {
    const host = hostOfWorkspace(n);
    const button = document.createElement('button');
    button.className = (n === output.workspace ? 'active' : '')
      + (host !== null && host !== name ? ' elsewhere' : '');
    button.textContent = String(n);
    button.addEventListener('click', () => switchWorkspace(name, n));
    output.workspacesEl.append(button);
  }

  output.taskbarEl.replaceChildren();
  for (const id of idsOf(output.workspace)) {
    const view = views.get(id);
    if (!view) continue;
    const button = document.createElement('button');
    button.className = (id === focusedId ? 'focused' : '')
      + (floats.has(id) ? ' floating' : '');
    button.textContent = view.title || view.app_id || `view ${id}`;
    button.addEventListener('click', () => send({ type: 'view.focus', id }));
    output.taskbarEl.append(button);
  }

  /* Show the active binding mode, as sway's bar does — without it there is no
   * way to tell that hjkl has stopped moving focus and started resizing.
   *
   * HDR shares the indicator, because it is invisible otherwise: the picture
   * changes and nothing says why, and a monitor left in HDR by a mis-hit key
   * looks like a broken colour profile rather than a setting. Both can be true
   * at once, so both are shown. */
  const labels = [];
  if (output.hdr) labels.push('HDR');
  if (currentMode !== 'default') labels.push(currentMode.toUpperCase());

  output.modeEl.textContent = labels.join(' · ');
  output.modeEl.hidden = labels.length === 0;

  const m = output.modules;
  const now = new Date();
  const date = now.toLocaleDateString('en-US',
    { weekday: 'short', month: 'short', day: '2-digit' });
  const time = `${String(now.getHours()).padStart(2, '0')}:` +
    `${String(now.getMinutes()).padStart(2, '0')}`;
  m.clock.textContent = `󰥔 ${date}, ${time}`;

  const s = lastStatus;
  m.cpu.textContent = s.cpu >= 0 ? ` ${Math.round(s.cpu)}%` : '';
  m.memory.textContent = s.memory >= 0 ? `󰍛 ${Math.round(s.memory)}%` : '';
  m.load.textContent = s.load !== undefined ? `󰓅 ${s.load.toFixed(2)}` : '';
  m.disk.textContent = s.disk_free ? `󰋊 ${formatBytes(s.disk_free)}` : '';
  m.net.textContent = s.net_rx !== undefined
    ? `󰇚 ${formatBytes(s.net_rx)}/s 󰕒 ${formatBytes(s.net_tx)}/s` : '';
}

function renderBars() {
  for (const name of outputs.keys()) renderBar(name);
}

/* The clock changes once a minute, but a second's granularity keeps it from
 * lagging visibly after a resume. Redrawing the bar is cheap; note that every
 * shell repaint is a composited frame, so do not make this faster. */
setInterval(renderBars, 1000);

/* ------------------------------------------------------------------------
 * Commands forwarded from the compositor
 * --------------------------------------------------------------------- */

function handleShellCommand(command, args) {
  const arg = args[0];
  const n = Number(arg);

  switch (command) {
    case 'workspace.switch':
      if (Number.isFinite(n)) switchWorkspace(activeOutputName(), n);
      break;
    case 'workspace.back':
      workspaceBack(activeOutputName());
      break;
    case 'workspace.move':
      if (Number.isFinite(n)) moveToWorkspace(n);
      break;
    case 'window.fullscreen':
      toggleFullscreen();
      break;
    case 'window.move': {
      if (focusedId == null) break;
      /* The strip moves whole columns rather than rearranging a tree. At its
       * ends the window carries over to the next monitor, exactly as it does
       * when tiling — the strip is per workspace, not per session. */
      if (layoutMode === 'scrolling' && !floats.has(focusedId)) {
        if (!scrollMove(arg)) moveViewToOutput(focusedId, arg);
        break;
      }
      /* A floating window has no place in the tree to move within, so the same
       * keys nudge it instead — sway does this too. */
      if (floats.has(focusedId)) {
        const step = 40;
        moveByDelta(focusedId,
          arg === 'left' ? -step : arg === 'right' ? step : 0,
          arg === 'up' ? -step : arg === 'down' ? step : 0);
        break;
      }
      /* Try to move within the workspace first; at the edge, carry the window
       * to the next monitor instead of stopping. */
      if (moveLeaf(focusedId, arg)) {
        relayoutAll();
      } else {
        moveViewToOutput(focusedId, arg);
      }
      break;
    }
    case 'layout.split':
      pendingSplit = arg === 'vertical' ? 'vertical' : 'horizontal';
      break;
    case 'bar.toggle':
      toggleBar();
      break;
    case 'layout.toggle':
      toggleLayout();
      break;
    case 'layout.resize':
      resizeFocused(arg);
      break;
    case 'window.fullscreen.set': {
      /* A client asked to go fullscreen itself, e.g. a video player. */
      const id = Number(args[0]);
      const on = args[1] === '1';
      if (Number.isFinite(id)) {
        /* The client asked for this itself, so it already knows — just lay it
         * out, without echoing the state back and starting a loop. */
        const workspace = workspaceOf(id);
        if (workspace !== null) {
          if (on) {
            fullscreens.set(workspace, id);
          } else if (fullscreens.get(workspace) === id) {
            fullscreens.delete(workspace);
          }
        }
        relayoutAll();
      }
      break;
    }
    case 'layout.resize.delta':
      resizeByDelta(Number(args[0]), Number(args[1]), Number(args[2]));
      break;
    case 'layout.move.delta':
      moveByDelta(Number(args[0]), Number(args[1]), Number(args[2]));
      break;
    case 'layout.float.toggle':
      toggleFloating(focusedId);
      break;
    case 'layout.tabbed':
      setContainerLayout('tabbed');
      break;
    case 'layout.stacked':
      setContainerLayout('stacked');
      break;

    /* Scrolling layout. Bound only when the compositor is configured for it,
       but harmless to receive otherwise. */
    case 'layout.focus':
      scrollFocus(arg);
      break;
    case 'layout.consume':
      consumeWindow();
      break;
    case 'layout.expel':
      expelWindow();
      break;
    case 'layout.column.width':
      cycleColumnWidth();
      break;
    case 'layout.column.height':
      cycleWindowHeight();
      break;

    /* Touchpad. The compositor keeps three-finger swipes for itself and sends
       them here; everything else goes to the focused client. */
    case 'gesture.scroll':
      gestureScroll(Number(arg));
      break;
    case 'gesture.settle':
      gestureSettle();
      break;
    case 'layout.overview':
      setOverview(!overviewActive);
      break;
    case 'output.hdr':
      /* No state of its own: the compositor owns whether an output is in HDR,
         and toggling is asking it to flip whatever it currently has. */
      send({ type: 'output.hdr', name: activeOutputName() });
      break;
    case 'workspace.step':
      stepWorkspace(Number(arg));
      break;
    case 'mode.changed':
      currentMode = arg || 'default';
      renderBars();
      break;
    case 'output.focus':
      focusOutputDirection(arg);
      break;
    default:
      console.warn('unknown shell command:', command, args);
  }
}

/* ------------------------------------------------------------------------
 * Inbound
 * --------------------------------------------------------------------- */

window.addEventListener('viewport', (event) => {
  const message = event.detail;

  switch (message.type) {
    case 'config':
      /* Which layout model to run. Sent on connect and on reload, so switching
         it in the config file and reloading takes effect without a restart —
         the tree survives, it is only presented differently. */
      windowRules = Array.isArray(message.rules) ? message.rules : [];
      if (message.layout === 'scrolling' || message.layout === 'tiling') {
        if (message.layout !== layoutMode) {
          layoutMode = message.layout;
          normaliseForLayout();
          relayoutAll();
        }
      }
      break;

    case 'output.layout':
      syncOutputs(message.outputs);
      send({ type: 'view.query' });
      break;

    case 'view.added':
      addView(message);
      break;

    case 'view.props': {
      const view = views.get(message.id);
      if (view) {
        view.title = message.title;
        view.app_id = message.app_id;
        renderBars();
      }
      break;
    }

    case 'view.removed':
      removeView(message.id);
      break;

    case 'view.focused': {
      focusedId = message.id || null;
      const found = focusedId != null ? findLeaf(focusedId) : null;
      if (found) {
        /* Focusing a window on a hidden workspace brings that workspace to a
         * monitor rather than leaving the user looking at nothing. */
        let host = hostOfWorkspace(found.workspace);
        if (host === null) {
          host = activeOutputName();
          const output = outputs.get(host);
          if (output) output.workspace = found.workspace;
        }
        setActiveOutput(host);
      }
      relayoutAll();
      break;
    }

    case 'status.update':
      lastStatus = message;
      renderBars();
      break;

    case 'notification.add':
      showNotification(message);
      break;
    case 'notification.close':
      /* The application withdrew it, so nothing is sent back. */
      dropNotification(message.id, false);
      break;

    case 'session.restore':
      restoreSession(message.state);
      break;

    case 'shell.command':
      handleShellCommand(message.command, message.args ?? []);
      /* Most commands rearrange something; saving is debounced, so asking on
         every one of them costs a timer reset. */
      saveSession();
      break;

    case 'error':
      console.error(`viewport: ${message.context}: ${message.message}`);
      break;
  }
});

window.addEventListener('resize', relayoutAll);

send({ type: 'output.query' });
/* Before view.query: the layout has to be in place as slots before the windows
   that fill them are replayed, or every one of them lands in a default
   position first and the restore has nothing left to do. */
send({ type: 'session.query' });
send({ type: 'view.query' });
