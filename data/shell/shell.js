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

let focusedId = null;
let activeOutput = null;
/* Direction the next new window splits in, like sway's splith/splitv. */
let pendingSplit = 'horizontal';
let fullscreenId = null;
let lastStatus = {};
let currentMode = 'default';

const outputsEl = document.getElementById('outputs');
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
function newSplit(dir) {
  return { type: 'split', dir, children: [], weight: 1 };
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
function collapse(node) {
  if (node.type === 'leaf') return;

  node.children.forEach(collapse);
  node.children = node.children.filter(
    (c) => c.type === 'leaf' || c.children.length > 0);

  if (node.children.length === 1 && node.children[0].type === 'split') {
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
  collapse(workspaces.get(found.workspace));
}

/* Insert next to the focused window, splitting in the pending direction —
 * i3's behaviour, and why two terminals side by side then Mod4+v puts the
 * third underneath the second rather than beside it. */
function insertLeaf(workspace, id) {
  const root = workspaceRoot(workspace);
  const leaf = newLeaf(id);

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
      collapse(root);
      return true;
    }
  }

  const grandparent = findParentOf(root, parent);
  if (grandparent) {
    const parentIndex = grandparent.children.indexOf(parent);
    parent.children.splice(index, 1);
    grandparent.children.splice(parentIndex + (forward ? 1 : 0), 0, leaf);
    collapse(root);
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

function renderTree(node) {
  if (node.type === 'leaf') {
    const view = views.get(node.id);
    if (view) view.el.style.flexGrow = String(node.weight ?? 1);
    return view ? view.el : null;
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
function resizeByDelta(id, dx, dy) {
  const found = findLeaf(id);
  if (!found) return;

  for (const [axis, delta] of [['horizontal', dx], ['vertical', dy]]) {
    if (delta === 0) continue;
    const target = ancestorOnAxis(id, axis);
    if (!target) continue;

    const el = views.get(id)?.el?.parentElement;
    const extent = el
      ? (axis === 'horizontal'
        ? el.getBoundingClientRect().width
        : el.getBoundingClientRect().height)
      : 0;
    if (extent <= 0) continue;

    const { parent, index } = target;
    const usePrevious = index === parent.children.length - 1;
    shiftWeight(parent, usePrevious ? index - 1 : index,
      (usePrevious ? -1 : 1) * (delta / extent));
  }
  relayoutAll();
}

/* sway's `layout toggle split`: flip the container the focused window is in,
 * rearranging the windows already inside it rather than affecting the next
 * one to open. */
function toggleLayout() {
  if (focusedId == null) return;
  const found = findLeaf(focusedId);
  if (!found) return;

  found.parent.dir =
    found.parent.dir === 'horizontal' ? 'vertical' : 'horizontal';
  relayoutAll();
}

function reportGeometry(id) {
  const view = views.get(id);
  if (!view) return;

  const rect = view.viewport.getBoundingClientRect();
  const box = {
    x: Math.round(rect.left),
    y: Math.round(rect.top),
    width: Math.round(rect.width),
    height: Math.round(rect.height),
  };

  if (box.width <= 0 || box.height <= 0) {
    send({ type: 'view.visible', id, visible: false });
    return;
  }

  const prev = view.box;
  if (prev && prev.x === box.x && prev.y === box.y &&
      prev.width === box.width && prev.height === box.height) {
    return;
  }

  view.box = box;
  send({ type: 'view.layout', id, ...box });
}

const resizeObserver = new ResizeObserver((entries) => {
  for (const entry of entries) {
    const id = Number(entry.target.dataset.viewId);
    if (Number.isFinite(id)) reportGeometry(id);
  }
});

function relayoutAll() {
  /* workspace -> output showing it. A workspace appears at most once. */
  const shown = new Map();
  for (const [name, output] of outputs) shown.set(output.workspace, name);

  for (const [id, view] of views) {
    const found = findLeaf(id);
    const visible = found !== null && shown.has(found.workspace);

    view.el.hidden = !visible;
    if (!visible && view.box !== null) {
      view.box = null;
      send({ type: 'view.visible', id, visible: false });
    }
    view.el.classList.toggle('focused', id === focusedId);
    view.el.classList.toggle('fullscreen', id === fullscreenId);
  }

  for (const [name, output] of outputs) {
    const root = workspaces.get(output.workspace);
    const rendered = root ? renderTree(root) : null;

    output.windowsEl.replaceChildren();
    if (rendered) output.windowsEl.append(rendered);

    output.emptyEl.hidden = leavesOf(output.workspace).length > 0;

    /* A fullscreen window covers the whole output, bar included — that is what
     * fullscreen means, and a video with a status bar across the top is not
     * fullscreen. The bar also stays hidden while explicitly toggled off. */
    const fullscreenHere = fullscreenId !== null &&
      findLeaf(fullscreenId)?.workspace === output.workspace;
    output.el.classList.toggle('has-fullscreen', fullscreenHere);
    output.el.classList.toggle('bar-hidden', output.barHidden);
    renderBar(name);
  }

  /* Measure after the browser has laid the new tree out. */
  requestAnimationFrame(() => {
    for (const [id, view] of views) {
      if (!view.el.hidden) reportGeometry(id);
    }
  });
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

  const host = hostOfWorkspace(n);
  if (host !== null && host !== name) {
    setActiveOutput(host);
    focusFirstOn(host);
    return;
  }
  if (output.workspace === n) return;

  output.workspace = n;
  setActiveOutput(name);
  relayoutAll();
  focusFirstOn(name);
}

function focusFirstOn(name) {
  const output = outputs.get(name);
  if (!output) return;
  const leaves = leavesOf(output.workspace);
  send(leaves.length > 0
    ? { type: 'view.focus', id: leaves[0].id }
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

function addView({ id, title, app_id, output: outputName }) {
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
  el.addEventListener('mousedown', () => send({ type: 'view.focus', id }));

  views.set(id, { el, viewport, title, app_id, box: null });
  resizeObserver.observe(viewport);

  insertLeaf(output.workspace, id);
  treeGeneration++;
  relayoutAll();
}

function removeView(id) {
  const view = views.get(id);
  if (!view) return;

  resizeObserver.unobserve(view.viewport);
  view.el.remove();
  views.delete(id);
  removeLeaf(id);
  treeGeneration++;
  if (fullscreenId === id) fullscreenId = null;

  relayoutAll();
}

function toggleFullscreen() {
  if (focusedId == null) return;
  fullscreenId = fullscreenId === focusedId ? null : focusedId;
  relayoutAll();
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
  for (const leaf of leavesOf(output.workspace)) {
    const view = views.get(leaf.id);
    if (!view) continue;
    const button = document.createElement('button');
    button.className = leaf.id === focusedId ? 'focused' : '';
    button.textContent = view.title || view.app_id || `view ${leaf.id}`;
    button.addEventListener('click',
      () => send({ type: 'view.focus', id: leaf.id }));
    output.taskbarEl.append(button);
  }

  /* Show the active binding mode, as sway's bar does — without it there is no
   * way to tell that hjkl has stopped moving focus and started resizing. */
  output.modeEl.textContent =
    currentMode === 'default' ? '' : currentMode.toUpperCase();
  output.modeEl.hidden = currentMode === 'default';

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
    case 'workspace.move':
      if (Number.isFinite(n)) moveToWorkspace(n);
      break;
    case 'window.fullscreen':
      toggleFullscreen();
      break;
    case 'window.move':
      if (focusedId == null) break;
      /* Try to move within the workspace first; at the edge, carry the window
       * to the next monitor instead of stopping. */
      if (moveLeaf(focusedId, arg)) {
        relayoutAll();
      } else {
        moveViewToOutput(focusedId, arg);
      }
      break;
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
        fullscreenId = on ? id : (fullscreenId === id ? null : fullscreenId);
        relayoutAll();
      }
      break;
    }
    case 'layout.resize.delta':
      resizeByDelta(Number(args[0]), Number(args[1]), Number(args[2]));
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

    case 'shell.command':
      handleShellCommand(message.command, message.args ?? []);
      break;

    case 'error':
      console.error(`viewport: ${message.context}: ${message.message}`);
      break;
  }
});

window.addEventListener('resize', relayoutAll);

send({ type: 'output.query' });
send({ type: 'view.query' });
