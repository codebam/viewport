/* SPDX-License-Identifier: MIT
 *
 * The window lifecycle: added, focused, closed, floated, fullscreened.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */
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
    if (isFloating(id)) return;
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

    view.floating = {
      workspace,
      /* Centred. A dialog that opens in a corner reads as a glitch, and the
         client never says where it wants to be. */
      x: rect?.x ?? Math.round((area.width - width) / 2),
      y: rect?.y ?? Math.round((area.height - height) / 2),
      width: Math.round(width),
      height: Math.round(height),
    };
  } else {
    if (!isFloating(id)) return;
    view.floating = null;

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
  setFloating(id, !isFloating(id));
}

/* Drag a window, in response to Mod4 + left drag reported by the compositor.
 *
 * A tiled window has no position of its own, so dragging one floats it where
 * it already is and carries on from there — which is what sway does, and the
 * only reading of the gesture that does anything: the alternative was to
 * ignore the drag entirely, which is what happened before. */
function moveByDelta(id, dx, dy) {
  if (!floatingOf(id)) {
    const view = views.get(id);
    const workspace = workspaceOf(id);
    if (!view || workspace === null) return;

    /* Where it is now, so it does not jump on the first pixel of the drag.
       The frame rather than the hole: a floating rect describes the window
       element, borders and all. */
    const rect = view.el.getBoundingClientRect();
    const area = windowsAreaOf(workspace);
    setFloating(id, true, area
      ? {
        x: Math.round(rect.left - area.left),
        y: Math.round(rect.top - area.top),
        width: Math.round(rect.width),
        height: Math.round(rect.height),
      }
      : null);
  }

  const floating = floatingOf(id);
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
    if (!selectedIds.has(id)) clearSelection();
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
    /* Rect while floating, null while tiled; see floatingOf(). */
    floating: null,
    /* The frame last reported to the compositor, so an unchanged one is not
       re-sent. Null for a tiled window, which needs none. */
    frame: null,
    /* Whether the compositor has been told this window floats, which is what
       keeps it stacked over the tiled ones. Separate from `floating` above:
       that is the rect, this is what went out on the wire. */
    reportedFloating: false,
    /* Set while the overview is up; see clearOverviewState(). */
    overview: null,
  });
  resizeObserver.observe(viewport);

  /* The matrix layout's history, before the window is placed anywhere: it is
     ordered by focus rather than by the tree, so a new window is at the top of
     it whichever of the paths below ends up inserting this one.
     A replayed window is not new — it is a restart's worth of windows arriving
     at once — so it is left out and takes tree order until something focuses
     it, rather than the replay's arrival order deciding the whole column. */
  if (!replay) matrixOpened(id);

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

/* The first window inside a subtree, in tree order. */
function firstLeafIn(node) {
  if (node.type === 'leaf') return views.has(node.id) ? node.id : null;
  for (const [leaf] of walk(node)) {
    if (views.has(leaf.id)) return leaf.id;
  }
  return null;
}

/* Which window to focus once this one closes.
 *
 * Picked before the window is removed, because afterwards the tree has already
 * collapsed around the hole and there is nothing left to say where it was.
 *
 * The two layouts want different answers. In the strip, closing a window should
 * leave you on the column to its left — the strip has a direction, and being
 * dropped at its start after closing something in the middle means scrolling
 * back. In a tiling tree the meaningful neighbour is the one it shared a
 * container with, which is what "the parent" amounts to: the split it was part
 * of, not whatever happens to be first on the workspace. */
