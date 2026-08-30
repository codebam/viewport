/* SPDX-License-Identifier: MIT
 *
 * Outputs and workspaces, and moving between them.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */
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
  const configured = [...workspaceRules.entries()]
    .find(([, rule]) => rule.output === name)?.[0];
  const preferred = configured ?? OUTPUT_WORKSPACE[name];
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
  physicalOutputs.clear();
  for (const info of list) physicalOutputs.set(info.name, { name: info.name, info });
  list = list.filter((info) => info.enabled !== false && info.role !== 'mirror-sink');
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
        /* Notifications stack in this output's own corner — the shell draws
           each one over the output of the window it came from, not over some
           global one. */
        notificationsEl: el.querySelector('.notifications'),
        barEl: el.querySelector('.bar'),
        emptyEl: el.querySelector('.empty'),
        workspacesEl: el.querySelector('.workspaces'),
        taskbarEl: el.querySelector('.taskbar'),
        modeEl: el.querySelector('.mode'),
        modules: {
          tray: el.querySelector('.tray'),
          clock: el.querySelector('.clock'),
          cpu: el.querySelector('.cpu'),
          memory: el.querySelector('.memory'),
          load: el.querySelector('.load'),
          disk: el.querySelector('.disk'),
          net: el.querySelector('.net'),
        },
        barHidden: barMode !== 'visible',
        /* Whether the bar was on screen last time the layout was applied, so
           that revealing it can be told from leaving it revealed. Only the
           entrance animation reads it; see the note in geometry.js. */
        barShown: false,
        workspace: 0,
      };
      el.dataset.output = info.name;
      /* The network module answers a click by opening the network picker, the
         way the battery widget opens the profile list. Wired here because this
         is the shipped markup's own element — a `bar_items` override builds its
         own and wires them through `wireWidget`, which does the same thing.
         The click is stopped so that the document listener that closes every
         picker does not close the one it just opened. */
      if (output.modules.net) {
        output.modules.net.addEventListener('click', (e) => {
          e.stopPropagation?.();
          toggleNetworkPicker();
        });
      }
      /* And the clock answers a click by opening the calendar under itself,
         wired here for the same reason and on the same terms — a `bar_items`
         override builds its own clock and wires it through `wireWidget`. The
         element goes with the call so the panel hangs off this output's clock:
         the shell is one page across every monitor, and "the clock" is not a
         single thing on a desk with two of them. */
      if (output.modules.clock) {
        output.modules.clock.title = 'calendar';
        output.modules.clock.addEventListener('click', (e) => {
          e.stopPropagation?.();
          toggleCalendar(output.modules.clock);
        });
      }
      el.addEventListener('mouseenter', () => setActiveOutput(info.name));
      outputsEl.append(el);
      outputs.set(info.name, output);
      output.workspace = startingWorkspace(info.name);
      if (activeOutput === null) activeOutput = info.name;
    }

    /* The whole of what the compositor said about this monitor, kept as it
       arrived. The fields below are the ones the desktop lays out from; this
       is for the settings panel, which needs the mode list, the scale, the
       rotation and the model name — none of which the layout has any use for,
       and all of which would otherwise be dropped on the floor between the
       message arriving and the desktop being drawn. */
    output.info = info;

    /* Kept as numbers as well as CSS: anything that has to report a position
       back to the compositor needs the layout coordinates, and reading them
       back out of a style string is a parser nobody needs. */
    output.rect = {
      x: info.x, y: info.y, width: info.width, height: info.height,
    };

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
    /* Before the rectangles are dropped, because moving a notification to a
       screen that is still there is what reports the new one. */
    rehomeNotifications(name);
    /* And whatever this monitor had floating goes with it. Nothing else would
       ever clear it: the reports are per output and this output is gone. */
    dropOverlaysForOutput(name);
    if (activeOutput === name) activeOutput = null;
  }

  const fallback = firstOutputName();
  for (const [, floating, view] of specialEntries()) {
    if (outputs.has(view.specialOutput)) continue;
    view.specialOutput = fallback;
    const output = outputs.get(fallback);
    if (output) floating.workspace = output.workspace;
    if (view.special === 'scratchpad') view.specialHidden = true;
  }

  relayoutAll();
}

/* A workspace lives on exactly one output at a time. Asking for one already
 * elsewhere moves focus there rather than creating a second copy — otherwise
 * each monitor grows its own "workspace 1". */
