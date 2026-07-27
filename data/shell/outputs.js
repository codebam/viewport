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
        barHidden: barMode !== 'visible',
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
  if (focusedId == null) return;
  if (moveViewToWorkspace(focusedId, n)) {
    relayoutAll();
  }
}

