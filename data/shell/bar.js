/* SPDX-License-Identifier: MIT
 *
 * The status bar.
 *
 * Split in two on purpose. The chrome — workspace buttons and the taskbar —
 * is rebuilt only when the window list changes; the modules are strings
 * reassigned only when they differ. A status sample arrives every two seconds
 * and every shell repaint is a composited frame, so redrawing a CPU percentage
 * must not cost the whole desktop a frame.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */
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

/* The bar is drawn in two halves, because they change at wildly different
 * rates and cost wildly different amounts.
 *
 * The chrome — workspace buttons and taskbar — is updated in place by
 * syncButtons() below. It only ever changes when the window list, the
 * workspace set or the focus moves, all of which already go through a
 * relayout.
 *
 * The modules are a mode label and six status strings, redrawn whenever the
 * compositor publishes a sample — every two seconds, awake or idle. Redrawing
 * the chrome to keep them current was the same waste 95f625c took off the
 * clock tick: every shell repaint is a composited frame, so an idle machine
 * was painting the whole desktop every two seconds to move a CPU percentage.
 * status.update calls renderBarModules() alone, and it assigns only what
 * actually differs, so the common tick touches no DOM at all. */
function renderBar(name) {
  renderBarChrome(name);
  renderBarModules(name);
}

/* A row of buttons, kept and updated rather than rebuilt.
 *
 * This used to be replaceChildren() and a fresh element per entry, which cost
 * an allocation and a rebound listener per workspace and per window on every
 * relayout — a divider drag runs one of those per mousemove. It also made the
 * bar unanimatable, which is the reason this changed: a CSS transition needs a
 * previous computed style to move from, and an element created this frame has
 * none. The pill under the active workspace could only ever jump. Keeping the
 * element is what lets shell.css carry the colour across.
 *
 * Reuse is positional, so a listener cannot close over which workspace or
 * window it was made for. It reads that back off the element instead, which is
 * also what makes one listener per button enough for the life of the bar. */
function syncButtons(container, items, activate) {
  const existing = [...container.children];

  for (let i = 0; i < items.length; i++) {
    let button = existing[i];
    if (button === undefined) {
      button = document.createElement('button');
      button.addEventListener('click', () => activate(button.dataset.key));
      container.append(button);
    }
    const item = items[i];
    /* Guarded exactly as renderBarModules() guards its strings: an assignment
       dirties the element whether or not the value is new, and a dirty element
       is a repaint of a bar that mostly has nothing to say. */
    if (button.dataset.key !== item.key) button.dataset.key = item.key;
    if (button.textContent !== item.text) button.textContent = item.text;
    if (button.className !== item.className) button.className = item.className;
  }

  /* Whatever the last render needed and this one does not. */
  for (let i = items.length; i < existing.length; i++) existing[i].remove();
}

function renderBarChrome(name) {
  const output = outputs.get(name);
  if (!output) return;

  /* Every workspace that exists anywhere, since they are global. */
  const occupied = new Set([output.workspace]);
  for (const n of workspaces.keys()) {
    if (leavesOf(n).length > 0) occupied.add(n);
  }
  /* A workspace holding only floating windows is still occupied. */
  for (const [, floating] of floatingEntries()) occupied.add(floating.workspace);

  /* Built by joining an array rather than by concatenating, so the string is
     the same one every render for an unchanged button and the guard above can
     see that it is. */
  syncButtons(output.workspacesEl,
    [...occupied].sort((a, b) => a - b).map((n) => {
      const host = hostOfWorkspace(n);
      const classes = [];
      if (n === output.workspace) classes.push('active');
      if (host !== null && host !== name) classes.push('elsewhere');
      return { key: String(n), text: String(n), className: classes.join(' ') };
    }),
    (key) => switchWorkspace(name, Number(key)));

  syncButtons(output.taskbarEl,
    idsOf(output.workspace).filter((id) => views.has(id)).map((id) => {
      const view = views.get(id);
      const classes = [];
      if (id === focusedId) classes.push('focused');
      if (isFloating(id)) classes.push('floating');
      return {
        key: String(id),
        text: view.title || view.app_id || `view ${id}`,
        className: classes.join(' '),
      };
    }),
    (key) => send({ type: 'view.focus', id: Number(key) }));
}

/* Every write here is guarded, exactly as renderClocks() guards the clock: an
 * assignment to textContent dirties the element whether or not the string is
 * new, and a dirty element is a repaint. A bar nobody can see is skipped
 * outright — it will be redrawn by the relayout that reveals it. */
function renderBarModules(name) {
  const output = outputs.get(name);
  if (!output) return;
  if (output.el.classList.contains('bar-hidden')) return;

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

  const modeText = labels.join(' · ');
  if (output.modeEl.textContent !== modeText) {
    output.modeEl.textContent = modeText;
  }
  if (output.modeEl.hidden !== (labels.length === 0)) {
    output.modeEl.hidden = labels.length === 0;
    /* Only on the way in, and only on the edge. This function runs on every
       status sample — every two seconds, awake or idle — and a badge that
       re-popped each time would be a piece of the desktop animating on its own
       for as long as resize mode was held. */
    if (!output.modeEl.hidden) animateModeIn(output.modeEl);
  }

  const s = lastStatus;
  const m = output.modules;
  setModule(m.clock, clockText());
  setModule(m.cpu, s.cpu >= 0 ? ` ${Math.round(s.cpu)}%` : '');
  setModule(m.memory, s.memory >= 0 ? `󰍛 ${Math.round(s.memory)}%` : '');
  setModule(m.load, s.load !== undefined ? `󰓅 ${s.load.toFixed(2)}` : '');
  setModule(m.disk, s.disk_free ? `󰋊 ${formatBytes(s.disk_free)}` : '');
  setModule(m.net, s.net_rx !== undefined
    ? `󰇚 ${formatBytes(s.net_rx)}/s 󰕒 ${formatBytes(s.net_tx)}/s` : '');
}

function setModule(el, text) {
  if (el.textContent !== text) el.textContent = text;
}

function renderBars() {
  for (const name of outputs.keys()) renderBar(name);
}

function renderBarsModules() {
  for (const name of outputs.keys()) renderBarModules(name);
}

function clockText() {
  const now = new Date();
  const date = now.toLocaleDateString('en-US',
    { weekday: 'short', month: 'short', day: '2-digit' });
  const time = `${String(now.getHours()).padStart(2, '0')}:` +
    `${String(now.getMinutes()).padStart(2, '0')}`;
  return `󰥔 ${date}, ${time}`;
}

/* The tick redraws the clock and nothing else.
 *
 * It used to call renderBars(), which is not cheap: the chrome half rebuilds
 * the workspace buttons and the taskbar with replaceChildren(), allocating
 * every element and rebinding every click listener. Once a second, on every
 * output, whether or not the bar was even on screen — and since every shell
 * repaint is a composited frame, an idle machine with the bar hidden was still
 * painting the desktop 86,400 times a day to redraw a string that changes
 * hourly.
 *
 * A hidden bar is skipped outright, and the text is only assigned when it
 * differs, so the common tick touches no DOM at all. The clock is sampled at a
 * second's granularity rather than a minute's so it does not lag visibly after
 * a resume; nothing else here needs the tick, because everything else is
 * redrawn by whatever changed it. */
function renderClocks() {
  const text = clockText();
  for (const output of outputs.values()) {
    if (output.el.classList.contains('bar-hidden')) continue;
    if (output.modules.clock.textContent !== text) {
      output.modules.clock.textContent = text;
    }
  }
}

setInterval(renderClocks, 1000);