function switchWorkspace(name, n) {
  let output = outputs.get(name);
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
  const preferred = workspaceRule(n).output;
  if (host === null && preferred !== undefined && outputs.has(preferred)) {
    name = preferred;
    output = outputs.get(name);
  }
  if (output.workspace === n) return;

  output.previous = output.workspace;
  output.workspace = n;
  workspaceHomes.set(n, name);
  setActiveOutput(name);

  /* What was on screen before the switch, so the windows that arrive can be
     faded in and the ones this workspace shares with the last are left alone. */
  const before = new Set(renderedIds);
  relayoutAll();
  fadeInArrivals(before);

  focusFirstOn(name);
  saveSession();
}

/* Straight to the previous workspace, whatever it was. The same thing the
 * repeated switch does, for a binding of its own. */
function workspaceBack(name) {
  const output = outputs.get(name);
  if (!output || output.previous == null) return;
  switchWorkspace(name, output.previous);
}

/* One workspace on from the current (`delta` 1) or back (`delta` -1), wrapping
 * within 1..WORKSPACES. The thumb-button gesture: forward to the next
 * workspace, back to the previous — browser back/forward, but for desktops. */
function stepWorkspace(name, delta) {
  const output = outputs.get(name);
  if (!output) return;
  const n = ((output.workspace - 1 + delta) % WORKSPACES + WORKSPACES)
    % WORKSPACES + 1;
  switchWorkspace(name, n);
}

function focusFirstOn(name) {
  const output = outputs.get(name);
  if (!output) return;
  const ids = idsOf(output.workspace);
  send(ids.length > 0
    ? { type: 'view.focus', id: ids[0] }
    : { type: 'shell.focus' });
}

/* ------------------------------------------------------------------------
 * Workspaces, as everything outside the shell sees them
 * --------------------------------------------------------------------- */

/* The workspaces are the shell's: nothing else knows how many there are, which
 * is showing or what is on them. `ext-workspace-v1` is a client — waybar's
 * `ext/workspaces` module, an overview panel, anything — asking to be told, and
 * the compositor can only relay what arrives here as `workspace.list`. A shell
 * that never sends it publishes an empty world, which is what a bar with no
 * workspace buttons on it looks like.
 *
 * Which workspaces are in the list is the same question the bar's own buttons
 * answer: the ones on screen, plus any holding a window. An empty workspace
 * nobody is showing does not exist yet in any sense a bar could draw.
 *
 * The whole list every time, per the message: the shell already has it, and
 * reconciling two halves of one is how they drift apart. */
function workspaceList() {
  const shown = new Map(); // workspace -> output showing it
  for (const [name, output] of outputs) {
    shown.set(output.workspace, name);
    workspaceHomes.set(output.workspace, name);
  }

  const known = new Set(shown.keys());
  for (const n of workspaces.keys()) {
    if (leavesOf(n).length > 0) known.add(n);
  }
  /* A workspace holding only floating windows is still occupied. */
  for (const [, floating] of floatingEntries()) known.add(floating.workspace);

  return [...known].sort((a, b) => a - b).map((n) => {
    const workspace = { id: String(n), name: String(n) };
    /* The group it goes in. Where it is now if it is anywhere, and where it
       was last if it is not; a home on a monitor that has since been unplugged
       is no home at all, so it goes out ungrouped rather than into a group for
       a screen that is gone. */
    const home = shown.get(n) ?? workspaceHomes.get(n);
    if (home !== undefined && outputs.has(home)) workspace.output = home;
    if (shown.has(n)) workspace.active = true;
    /* The protocol's third state: it exists, it is not showing, and nothing on
       it is asking for attention. */
    else workspace.hidden = true;
    return workspace;
  });
}

/* What was last published, so an unchanged list costs nothing. This runs from
 * relayoutAll(), which a divider drag runs once per mousemove — and a workspace
 * list on the wire per mousemove is a bar redrawn sixty times a second to say
 * the same thing. */
let lastWorkspaceList = null;

function publishWorkspaces() {
  const list = workspaceList();
  const wire = JSON.stringify(list);
  if (wire === lastWorkspaceList) return;
  lastWorkspaceList = wire;
  send({ type: 'workspace.list', workspaces: list });
}

/* A client outside the shell asked for something, through `ext-workspace-v1`.
 *
 * Requests are of the shell, not of the compositor: nothing has happened yet
 * when this arrives, and whatever is decided here shows up in the next
 * `workspace.list` — which is what the asking bar redraws from.
 *
 * `activate` and `assign` are the two that mean something to this shell.
 * Deactivating is not a state it has — a monitor is always showing some
 * workspace — and creating or removing one is not either: there are WORKSPACES
 * of them, always, and a workspace comes into existence by having a window put
 * on it. Those are declined by doing nothing rather than by pretending. */