function focusAfterClosing(id) {
  const workspace = workspaceOf(id);
  if (workspace === null) return null;

  const found = findLeaf(id);

  if (layoutMode === 'scrolling' && found) {
    const root = workspaceRoot(workspace);
    const index = columnIndexOf(workspace, id);
    const column = root.children[index];

    /* Another window stacked in the same column comes first: closing one of a
       pair should not move you to a different column. */
    if (column && column.type === 'split') {
      const leaves = [...walk(column)].map(([leaf]) => leaf)
        .filter((leaf) => leaf.id !== id && views.has(leaf.id));
      if (leaves.length > 0) return leaves[0].id;
    }

    /* Then the column to the left, and only if there is none, the right. */
    for (const next of [index - 1, index + 1]) {
      const neighbour = root.children[next];
      const pick = neighbour ? firstLeafIn(neighbour) : null;
      if (pick !== null && pick !== id) return pick;
    }
  }

  if (found) {
    const siblings = found.parent.children;
    const at = siblings.indexOf(found.leaf);
    for (const next of [at - 1, at + 1]) {
      const sibling = siblings[next];
      const pick = sibling ? firstLeafIn(sibling) : null;
      if (pick !== null && pick !== id) return pick;
    }
  }

  /* Nothing beside it — a floating window, or the last one in its container.
     Anything still on the workspace beats dropping focus to the shell. */
  const remaining = idsOf(workspace).filter((other) => other !== id);
  return remaining.length > 0 ? remaining[0] : null;
}

