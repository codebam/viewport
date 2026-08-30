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
    const swallowed = view?.swallowParent != null
      ? views.get(view.swallowParent) : null;
    const identity = swallowed ?? view;
    const app = swallowed
      ? (swallowed.app_id || swallowed.title || swallowed.tag)
      : (view ? (view.app_id || view.title || view.tag) : node.app);
    if (!app) return null;
    return {
      type: 'leaf', app,
      ...(identity?.tag ? { tag: identity.tag } : (node.tag ? { tag: node.tag } : {})),
      weight: node.weight ?? 1,
      ...(node.width !== undefined ? { width: node.width } : {}),
      ...(identity?.pseudotile ? { pseudotile: identity.pseudotile }
        : (node.pseudotile ? { pseudotile: node.pseudotile } : {})),
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
    /* Where everything sits on the canvas's planes, and where each plane is
       being looked at from. Written whatever the layout is, so that switching
       away from the canvas and restarting does not lose the arrangement — the
       tree survives a layout switch for the same reason, and the plane is as
       much a layout as the tree is. Empty until something has run the canvas,
       and skipped entirely when it is, so the file gains nothing for a desktop
       that never uses it. */
    canvas: serialiseCanvas(),
  };
  if (saved.canvas.places.length === 0
    && Object.keys(saved.canvas.viewports).length === 0) {
    delete saved.canvas;
  }

  for (const [n, root] of workspaces) {
    const tree = serialiseNode(root);
    if (tree !== null) saved.workspaces[n] = tree;
  }

  /* Floating windows live outside the tree, so walking it misses them
     entirely — they came back tiled, in whatever order they happened to open.
     Their rect is the whole of their layout, so it is what gets written. */
  for (const [id, view] of views) {
    const floating = view.floating;
    if (!floating) continue;
    const app = view.app_id || view.title || view.tag;
    if (!app) continue;
    saved.floating.push({
      app,
      ...(view.tag ? { tag: view.tag } : {}),
      workspace: floating.workspace,
      x: floating.x, y: floating.y,
      width: floating.width, height: floating.height,
      ...(view.pseudotile ? { pseudotile: view.pseudotile } : {}),
      ...(view.special ? { special: view.special } : {}),
      ...(view.specialOutput ? { output: view.specialOutput } : {}),
      ...(view.special === 'scratchpad' ? { hidden: view.specialHidden !== false } : {}),
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
    if (node.tag) leaf.tag = node.tag;
    leaf.weight = node.weight ?? 1;
    if (node.width !== undefined) leaf.width = node.width;
    const pseudo = safePseudoDimensions(node.pseudotile);
    if (pseudo) leaf.pseudotile = pseudo;
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
  /* And the canvas's planes, whose places wait to be claimed by the windows as
     they are replayed exactly as the floating rects above do. A file written
     before this existed has no `canvas` key and restores nothing, which is the
     same as never having run the layout. */
  restoreCanvas(saved.canvas);

  for (const [name, state] of Object.entries(saved.outputs ?? {})) {
    const output = outputs.get(name);
    if (output && Number.isFinite(state.workspace)) {
      output.workspace = state.workspace;
      if (Number.isFinite(state.previous)) output.previous = state.previous;
    }
  }

  /* Nothing is showing yet — every leaf is a slot — but the workspace
     assignment and the shape are in place for the windows to arrive into. */
  if (slotsPending > 0 || floatSlots.length > 0 || canvasSlots.length > 0) {
    setTimeout(dropUnclaimedSlots, SLOT_TIMEOUT_MS);
  }
  relayoutAll();
}

/* Give up on slots nothing came back for. */
function dropUnclaimedSlots() {
  floatSlots = [];
  /* And the places on the canvas's planes, for the same reason: a window
     opened an hour after the restart should be put where a new window goes,
     not handed the rect of something that closed during it. */
  dropCanvasSlots();
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
 * Matched on application, first one in tree order, and each is taken only
 * once — so three terminals reopening land in the three places three
 * terminals were, and which of them goes where is not something the layout
 * can know or needs to.
 *
 * Which is also why there is no preference here for a workspace that has no
 * instance on it yet. Every slot gets filled either way; a preference would
 * only change which of three identical terminals ends up on which screen,
 * and the saved layout is a better answer to that than the order the
 * applications happened to start in. */
function claimSlot(id, app, tag = null) {
  if (slotsPending === 0 || !app) return false;

  for (const [n, root] of workspaces) {
    for (const [leaf] of walk(root)) {
      const identityMatches = leaf.tag ? tag === leaf.tag : leaf.app === app;
      if (leaf.id < 0 && identityMatches) {
        leaf.id = id;
        const view = views.get(id);
        if (view && leaf.pseudotile) view.pseudotile = leaf.pseudotile;
        delete leaf.app;
        delete leaf.tag;
        delete leaf.pseudotile;
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

/* Which output a notification belongs to — the one showing the window of the
 * app that sent it.
 *
 * The notification names its app, and the shell knows where every window of
 * that app is (an app is a wayland `app_id`, its windows are the `views` that
 * carry it). Its output is the one hosting the workspace that window is on. An
 * app with no window — a daemon, a background service, a notifier run headless
 * — has no output to claim, so the notification lands on the output being
 * looked at, which is where a fallback ought to sit. */
function notificationOutputName(message) {
  const app = String(message.app_name || '').toLowerCase();
  for (const [id, view] of views) {
    if (String(view.app_id || '').toLowerCase() !== app) continue;
    const root = hostOfWorkspace(workspaceOf(id));
    if (root) return root;
  }
  /* The app has no window on screen — a daemon, a headless notifier — so it
     claims no output; the fallback is the one being looked at. */
  return activeOutputName();
}

function showNotification(message) {
  dropNotification(message.id, false);

  /* A notification is drawn over the output its source window is on, not over
     some global one — a message from an app sitting on the left monitor
     belongs in that monitor's corner, not the right one's. Resolved by the
     app that sent it; a notification from an app with no window (a daemon, a
     background service) falls back to the output being looked at. */
  const outputName = notificationOutputName(message);
  const hostEl = (outputs.get(outputName) ?? [...outputs.values()][0])
    ?.notificationsEl;
  if (!hostEl) return; // no outputs yet; a later layout will not replay this

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

  hostEl.append(el);
  animateNotificationIn(el);

  const timeout = message.timeout;
  const critical = (message.urgency ?? 1) >= 2;
  const ms = timeout > 0 ? timeout
    : (timeout === 0 || critical ? 0 : NOTIFICATION_TIMEOUT_MS);

  notifications.set(message.id, {
    el,
    output: outputName,
    timer: ms > 0
      ? setTimeout(() => dropNotification(message.id, true), ms)
      : null,
    /* Set when it is on its way out; see dropNotification. */
    fallback: null,
  });
  /* The stack grew, so where it is has changed. Reported after the entry
     is registered, because the rectangle is measured from the DOM. */
  reportNotificationRect();
}

/* Remove one from the screen. `expired` distinguishes a timer running out from
 * a user acting, because the sending application is told which happened. */
function dropNotification(id, expired) {
  const entry = notifications.get(id);
  if (!entry) return;

  clearTimeout(entry.timer);
  /* Dropped from the map now, kept on screen a moment longer. The map is what
     answers a second dismissal, a late expiry and a replacement arriving under
     the same id, and all three should find this notification already gone —
     but the element it names is still there being animated away, so the
     removal is what the animation is handed rather than something that has
     already happened by the time it starts. */
  notifications.delete(id);

  let removed = false;
  const remove = () => {
    if (removed) return;
    removed = true;
    clearTimeout(entry.fallback);
    entry.el.remove();
    reportNotificationRect();
  };
  /* The exit is a tween, and a tween runs on animation frames — which stop
     while the screens are off, because a client that is not being invited to
     draw is not drawing. A notification dismissed as the monitor goes to sleep
     has its removal frozen part way through, and what comes back with the
     screen is an element nothing on it accounts for: the strip still has a
     child, so the compositor is still told to draw that rectangle of shell
     over the windows, and no click and no timer will ever take it away.
     setTimeout is not on the animation clock, so this is the one that lands. */
  entry.fallback = setTimeout(remove, NOTIFICATION_EXIT_FALLBACK_MS);
  animateNotificationOut(entry.el, remove);
  reportNotificationRect();

  if (expired) send({ type: 'notification.expire', id });
}

/* How long an exit animation is given before it is finished by hand. Well past
 * the two tweens it is made of (`--anim` twice, so under half a second), and
 * short enough that a stuck notification is a blink rather than something the
 * user has to live with. */
const NOTIFICATION_EXIT_FALLBACK_MS = 2000;

/* A monitor went away with notifications still on it.
 *
 * Their elements went with its half of the desktop, and the rectangle the
 * compositor was given for that strip is cleared by dropOverlaysForOutput —
 * but the notifications themselves are still open as far as the applications
 * that sent them are concerned, and a message does not stop mattering because
 * a screen went to sleep. So they move to a screen that is still there, timers
 * and all, rather than being thrown away.
 *
 * With no screen left there is nowhere to move them to. They end as dismissed,
 * which is what the sender is told, because the alternative is a notification
 * this shell will never draw again and never close. */
function rehomeNotifications(gone) {
  const host = [...outputs.values()][0] ?? null;

  for (const [id, entry] of notifications) {
    if (entry.output !== gone) continue;
    if (!host) {
      clearTimeout(entry.timer);
      clearTimeout(entry.fallback);
      entry.el.remove();
      notifications.delete(id);
      send({ type: 'notification.dismiss', id });
      continue;
    }
    entry.output = host.name;
    host.notificationsEl.append(entry.el);
  }
  reportNotificationRect();
}

/* Where the notifications are, so the compositor can draw them above the
   windows. Nothing when a container is empty: the element is still there, and
   a rectangle of empty shell drawn over a window would be a hole in it.
   Reported per output, because a notification now lives in the corner of the
   output it came from — a single rectangle would put every one of them over
   one screen.
   One that is animating away still counts as being there: it is still a child
   of the container until removal, which is what keeps the strip composited
   for the length of its own exit. */
function reportNotificationRect() {
  for (const [name, output] of outputs) {
    const el = output.notificationsEl;
    setOverlay(`notifications:${name}`, el.children.length > 0 ? el : null);
  }
}

/* The first rule matching a window, or null.
 *
 * app_id is the identity worth matching: it is what the application calls
 * itself rather than what it happens to be showing. Title is offered as well
 * because some applications — a browser, a terminal multiplexer — give every
 * window the same app_id and differ only in what they display. Both are
 * substring matches, since an exact one would need the user to know the
 * application's internal name exactly. */
function ruleValueMatches(value, condition) {
  const text = String(value || '');
  if (typeof condition === 'string') {
    return text.toLowerCase().includes(condition.toLowerCase());
  }
  if (!condition || typeof condition !== 'object') return false;
  if (condition.contains !== undefined) {
    return text.toLowerCase().includes(String(condition.contains).toLowerCase());
  }
  if (condition.equals !== undefined) {
    return text.toLowerCase() === String(condition.equals).toLowerCase();
  }
  if (condition.regex !== undefined) {
    try {
      return new RegExp(String(condition.regex), condition.flags ?? '').test(text);
    } catch (_) {
      return false;
    }
  }
  return false;
}

function ruleFor(appId, title, tag, openingWorkspace = null) {
  const haystackApp = (appId || '').toLowerCase();
  const haystackTitle = (title || '').toLowerCase();

  return windowRules.find((rule) => {
    if (!rule || typeof rule !== 'object') return false;
    if (rule.match && typeof rule.match === 'object') {
      const fields = [['app_id', appId], ['title', title], ['tag', tag]];
      let matched = false;
      for (const [field, value] of fields) {
        if (rule.match[field] === undefined) continue;
        matched = true;
        if (!ruleValueMatches(value, rule.match[field])) return false;
      }
      if (rule.match.workspace !== undefined) {
        matched = true;
        if (!Number.isInteger(rule.match.workspace)
            || rule.match.workspace < 1 || rule.match.workspace > WORKSPACES
            || rule.match.workspace !== openingWorkspace) return false;
      }
      return matched;
    }
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
function claimFloatSlot(id, app, tag = null) {
  if (floatSlots.length === 0 || !app) return null;

  const at = floatSlots.findIndex((slot) => slot.tag ? slot.tag === tag : slot.app === app);
  if (at < 0) return null;

  const [slot] = floatSlots.splice(at, 1);
  slot.pseudotile = safePseudoDimensions(slot.pseudotile);
  return slot;
}

/* Move one window to a workspace, without it having to be focused first. The
 * overview can act on any window on screen, not just the current one. */
function moveViewToWorkspace(id, n) {
  if (n < 1 || n > WORKSPACES) return false;
  dissolveSwallow(id);
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
  if (from !== null && maximized.get(from) === id) {
    maximized.delete(from);
    maximized.set(n, id);
  }

  /* And on the canvas, where the window has a place, that place has to be
     rewritten for the plane it is arriving on: the two planes share no origin,
     so the same numbers name a different spot. See canvasCarry — it is a no-op
     in every other layout. */
  canvasCarry(id, from, n);

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
 * three-finger swipe.
 *
 * Named apart from outputs.js's stepWorkspace(name, delta): the shell is a set
 * of classic scripts sharing one scope, so two functions of the same name are
 * one function — whichever loaded last — and the caller of the other one is
 * then passing arguments nobody reads. */
function stepWorkspaceOnActive(delta) {
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

/* Step onto the monitor in this direction, if that is allowed.
 *
 * Every way out of scrollFocus goes through here rather than calling
 * focusOutputDirection itself, so the setting cannot be honoured at one edge
 * of the strip and forgotten at another. An explicit `output.focus` binding
 * still crosses: asking for the next monitor by name is not the same as
 * falling off the end of this one, and only the second is a surprise. */
function crossToOutput(direction) {
  if (!focusCrossesOutputs) return;
  /* Only the four that name an edge. `next` and `prev` step through the strip
     and wrap at its ends, so falling off one is not a direction to carry onto
     another monitor — and `adjacentOutput('next')` would be a search for a
     screen in a direction that does not exist. */
  if (!['left', 'right', 'up', 'down'].includes(direction)) return;
  focusOutputDirection(direction);
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
    crossToOutput(direction);
    return;
  }

  const root = workspaceRoot(workspace);
  const columns = root.children;
  if (columns.length === 0) {
    crossToOutput(direction);
    return;
  }

  const firstOf = (column) =>
    column.type === 'leaf' ? column.id : [...walk(column)][0][0].id;

  /* Step through every window on the strip, in strip order, wrapping at the
     ends. Mod4+Tab, which the compositor answers itself where every window is
     drawn — and cannot here, for the reason this function exists: a column
     scrolled off the screen is reported as not on it, and the compositor's own
     cycle walks what is on screen. Tabbing could therefore reach the columns
     either side of the view and nothing beyond them. */
  if (direction === 'next' || direction === 'prev') {
    const leaves = columns.flatMap((column) => column.type === 'leaf'
      ? [column.id] : [...walk(column)].map(([leaf]) => leaf.id));
    if (leaves.length === 0) return;
    const at = focusedId != null ? leaves.indexOf(focusedId) : -1;
    const step = direction === 'next' ? 1 : -1;
    const to = at < 0
      ? (direction === 'next' ? 0 : leaves.length - 1)
      : (at + step + leaves.length) % leaves.length;
    send({ type: 'view.focus', id: leaves[to] });
    return;
  }

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
      crossToOutput(direction);
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
    crossToOutput(direction);
    return;
  }
  send({ type: 'view.focus', id: leaves[next].id });
}

/* Move the focused window along the strip, or within its column. Moving left or
 * right carries the whole window into the neighbouring position as its own
 * column, which is what niri does. */
function scrollMove(direction, targetId = focusedId) {
  if (targetId == null) return false;
  dissolveSwallow(targetId);
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
  for (const id of selectedIds) dissolveSwallow(id);
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
  dissolveSwallow(focusedId);
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
  dissolveSwallow(focusedId);
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
