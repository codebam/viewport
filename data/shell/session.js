/* SPDX-License-Identifier: MIT
 *
 * Remembering the layout, placing new windows, and notifications.
 *
 * The saved blob is this file's own format; the compositor stores and returns it
 * without interpreting it. Slots are how a restored layout waits for the
 * applications that will fill it.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */
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
  for (const [id, floating, view] of floatingEntries()) {
    const app = view.app_id || view.title;
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
    if (!tree || typeof tree !== 'object') continue;

    /* A workspace root is always a split — every function that touches the
       tree reads root.children without checking, because workspaceRoot() only
       ever creates splits. This file does not: session.json is state on disk,
       editable by hand and written by whichever version ran last, and a
       workspace that holds one window is a plausible thing to write as a bare
       leaf. Restoring that put a leaf where a split was assumed, and the shell
       threw on the next window opened there — a crash on startup, which is
       when nothing is left to recover it. */
    let revived = reviveNode(tree);
    if (revived && revived.type !== 'split') {
      const root = newSplit('horizontal');
      root.children = [revived];
      revived = root;
    }
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
  const from = workspaceOf(id);
  if (from === n) return false;

  const floating = floatingOf(id);
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

  /* Fullscreen is recorded per workspace, so it has to travel with the window.
     Left behind, the workspace it came from goes on claiming a fullscreen
     window that is no longer there — bar hidden, and a layout drawn around a
     view it cannot find — while the window arrives on the new workspace as an
     ordinary one. */
  if (from !== null && fullscreens.get(from) === id) {
    fullscreens.delete(from);
    fullscreens.set(n, id);
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

  const view = views.get(id);
  const from = view?.overview?.cell;
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
    clearOverviewState();
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
function scrollMove(direction, targetId = focusedId) {
  if (targetId == null) return false;
  const workspace = workspaceOf(targetId);
  if (workspace === null) return false;

  const root = workspaceRoot(workspace);
  const index = columnIndexOf(workspace, targetId);
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
    child.type === 'leaf' && child.id === targetId);
  const target = at + (direction === 'down' ? 1 : -1);
  if (at < 0 || target < 0 || target >= column.children.length) return false;

  [column.children[at], column.children[target]] =
    [column.children[target], column.children[at]];
  treeGeneration++;
  relayoutAll();
  return true;
}

function colContainsAnySelected(column) {
  if (!column) return false;
  if (column.type === 'leaf') return selectedIds.has(column.id);
  return [...walk(column)].some(([leaf]) => selectedIds.has(leaf.id));
}

function scrollMoveSelected(direction) {
  const ws = activeWorkspace();
  if (ws === null) return false;

  const root = workspaceRoot(ws);
  if (!root || root.children.length === 0) return false;

  const columns = root.children;
  const selectedIndices = [];
  columns.forEach((col, idx) => {
    if (colContainsAnySelected(col)) selectedIndices.push(idx);
  });

  if (selectedIndices.length === 0) {
    if (focusedId != null) return scrollMove(direction, focusedId);
    return false;
  }

  if (direction === 'left' || direction === 'right') {
    if (direction === 'right') {
      const lastIdx = selectedIndices[selectedIndices.length - 1];
      if (lastIdx < columns.length - 1) {
        const targetCol = columns[lastIdx + 1];
        const selectedCols = columns.filter((_, idx) => selectedIndices.includes(idx));
        const remainingCols = columns.filter((_, idx) => !selectedIndices.includes(idx));
        
        const insertPos = remainingCols.indexOf(targetCol) + 1;
        remainingCols.splice(insertPos, 0, ...selectedCols);
        root.children = remainingCols;
        treeGeneration++;
        relayoutAll();
        return true;
      } else {
        let moved = false;
        for (const id of Array.from(selectedIds)) {
          if (moveViewToOutput(id, 'right')) moved = true;
        }
        return moved;
      }
    } else if (direction === 'left') {
      const firstIdx = selectedIndices[0];
      if (firstIdx > 0) {
        const targetCol = columns[firstIdx - 1];
        const selectedCols = columns.filter((_, idx) => selectedIndices.includes(idx));
        const remainingCols = columns.filter((_, idx) => !selectedIndices.includes(idx));

        const insertPos = remainingCols.indexOf(targetCol);
        remainingCols.splice(insertPos, 0, ...selectedCols);
        root.children = remainingCols;
        treeGeneration++;
        relayoutAll();
        return true;
      } else {
        let moved = false;
        for (const id of Array.from(selectedIds)) {
          if (moveViewToOutput(id, 'left')) moved = true;
        }
        return moved;
      }
    }
  }

  if (direction === 'up' || direction === 'down') {
    let moved = false;
    for (const col of columns) {
      if (col.type === 'split' && colContainsAnySelected(col)) {
        const leaves = col.children;
        const selInCol = leaves.map((c, i) => (c.type === 'leaf' && selectedIds.has(c.id)) ? i : -1).filter(i => i >= 0);
        if (selInCol.length > 0) {
          if (direction === 'down') {
            const last = selInCol[selInCol.length - 1];
            if (last < leaves.length - 1) {
              [leaves[last], leaves[last + 1]] = [leaves[last + 1], leaves[last]];
              moved = true;
            }
          } else if (direction === 'up') {
            const first = selInCol[0];
            if (first > 0) {
              [leaves[first], leaves[first - 1]] = [leaves[first - 1], leaves[first]];
              moved = true;
            }
          }
        }
      }
    }
    if (moved) {
      treeGeneration++;
      relayoutAll();
      return true;
    }
  }

  return false;
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