function removeView(id) {
  const view = views.get(id);
  if (!view) return;

  const wasFocused = focusedId === id;
  const workspace = workspaceOf(id);
  /* Worked out while the tree still knows where this window was. */
  const successor = wasFocused ? focusAfterClosing(id) : null;

  resizeObserver.unobserve(view.viewport);
  view.el.remove();
  /* The floating rect and the overview state live on the record, so they go
     with it — there is no second structure left to forget. */
  views.delete(id);
  removeLeaf(id);
  /* Which window is a workspace's sun is one of the few things kept outside
     the record, because it is a property of the workspace rather than of the
     window — so it does have to be forgotten by hand. */
  solarForget(id);
  /* And the focus history the matrix layout lays itself out from, for the
     same reason: it is state about a window kept outside the record. A stale
     id there would hold a slot for a window that no longer exists. */
  matrixClosed(id);
  /* And where it sat on the canvas's plane, which is kept outside the record
     for the same reason again. A stale place there is a rectangle nothing
     draws — until fit-to-all measures the bounding box it is part of and fits
     the view around a window that is gone. */
  canvasClosed(id);
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
    send(successor !== null
      ? { type: 'view.focus', id: successor }
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

/* Colours from the config file, as CSS custom properties.
   Names are restricted to what a custom property may contain and values to
   what a colour may: this is a string from a file being written into the
   document's style, and "it is the user's own config" is not a reason to hand
   it a wider grammar than it needs. Unknown names are allowed through — the
   stylesheet decides what it reads, and a typo showing no effect is a better
   failure than a silent whitelist that has to be edited to add a variable. */
function applyTheme(theme) {
  if (!theme || typeof theme !== 'object') return;
  const style = document.documentElement.style;
  for (const [key, value] of Object.entries(theme)) {
    if (!/^[a-z][a-z0-9-]*$/.test(key)) continue;
    if (typeof value !== 'string' || !/^[#a-zA-Z0-9(),.%\s/-]+$/.test(value)) continue;
    style.setProperty('--' + key, value);
  }
}

/* The gaps from the config file's `gaps` block — the first-class way to set
   them, in contrast to the theme keys `gap` / `gap-outer` which are lengths
   for anyone who wants a unit they choose. `inner` lands on `--gap` and
   `outer` on `--gap-outer`, which `gapPx()` / `gapOuterPx()` read back for
   the layouts, and `smart` is remembered so a lone window can drop the inner
   gap. Applied after the theme so the explicit option wins on reload; a
   `gaps` block and a theme that disagree are the user's own conflict.
   Absence leaves each field's shell default standing (inner 8, outer 0,
   smart off). */
function applyGaps(gaps) {
  if (gaps === undefined || gaps === null) return;
  const style = document.documentElement.style;
  if (gaps.inner !== null && gaps.inner !== undefined) {
    const px = Number(gaps.inner);
    if (Number.isFinite(px)) style.setProperty('--gap', px + 'px');
  }
  if (gaps.outer !== null && gaps.outer !== undefined) {
    const px = Number(gaps.outer);
    if (Number.isFinite(px)) style.setProperty('--gap-outer', px + 'px');
  }
  if (gaps.smart !== null && gaps.smart !== undefined) {
    gapsSmart = gaps.smart === true;
  }
}

/* The window border from the config file's `border` block. `radius` lands on
   `--window-radius` and `width` on `--window-border`, which `.window` draws
   its corners and its edge with.

   The two properties here the compositor also reads. Everything else the
   shell is told about its own appearance it keeps to itself; these describe a
   corner that has a client surface in it, and the compositor crops that
   surface to the same corner — the outer radius less the border — so the
   values that arrive here have already been used on the other side, and
   disagreeing with them is not an option the shell has. Absence leaves each
   property as the stylesheet declares it. */
function applyBorder(border) {
  if (border === undefined || border === null) return;
  const style = document.documentElement.style;
  if (border.radius !== null && border.radius !== undefined) {
    const px = Number(border.radius);
    if (Number.isFinite(px)) style.setProperty('--window-radius', px + 'px');
  }
  if (border.width !== null && border.width !== undefined) {
    const px = Number(border.width);
    if (Number.isFinite(px)) style.setProperty('--window-border', px + 'px');
  }
  /* Smart radius, which is not a length and so is remembered rather than set
     on a property. Absent leaves it following `gaps.smart` — see
     `borderSmart` — and that is not the same as setting it false, which is
     why only a value that is actually there is read. */
  if (border.smart !== null && border.smart !== undefined) {
    borderSmart = border.smart === true;
  }
}

function applyBarMode(mode) {
  const next = mode === 'hidden' || mode === 'auto' || mode === 'visible'
    ? mode : 'visible';
  if (next === barMode) return;
  barMode = next;
  /* Existing outputs keep whatever they were told individually, except that a
     mode change is exactly when that should be reconsidered: switching to
     'visible' with every output toggled off would otherwise show nothing. */
  for (const output of outputs.values()) {
    output.barHidden = barMode !== 'visible';
  }
  relayoutAll();
}

function toggleBar() {
  const output = outputs.get(activeOutputName());
  if (!output) return;
  output.barHidden = !output.barHidden;
  relayoutAll();
}

function activeWorkspace() {
  if (focusedId != null) return workspaceOf(focusedId);
  const outputName = activeOutputName();
  const output = outputs.get(outputName);
  return output ? output.workspace : 1;
}

function nodeContains(node, id) {
  if (!node) return false;
  if (node.type === 'leaf') return node.id === id;
  return [...walk(node)].some(([leaf]) => leaf.id === id);
}

function focusParent() {
  const ws = activeWorkspace();

  if (layoutMode === 'scrolling') {
    const allIds = idsOf(ws);
    if (allIds.length === 0) return;

    const allSelected = allIds.length > 0 && allIds.every((id) => selectedIds.has(id));
    if (allSelected) {
      selectedIds.clear();
      if (focusedId != null) selectedIds.add(focusedId);
    } else {
      selectedIds.clear();
      allIds.forEach((id) => selectedIds.add(id));
    }
    relayoutAll();
    return;
  }

  if (focusedId == null) return;
  const root = workspaces.get(ws);
  if (!root) return;

  const found = findLeaf(focusedId);
  if (!found) return;

  if (selectedContainer == null || !nodeContains(selectedContainer, focusedId)) {
    selectedContainer = found.parent;
  } else {
    const parentOfContainer = findParentOf(root, selectedContainer);
    if (parentOfContainer != null) {
      selectedContainer = parentOfContainer;
    }
  }

  selectedIds.clear();
  if (selectedContainer) {
    for (const [leaf] of walk(selectedContainer)) {
      selectedIds.add(leaf.id);
    }
  }
  relayoutAll();
}