function workspaceRequested(message) {
  const n = Number(message.id);
  const wanted = Number.isInteger(n) && n >= 1 && n <= WORKSPACES;

  switch (message.action) {
    case 'activate': {
      if (!wanted) return;
      /* A workspace some monitor is already showing is activated by going to
         that monitor. Not through switchWorkspace(): asking it for the
         workspace an output is already on is the back-and-forth gesture, and a
         bar's second click on the workspace it is highlighting would take you
         somewhere else entirely. */
      const host = hostOfWorkspace(n);
      if (host !== null) {
        setActiveOutput(host);
        focusFirstOn(host);
        return;
      }
      /* Otherwise it arrives on whichever monitor is active, which is what
         someone clicking a bar button meant by it. */
      switchWorkspace(activeOutputName(), n);
      break;
    }

    /* "Put this workspace on that screen", which for this shell is the same
       act as showing it there. */
    case 'assign':
      if (!wanted || !outputs.has(message.output)) return;
      if (outputs.get(message.output).workspace === n) return;
      switchWorkspace(message.output, n);
      break;

    default:
      console.warn('workspace request this shell has no answer for:',
        message.action);
  }
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

/* Move to another monitor, by direction or by name.
 *
 * A direction is what a keybinding sends, and it is the right thing for a key:
 * "the screen to my left" is what someone means when they press it, and it
 * follows the monitors being rearranged without anything being rebound.
 *
 * A name is for everything that is not a person. Anything driving the shell
 * over IPC knows which monitor it wants and has no way to work out the
 * direction to it — the layout rects are here, not there — and a benchmark
 * that has to guess `right` and hope the monitors are side by side is one that
 * silently measures the wrong screen when they are stacked. */
function focusOutputDirection(direction) {
  const best = outputs.has(direction) ? direction : adjacentOutput(direction);
  if (best === null) return;

  setActiveOutput(best);
  /* Say which output this is, even when nothing moved.
   *
   * setActiveOutput stays quiet about a move to the output that is already
   * active, which is right for the case it was written for — it runs on every
   * pointer crossing — but it leaves anything driving this over IPC unable to
   * tell "already there" from "ignored". The compositor's own record of the
   * active output starts empty and is written by nothing but this message, so
   * a caller asking for the output the shell happens to have started on waits
   * for a confirmation that is never coming. That is one message per explicit
   * focus, which is not a rate anything cares about. */
  send({ type: 'output.active', name: best });
  focusFirstOn(best);
}

/* Carry the focused window to the monitor in a direction, onto whatever
 * workspace that monitor is showing. Used when the window is already at the
 * edge of its own workspace's tree — sway's behaviour. */
function moveViewToOutput(id, direction) {
  dissolveSwallow(id);
  const target = adjacentOutput(direction);
  if (target === null) return false;

  const output = outputs.get(target);
  if (!output) return false;

  /* Before the window leaves the tree it is in, because that is what says
     which workspace it is on. */
  const from = workspaceOf(id);

  removeLeaf(id);
  const leaf = newLeaf(id);
  workspaceRoot(output.workspace).children.push(leaf);

  /* Fullscreen is recorded per workspace, and carrying a window to the next
     monitor carries it onto that monitor's workspace — so the record has to
     travel, exactly as it does in moveViewToWorkspace(). Left behind, the
     workspace it came from goes on hiding its bar and drawing its layout
     around a window that is on another screen. */
  if (from !== null && fullscreens.get(from) === id) {
    fullscreens.delete(from);
    fullscreens.set(output.workspace, id);
  }
  if (from !== null && maximized.get(from) === id) {
    maximized.delete(from);
    maximized.set(output.workspace, id);
  }

  treeGeneration++;

  setActiveOutput(target);
  relayoutAll();
  /* Focus follows the window, as it does when moving within a workspace. */
  send({ type: 'view.focus', id });
  return true;
}

/* Mod4+Shift+N. Defers to moveViewToWorkspace() rather than moving the leaf
 * itself.
 *
 * It used to find the window with findLeaf(), which only walks the tiling tree
 * — so for a floating window it found nothing and returned, and the binding
 * did nothing at all. Dragging the same window between thumbnails in the
 * overview worked, because that path already called moveViewToWorkspace().
 * Two ways to move a window to a workspace, one of which handled half the
 * windows. Now there is one. */
function moveToWorkspace(n) {
  if (selectedIds.size > 0) {
    const ids = Array.from(selectedIds);
    let moved = false;
    for (const id of ids) {
      if (moveViewToWorkspace(id, n)) moved = true;
    }
    if (moved) relayoutAll();
  } else if (focusedId != null) {
    if (moveViewToWorkspace(focusedId, n)) {
      relayoutAll();
    }
  }
}
