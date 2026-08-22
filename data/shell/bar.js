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

  syncBarRight(output);
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
   * at once, so both are shown. A bar_items override may have dropped the mode
   * badge; only draw it if an element is actually standing there. */
  const labels = [];
  if (output.hdr) labels.push('HDR');
  if (currentMode !== 'default') labels.push(currentMode.toUpperCase());

  if (output.modeEl) {
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
  /* The throughput stays the number on the bar — it is what a status bar's
     network module has always been — and which network it is goes in the
     tooltip. Two figures and a name would not fit, and the name is the part
     somebody looks up rather than watches. Only once the picker has been
     opened at least once: nothing talks to NetworkManager before that, so
     until then there is nothing truthful to say. */
  setModuleTitle(m.net, networkTitle());
  syncTray(output);
  renderBarWidgets(output);
}

function setModule(el, text) {
  if (el && el.textContent !== text) el.textContent = text;
}

/* Guarded like setModule and for the same reason: an assignment dirties the
 * element whether or not the string is new, and this runs on every status
 * sample. */
function setModuleTitle(el, title) {
  if (el && el.title !== title) el.title = title;
}

/* What the network module's tooltip says. The wired case matters: a desktop on
 * a cable is online with no network joined, and a tooltip that said "not
 * connected" would be wrong in the way that sends somebody looking for a
 * fault. */
function networkTitle() {
  if (!networkState?.available) return 'network';
  if (networkState.ssid) return `network — ${networkState.ssid}`;
  if (networkState.wired) return 'network — wired';
  return 'network — not connected';
}

/* ------------------------------------------------------------------------
 * The system tray
 * --------------------------------------------------------------------- */

/* The tray, replaced whole whenever the compositor sends one.
 *
 * The compositor holds the StatusNotifierWatcher name and does every D-Bus
 * call an item answers; what arrives here is a picture, a label and a key to
 * send back. Nothing in this file knows an item's bus name, and that is the
 * point — the shell cannot address an application, only name which icon was
 * clicked. */
function applyTray(items) {
  trayItems = Array.isArray(items) ? items : [];
  for (const [, output] of outputs) syncTray(output);
}

/* Where the tray sits on one output's bar, whichever shape built the bar. The
 * override path puts the element in `modules`, the shipped markup has it in
 * the document; both are the same element to this. */
function trayElement(output) {
  return output.modules?.tray ?? output.barEl?.querySelector('.tray') ?? null;
}

/* Kept and updated by position, like the workspace buttons and for the same
 * reason: a status sample arrives every two seconds and rebuilding the tray on
 * each one would be an allocation and a rebound listener per icon, on a bar
 * that mostly has nothing new to say. The listeners are bound once and read
 * the item back off the element, so they survive an icon changing places. */
function syncTray(output) {
  const container = trayElement(output);
  if (!container) return;

  const existing = [...container.children];
  for (let i = 0; i < trayItems.length; i++) {
    let el = existing[i];
    if (el === undefined) {
      el = document.createElement('button');
      el.className = 'tray-item';
      wireTrayItem(el);
      container.append(el);
    }
    const item = trayItems[i];
    el._tray = item;

    /* The image element exists only while there is an icon to put in it. An
       <img> with no source is a broken image in some engines and a request for
       the page's own URL in others, and neither is a thing to leave on the
       bar. */
    if (item.icon) {
      let img = el._img;
      if (img === undefined) {
        img = el._img = document.createElement('img');
        el.append(img);
      }
      /* Guarded like every other write in this file: assigning a src the
         element already has is a fetch, a decode and a repaint of the whole
         desktop. */
      if (img.src !== item.icon) img.src = item.icon;
    } else if (el._img) {
      el._img.remove();
      el._img = undefined;
    }
    /* An item with no icon draws its own first letter, so a program that
       publishes nothing this shell can show is still something to click. */
    const fallback = item.icon ? '' : (item.title || '?').slice(0, 1).toUpperCase();
    if (el.dataset.fallback !== fallback) el.dataset.fallback = fallback;

    const title = item.tooltip || item.title || '';
    if (el.title !== title) el.title = title;
    const className = `tray-item ${item.status || 'active'}`;
    if (el.className !== className) el.className = className;
  }

  for (let i = trayItems.length; i < existing.length; i++) existing[i].remove();
}

/* ------------------------------------------------------------------------
 * A tray item's menu
 * --------------------------------------------------------------------- */

/* The menu the compositor fetched, drawn where the icon is.
 *
 * Every row here came from the application, through `com.canonical.dbusmenu`
 * and the compositor: the shell decides none of it and invents nothing. What
 * it does decide is the shape — submenus open in place rather than flying out
 * beside the menu, because everything the shell draws over a window is one
 * rectangle it has to name to the compositor, and a submenu hanging outside
 * that rectangle would be drawn behind the window it is meant to be over. An
 * accordion has one rectangle by construction.
 */
function showTrayMenu(message) {
  trayMenuOpen = message.id;
  trayMenuEl.replaceChildren();
  trayMenuEl.hidden = false;

  const list = document.createElement('div');
  list.className = 'tray-menu-list';
  buildTrayMenuRows(list, message.items ?? [], 0);
  trayMenuEl.append(list);

  /* Under the icon that was clicked, and inside the screen: a menu opened by
     the rightmost icon on the bar would otherwise run off the edge, and the
     compositor draws exactly the rectangle it is given — including the part
     that is not on any monitor. */
  const width = 240;
  const bounds = outputBoundsAt(message.x, message.y);
  const left = Math.max(bounds.left, Math.min(message.x, bounds.right - width));
  trayMenuEl.style.left = `${Math.round(left)}px`;
  trayMenuEl.style.top = `${Math.round(message.y)}px`;
  trayMenuEl.style.width = `${width}px`;
  trayMenuEl.style.maxHeight = `${Math.max(120, bounds.bottom - message.y - 8)}px`;

  setOverlay('tray-menu', trayMenuEl);
}

/* The output the click landed on, in layout coordinates, so a menu can be
 * kept on the monitor it was opened from. Falls back to the whole page, which
 * is what a click at a position no output claims would have got anyway. */
function outputBoundsAt(x, y) {
  for (const [, output] of outputs) {
    const rect = output.el?.getBoundingClientRect?.();
    if (!rect) continue;
    const right = rect.left + rect.width;
    const bottom = rect.top + rect.height;
    if (x >= rect.left && x < right && y >= rect.top && y < bottom) {
      return { left: rect.left, top: rect.top, right, bottom };
    }
  }
  return { left: 0, top: 0, right: 1e6, bottom: 1e6 };
}

/* One level of rows, and everything under it, indented.
 *
 * A row that has children is a toggle rather than a command: dbusmenu says so
 * by giving it children, and clicking one sends nothing to the application. */
function buildTrayMenuRows(list, items, depth) {
  for (const item of items) {
    if (item.kind === 'separator') {
      const line = document.createElement('div');
      line.className = 'tray-menu-separator';
      list.append(line);
      continue;
    }

    const row = document.createElement('button');
    row.className = 'tray-menu-row';
    if (!item.enabled) row.classList.add('disabled');
    if (item.checked) row.classList.add('checked');
    if (item.children?.length) row.classList.add('parent');
    if (depth > 0) row.style.paddingLeft = `${8 + depth * 14}px`;

    if (item.icon) {
      const img = document.createElement('img');
      img.src = item.icon;
      row.append(img);
    }

    const label = document.createElement('span');
    label.className = 'tray-menu-label';
    /* A ticked row says so with a mark rather than with colour alone, which
       is the one part of a menu a stylesheet cannot supply: the state is the
       application's and has to be visible without one. */
    const mark = item.toggle === 'radio' ? '◉' : '✓';
    label.textContent = item.checked ? `${mark} ${item.label}` : item.label;
    row.append(label);

    if (item.children?.length) {
      const chevron = document.createElement('span');
      chevron.className = 'tray-menu-chevron';
      chevron.textContent = '▸';
      row.append(chevron);
    }

    row.addEventListener('click', (e) => {
      /* The document closes the menu on any click that is not in it; this is
         in it. */
      e.stopPropagation?.();
      if (!item.enabled) return;
      if (item.children?.length) {
        /* Open in place. The rectangle the compositor was given grows with
           the menu, so it is re-reported after the rows appear. */
        const open = row.classList.contains('open');
        row.classList.toggle('open', !open);
        for (const child of [...list.children]) {
          if (child._parentRow === row) child.hidden = open;
        }
        setOverlay('tray-menu', trayMenuEl);
        return;
      }
      send({ type: 'tray.menu.click', id: trayMenuOpen, item: item.id });
      closeTrayMenu(false);
    });
    list.append(row);

    if (item.children?.length) {
      const before = list.children.length;
      buildTrayMenuRows(list, item.children, depth + 1);
      for (let i = before; i < list.children.length; i++) {
        const child = list.children[i];
        /* Which parent a row belongs to, so opening one shows exactly its own
           descendants. Written on the element rather than kept in a map: the
           menu is rebuilt from scratch every time it opens. */
        if (child._parentRow === undefined) child._parentRow = row;
        child.hidden = true;
      }
    }
  }
}

/* Take the menu down. `notify` is false when the application already knows —
 * it was told by the click that chose a row. */
function closeTrayMenu(notify = true) {
  if (trayMenuOpen === null) return;
  if (notify) send({ type: 'tray.menu.closed', id: trayMenuOpen });
  trayMenuOpen = null;
  trayMenuEl.replaceChildren();
  trayMenuEl.hidden = true;
  setOverlay('tray-menu', null);
}

/* An item's input, bound once at build.
 *
 * Where the pointer is matters: an application opening its own menu is handed
 * a position and puts its window there, so what is sent is the bottom left of
 * the icon — the menu then hangs off the icon rather than off the pointer,
 * which is where every other tray puts it. The coordinates are page
 * coordinates, and the page spans the whole output layout, so they are already
 * what the compositor means by a position. */
function wireTrayItem(el) {
  const at = (button) => {
    const item = el._tray;
    if (!item) return;
    const rect = el.getBoundingClientRect();
    send({
      type: 'tray.activate',
      id: item.id,
      button,
      x: Math.round(rect.left),
      /* The bottom edge, added rather than read: `bottom` is a live rect's
         own field and every measurement in this shell goes through top and
         height, which is what the geometry the compositor is sent is made
         of. */
      y: Math.round(rect.top + rect.height),
    });
  };

  /* Left click activates — which for an item that says it is a menu opens the
     menu instead, decided by the compositor because the property is the
     item's own. */
  el.addEventListener('click', () => at('primary'));
  /* Right click is the menu, as it is everywhere else on a desktop. The
     engine's own context menu is already suppressed for the shell; this
     preventDefault is for the backends where that is per element. */
  el.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    at('menu');
  });
  /* Middle click is the secondary action. Items that do not implement it
     answer an error the compositor logs and drops. */
  el.addEventListener('auxclick', (e) => {
    if (e.button === 1) at('secondary');
  });
  el.addEventListener('wheel', (e) => {
    const item = el._tray;
    if (!item) return;
    e.preventDefault();
    /* One step per notch, in whichever axis turned. A volume applet is the
       usual consumer and it wants the sign, not the pixel delta. */
    const horizontal = Math.abs(e.deltaX) > Math.abs(e.deltaY);
    send({
      type: 'tray.scroll',
      id: item.id,
      delta: (horizontal ? e.deltaX : e.deltaY) < 0 ? 1 : -1,
      orientation: horizontal ? 'horizontal' : 'vertical',
    });
  });
}

/* ------------------------------------------------------------------------
 * Extra widgets / bar override
 * --------------------------------------------------------------------- */

/* The extra widgets a config file asked for, beyond the bar's own modules.
 * Empty is the default bar, exactly as it shipped — nothing here changes what
 * the modules it was born with draw. */
let barWidgets = [];

/* A full override of the right side of the bar, as `bar_items`: a bare string
 * names a built-in module (`"net"`, `"clock"`, ...), an object names a widget.
 * Null is the default bar. When set, the shell draws exactly this list, in
 * order, and nothing else — which is how a widget sits anywhere the built-ins
 * can, not just after them. */
let barItems = null;

/* Called when the compositor sends them with the config. The elements are
 * built on the next chrome render, which every output hits when it is laid
 * out; the weather fetch is kicked off here because it owes nothing to any
 * output. */
function applyBarWidgets(widgets) {
  barWidgets = Array.isArray(widgets) ? widgets.slice() : [];
  renderBars();
  refreshWeather();
}

/* A full bar override, or the default bar when `items` is absent. Present but
 * empty draws nothing on the right — the user asked for no contents at all. */
function applyBarItems(items) {
  barItems = Array.isArray(items) ? items : null;
  renderBars();
  refreshWeather();
}

function widgetTitle(w) {
  switch (w.type) {
    case 'disk': return `free on ${w.path || '/'}`;
    case 'weather': return `weather for ${(w.location || '').trim()}`.trim();
    case 'volume': return 'volume';
    case 'mic': return 'microphone';
    /* The media widget writes its own, from what is playing. */
    case 'mpris': return '';
    case 'battery': return 'battery';
  }
  return '';
}

function moduleTitle(name) {
  switch (name) {
    case 'tray': return '';
    case 'net': return 'network';
    case 'disk': return 'free on /';
    case 'cpu': return 'cpu';
    case 'load': return 'load average';
    case 'memory': return 'memory';
    default: return '';
  }
}

/* The widget's element-bound input. The bar is the shell page, so a pointer
 * over a widget already reaches the DOM; these listeners turn that into shell
 * commands the compositor runs on the host. Which input means what is per
 * widget kind: a disk opens its mount, a weather opens the place, a volume
 * scrolls in 5% steps and a right click mutes. Each element is wired once,
 * at build — the widget it answers to is read back off the element so the
 * listener survives a positional rebuild, when the config changes which
 * widget sits where. */
function wireWidget(el) {
  const cmd = (line) => send({ type: 'shell.exec', command: line });
  /* Audio goes through the compositor rather than through `shell.exec`, and
     the reason is ordering rather than tidiness: a spawned `wpctl` and a
     `status.refresh` in the next message are a race the refresh usually wins,
     so the bar redraws the volume that was already there and the new one waits
     for the next two-second tick. `status.volume` changes and samples in that
     order, so it cannot. */
  const audio = (widget, body) => send({
    type: 'status.volume',
    target: widget.type === 'mic' ? 'source' : 'sink',
    ...body,
  });

  /* The element carries its own widget (`el._widget`), set by the sync pass;
     the handlers below are bound once and read it, so they always act on the
     widget the element currently stands for. */
  el.addEventListener('click', (e) => {
    const w = el._widget;
    if (!w) {
      /* A module rather than a widget. Only one of them answers a click: the
         network module opens the picker, the way the battery widget opens the
         profile list. Stopped here so the document's own listener — which
         closes every picker — does not close the one this just opened. */
      if (el._module === 'net') {
        e.stopPropagation?.();
        toggleNetworkPicker();
      }
      return;
    }
    if (w.type === 'mpris') {
      /* A click that missed the buttons — on the cover or the title — is the
         same as pressing play. */
      send({ type: 'mpris.control', action: 'play-pause' });
    } else if (w.type === 'battery') {
      /* Stopped for the same reason the network module above is: without it
         the document's listener — the one that closes every picker — would
         close the one this click just opened. */
      e.stopPropagation?.();
      togglePowerPicker();
    } else if (w.type === 'disk') {
      /* Open the mount in the default file manager — for this user that is
         a terminal at the directory. `xdg-open` respects the system default,
         whichever it is. */
      cmd(`xdg-open ${JSON.stringify(w.path || '/')}`);
    } else if (w.type === 'weather') {
      const loc = (w.location || '').trim();
      if (!loc) return;
      /* Open the place in a browser, pointed at where it is. */
      cmd(`xdg-open ${JSON.stringify(`https://www.google.com/maps/search/?api=1&query=${encodeURIComponent(loc)}`)}`);
    }
  });
  el.addEventListener('wheel', (e) => {
    const w = el._widget;
    if (w?.type === 'mpris') {
      e.preventDefault();
      /* Scrolling a media widget skips, which is what every bar that has one
         does: up for the track before, down for the one after. */
      send({ type: 'mpris.control', action: e.deltaY < 0 ? 'previous' : 'next' });
      return;
    }
    if (!w || (w.type !== 'volume' && w.type !== 'mic')) return;
    e.preventDefault();
    /* Scrolling is the natural volume gesture: up to raise, down to lower,
       in 5% steps. `deltaY < 0` is wheel-up on a normal wheel. A mic widget
       drives the microphone rather than the speakers. */
    audio(w, { delta: e.deltaY < 0 ? 5 : -5 });
  });
  el.addEventListener('contextmenu', (e) => {
    const w = el._widget;
    if (!w || (w.type !== 'volume' && w.type !== 'mic')) return;
    e.preventDefault();
    /* Right click toggles mute, matching the audio convention — the speakers
       for a volume widget, the microphone for a mic widget. */
    audio(w, { mute: true });
  });
}

/* The default shape: the shipped modules (from index.html) stay put and the
 * `bar_widgets` additions are appended after them. Widget elements keep and
 * update by position, like the chrome buttons, so a status sample every two
 * seconds never rebuilds them. */
function syncBarWidgets(output) {
  const container = output.barEl.querySelector('.bar-right');
  const els = output.widgetsEls ?? (output.widgetsEls = []);

  for (let i = 0; i < barWidgets.length; i++) {
    let el = els[i];
    if (el === undefined) {
      el = document.createElement('span');
      el.className = 'module widget';
      wireWidget(el);
      container.append(el);
      els[i] = el;
    }
    const w = barWidgets[i];
    el._widget = w;
    const key = `${w.type}:${w.path || w.location || ''}`;
    if (el.dataset.widget !== key) el.dataset.widget = key;
    const title = widgetTitle(w);
    if (el.title !== title) el.title = title;
  }
  /* Whatever the last config wanted and this one does not. */
  for (let i = barWidgets.length; i < els.length; i++) {
    if (els[i]) els[i].remove();
    els[i] = undefined;
  }
  output.widgetsEls = els.filter(Boolean);
}

/* The module name a right-side element answers to, or null if it is a widget.
 * Mode is declared a module so the override can place the badge anywhere;
 * the element class keeps the default `mode` (no `module` prefix) so the
 * stylesheet matches it as it always has. */
function rightElementClass(item) {
  if (typeof item === 'string') {
    return item === 'mode' ? 'mode' : `module ${item}`;
  }
  return 'module widget';
}

/* Build the bar's right side, kept and updated positionally like the chrome
 * buttons. Two shapes share this function:
 *
 *  - with `barItems` set, the whole right side is rebuilt from the list —
 *    modules and widgets interleaved in the order the config named them;
 *  - with it absent, the shipped modules (from index.html) stay and the
 *    `bar_widgets` additions are appended after them, as they always were.
 */
function syncBarRight(output) {
  const container = output.barEl.querySelector('.bar-right');

  /* No override: the shipped modules (from index.html) stay put and the
     bar_widgets additions are appended after them. If a previous config did
     override the bar, take those elements off first so the default shape is
     clean. */
  if (barItems === null) {
    if (output.barItemsEls) {
      for (const el of output.barItemsEls) el.remove();
      output.barItemsEls = undefined;
      /* The widget elements the override built went out with it, so the
         default path builds its own rather than reusing detached ones — and
         its widget defs go with them. */
      output.widgetsEls = undefined;
      output.barItemsWidgets = undefined;
      /* The shipped markup was detached rather than dropped when the first
         override was built (below), so put it back — a querySelector cannot
         find an element that is no longer in the document, and a bar that had
         once been overridden would have kept an empty right side and null
         module refs for the rest of the session. */
      if (output.barDefaultEls) {
        for (const el of output.barDefaultEls) container.append(el);
        output.barDefaultEls = undefined;
      }
      /* The default module refs come back from the markup; see outputs.js. */
      output.modules = {
        tray: container.querySelector('.tray'),
        clock: container.querySelector('.clock'),
        cpu: container.querySelector('.cpu'),
        memory: container.querySelector('.memory'),
        load: container.querySelector('.load'),
        disk: container.querySelector('.disk'),
        net: container.querySelector('.net'),
      };
      output.modeEl = container.querySelector('.mode');
    }
    syncBarWidgets(output);
    return;
  }

  /* First time an override is drawn: the shipped modules sit in the markup
     (index.html) and the override replaces all of them, so empty the right
     side before building. Only on the first build — after that the elements
     are ours and kept by position. Check BEFORE binding the fresh array
     below, or the guard would never see it was undefined. */
  const firstBuild = output.barItemsEls === undefined;
  const els = output.barItemsEls ?? (output.barItemsEls = []);
  const widgetDefs = [];

  if (firstBuild) {
    /* Detached, not dropped: the shipped modules are the only copy of the
       default bar this output has, and a config that later drops `bar_items`
       has to be able to put them back. The widgets the default path appended
       are not part of that markup, so they go for good and the override
       builds its own. */
    const widgets = new Set(output.widgetsEls || []);
    const children = [...container.children];
    if (output.barDefaultEls === undefined) {
      output.barDefaultEls = children.filter((el) => !widgets.has(el));
    }
    for (const child of children) child.remove();
    output.widgetsEls = undefined;
  }

  for (let i = 0; i < barItems.length; i++) {
    let el = els[i];
    if (el === undefined) {
      el = document.createElement('span');
      wireWidget(el);
      container.append(el);
      els[i] = el;
    }
    const item = barItems[i];
    const cls = rightElementClass(item);
    if (el.className !== cls) el.className = cls;

    if (typeof item === 'string') {
      el._widget = null;
      /* Which built-in module this element is standing for, read back by the
         click handler: the elements are kept by position and a config change
         can move a different module onto this one. */
      el._module = item;
      const title = moduleTitle(item);
      if (el.title !== title) el.title = title;
    } else {
      el._widget = item;
      el._module = null;
      const key = `${item.type}:${item.path || item.location || ''}`;
      if (el.dataset.widget !== key) el.dataset.widget = key;
      const title = widgetTitle(item);
      if (el.title !== title) el.title = title;
      /* Keep the widget's own definition, in order, so the render pass below
         and the weather fetch can reach it. */
      widgetDefs[widgetDefs.length] = item;
    }
  }

  /* Whatever the previous override asked for and this one does not. */
  for (let i = barItems.length; i < els.length; i++) {
    if (els[i]) els[i].remove();
    els[i] = undefined;
  }
  output.barItemsEls = els.filter(Boolean);

  /* Re-point the module refs the render pass reads, so the built-in modules
     drawn from the override land on their own elements. */
  const modules = {};
  for (let i = 0; i < barItems.length; i++) {
    const item = barItems[i];
    if (typeof item === 'string' && item !== 'mode') modules[item] = els[i];
  }
  output.modules = modules;
  output.modeEl = els[barItems.findIndex((it) => it === 'mode')] ?? null;

  /* Widgets render through the shared widget path regardless of which shape
     built them, but the override's list is kept on the output rather than
     written to the global: `bar_widgets` is a config of its own, applied
     before this one (commands.js), and overwriting it here left a config that
     dropped `bar_items` and added `bar_widgets` drawing the old override's
     widgets instead of the ones it asked for. */
  output.barItemsWidgets = widgetDefs;
  output.widgetsEls = els.filter((_, i) => barItems[i] !== undefined &&
    typeof barItems[i] !== 'string');
}

/* Each widget is a short string drawn from the status sample (disk, volume)
 * or the weather cache. Every write is guarded like the modules above: an
 * assignment to textContent dirties the element whether or not the string is
 * new, and a dirty element is a repaint. */
function renderBarWidgets(output) {
  const s = lastStatus;
  /* An overridden bar draws the widgets its own `bar_items` named; a default
     one draws the shared `bar_widgets`. Either way the list is in the same
     order the elements were built in. */
  const defs = output.barItemsWidgets || barWidgets;
  (output.widgetsEls || []).forEach((el, i) => {
    const w = defs[i];
    if (!w) return;
    let text = '';
    if (w.type === 'disk') {
      const path = w.path || '/';
      const mount = (s.mounts || []).find((m) => m.path === path);
      if (mount && mount.total > 0) text = `󰋊 ${path} ${formatBytes(mount.free)}`;
    } else if (w.type === 'volume' || w.type === 'mic') {
      /* The two audio widgets render the same way — a glyph and a percentage
         — but read different halves of the sample: volume reads the sink,
         mic reads the source. A muted node keeps its percentage (the two are
         independent), only the glyph changes. */
      const isMic = w.type === 'mic';
      const vol = isMic ? s.mic_volume : s.volume;
      const muted = isMic ? s.mic_muted : s.muted;
      if (vol >= 0) {
        const pct = Math.round(vol * 100);
        /* md-microphone/md-microphone_off (U+F036C/U+F036D) and
           md-volume_high/md-volume_off. The mic drew U+F02DB and U+F02DC,
           which are md-hololens and md-home — a headset and a house where a
           microphone belongs. */
        const on = isMic ? '󰍬' : '󰕾';
        const off = isMic ? '󰍭' : '󰝟';
        text = `${muted ? off : on} ${pct}%`;
      }
    } else if (w.type === 'weather') {
      text = weatherText(w.location || '');
    } else if (w.type === 'mpris') {
      /* The one widget that is not a string. It draws buttons, so it owns its
         own children and returns before the assignment below — which would
         throw them all away. */
      syncMprisWidget(el);
      return;
    } else if (w.type === 'battery') {
      text = batteryText();
    }
    if (el.textContent !== text) el.textContent = text;
  });
}

/* Charge and charging state, as UPower last reported them. Empty when there
 * is no battery — a desktop, or a laptop whose DisplayDevice has gone —
 * so the widget collapses through `.module:empty`. */
function batteryText() {
  const bat = powerState?.batteries?.[0];
  if (!bat || !(bat.percentage >= 0)) return '';
  const pct = Math.round(bat.percentage);
  /* md-battery / md-battery-charging / md-battery-alert. Alert is empty. */
  const glyph = bat.state === 'charging' ? '󰂄'
    : bat.state === 'empty' ? '󰂃'
    : '󰁹';
  return `${glyph} ${pct}%`;
}

/* The media widget: cover, what is playing, and the buttons the player says
 * it will honour.
 *
 * Buttons rather than a string, because a track is something you skip rather
 * than read — and only the ones the player answers for: `can_pause` is false
 * on a live stream that can only be stopped, and a button that does nothing is
 * worse than no button.
 *
 * Kept and updated in place like everything else on the bar. A player that
 * reports its position sends an update every second, and rebuilding four
 * elements on each one would be a composited frame per second for a title that
 * has not changed. */
function syncMprisWidget(el) {
  const player = mprisPlayer;
  if (!player) {
    /* Nothing playing: the widget collapses through `.module:empty`, exactly
       as a disk widget with no mount does. */
    if (el.children.length) el.replaceChildren();
    if (el.textContent !== '') el.textContent = '';
    return;
  }

  const parts = el._mpris ?? (el._mpris = {});
  const need = (key, tag, className) => {
    let node = parts[key];
    if (node === undefined) {
      node = parts[key] = document.createElement(tag);
      node.className = className;
      el.append(node);
    }
    return node;
  };

  /* Order matters and the elements are appended once, so they are built in
     the order they are drawn: cover, previous, play, next, then the text. */
  const cover = player.art ? need('cover', 'img', 'mpris-art') : parts.cover;
  if (cover) {
    if (player.art && cover.src !== player.art) cover.src = player.art;
    if (cover.hidden !== !player.art) cover.hidden = !player.art;
  }

  const previous = need('previous', 'button', 'mpris-button');
  setModule(previous, '󰒮');
  if (previous.hidden !== !player.can_go_previous) {
    previous.hidden = !player.can_go_previous;
  }

  const play = need('play', 'button', 'mpris-button');
  /* The glyph is what pressing it will do, which is the convention every
     player follows: a paused track shows play. */
  setModule(play, player.status === 'playing' ? '󰏤' : '󰐊');
  const canToggle = player.status === 'playing' ? player.can_pause : player.can_play;
  if (play.hidden !== !canToggle) play.hidden = !canToggle;

  const next = need('next', 'button', 'mpris-button');
  setModule(next, '󰒭');
  if (next.hidden !== !player.can_go_next) next.hidden = !player.can_go_next;

  const label = need('label', 'span', 'mpris-label');
  /* A title, with the artist after it where there is one. A player with
     neither — a browser tab that has published the interface for media it has
     not loaded — is named by the player itself, so the widget still says what
     it is. */
  const title = player.title || player.id;
  setModule(label, player.artist ? `${title} — ${player.artist}` : title);

  const tip = [player.title, player.artist, player.album]
    .filter(Boolean).join('\n');
  if (el.title !== tip) el.title = tip;

  /* Bound once, on the buttons themselves: the widget's own listeners handle
     a click that landed on neither. */
  for (const [key, action] of [['previous', 'previous'], ['play', 'play-pause'],
    ['next', 'next']]) {
    const button = parts[key];
    if (button._wired) continue;
    button._wired = true;
    button.addEventListener('click', (e) => {
      e.stopPropagation?.();
      send({ type: 'mpris.control', action });
    });
  }
}

function renderBarsWidgets() {
  for (const name of outputs.keys()) {
    const output = outputs.get(name);
    if (output) renderBarWidgets(output);
  }
}

/* Weather is fetched by the shell rather than sampled by the compositor: the
 * page can reach the network even where it cannot read /proc. open-meteo has
 * no key and answers from any origin, which is what lets the widget ask it
 * directly. A geocode lookup resolves the location, then the forecast call
 * reads the current conditions. Cached per location so several monitors make
 * one of each per refresh, and a slot is marked in-flight/failed so the
 * two-second modules pass never starts another request. */

const WEATHER_URL = 'https://api.open-meteo.com/v1/forecast';
const WEATHER_REFRESH = 15 * 60 * 1000;
const WEATHER_FAILURE_RETRY = 5 * 60 * 1000;
const weatherCache = new Map(); // location(lower) -> { text, retryAt }

function weatherText(location) {
  const hit = location && weatherCache.get(location.trim().toLowerCase());
  return hit ? hit.text : '';
}

/* Every widget definition standing on a bar right now: the shared
 * `bar_widgets` plus whatever each output's `bar_items` override placed. The
 * fetch below owes nothing to any one output, but a weather widget only named
 * by an override is still a widget somebody has to fetch for. */
function widgetDefsOnAnyBar() {
  const defs = [...barWidgets];
  for (const output of outputs.values()) {
    for (const w of output.barItemsWidgets || []) defs.push(w);
  }
  return defs;
}

/* Start a fetch for every weather widget whose slot is ready. Called on
 * config and on the refresh interval. */
function refreshWeather() {
  const now = Date.now();
  for (const w of widgetDefsOnAnyBar()) {
    if (w.type !== 'weather') continue;
    const location = (w.location || '').trim();
    if (!location) continue;
    const key = location.toLowerCase();
    const hit = weatherCache.get(key);
    if (hit && now < hit.retryAt) continue;
    weatherCache.set(key, { text: '', retryAt: now + WEATHER_REFRESH });
    fetchWeather(location);
  }
}

function fetchWeather(location) {
  const key = location.toLowerCase();
  geocode(location)
    .then(([lat, lon]) =>
      fetch(`${WEATHER_URL}?latitude=${lat}&longitude=${lon}&current=temperature_2m,weather_code`),
    )
    .then((r) => (r.ok ? r.json() : Promise.reject(new Error(String(r.status)))))
    .then((data) => {
      const c = data && data.current;
      const text = c ? weatherLine(c.weather_code, c.temperature_2m) : '';
      weatherCache.set(key, { text, retryAt: Date.now() + WEATHER_REFRESH });
      renderBarsWidgets();
    })
    .catch(() => {
      /* Leave the slot failed so we retry in a few minutes rather than on
         every sample tick. */
      weatherCache.set(key, { text: '', retryAt: Date.now() + WEATHER_FAILURE_RETRY });
    });
}

function geocode(location) {
  return fetch(
    `https://geocoding-api.open-meteo.com/v1/search?name=${encodeURIComponent(location)}&count=1&language=en&format=json`,
  )
    .then((r) => (r.ok ? r.json() : Promise.reject(new Error(String(r.status)))))
    .then((data) => {
      const hit = data && data.results && data.results[0];
      if (!hit) throw new Error(`weather: no match for "${location}"`);
      return [hit.latitude, hit.longitude];
    });
}

/* What the widget draws: the condition first and the temperature after it,
 * like every other widget on the bar — the glyph says what the number is
 * about, so it reads as a label rather than as a unit stuck on the end. A code
 * with no glyph of its own leaves the temperature on its own rather than
 * behind a leading space. */
function weatherLine(code, celsius) {
  return `${condition(code)} ${Math.round(celsius)}°C`.trim();
}

/* WMO weather codes to a short condition, matching what a temperature is not
 * enough to say. */
function condition(code) {
  if (code === 0) return '☀';
  if (code === 1 || code === 2) return '◐';
  if (code === 3) return '☁';
  if (code >= 45 && code <= 48) return '🌫';
  if (code >= 51 && code <= 67) return '🌧';
  if (code >= 71 && code <= 77) return '❄';
  if (code >= 80 && code <= 82) return '🌧';
  if (code >= 95) return '⛈';
  return '';
}

setInterval(refreshWeather, WEATHER_REFRESH);

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
    /* A `bar_items` override draws exactly the modules it names, so there may
       be no clock standing there at all. Unguarded this threw once a second,
       forever, and took every output after the first with it. */
    const el = output.modules.clock;
    if (el && el.textContent !== text) el.textContent = text;
  }
}

/* The lock screen's clock rides the same tick. One timer for the session
   rather than two: see the note above `renderClocks` for why an idle machine
   painting once a second is a cost worth counting, and shell.md's rule that
   nothing here repeats for ever. It writes text into elements that already
   exist and does nothing at all while the session is unlocked. */
setInterval(() => {
  renderClocks();
  renderLockClocks();
}, 1000);


/* ------------------------------------------------------------------------
 * The keys, on an empty desktop
 *
 * The tutorial under the mark, filled in from the keymap the compositor sent.
 *
 * From the compositor and not from a table here, because the keymap is not
 * knowable from this side: a few chords exist only in one layout — the strip's
 * consume and expel, solar's spin, the canvas's pan — and a config file may
 * add its own or shadow any of them. A list written here would be describing a
 * keyboard nobody has, and would be wrong in exactly the case someone is most
 * likely to be reading it: a layout they have just switched to.
 * --------------------------------------------------------------------- */

/* Every chord the compositor will act on, as { chord, action, mode }. Replaced
 * whole on each `config`, which is sent on connect and on reload. */
let keybinds = [];

/* Fill in every output's tutorial.
 *
 * The ordinary keymap only. A binding mode is a second keymap that is not
 * active — resize mode's `h` is not a key you can press right now — and listing
 * the two together on an empty desktop says that Mod4+h and h do the same
 * thing, which they do not.
 *
 * Nothing is dropped beyond that. A cheat sheet that decides which of your
 * bindings are worth mentioning is one you cannot trust when the key you are
 * looking for is not on it. */
function renderKeybinds() {
  if (keybinds.length === 0) return;

  const shown = keybinds.filter((bind) => !bind.mode);
  if (shown.length === 0) return;

  for (const [, output] of outputs) {
    const el = output.emptyEl?.querySelector('.keys');
    if (!el) continue;

    /* Rebuilt rather than diffed: this is drawn on a desktop with no windows
       on it, and the compositor sends `config` twice a session. */
    el.replaceChildren();
    for (const bind of shown) {
      const row = document.createElement('div');
      row.className = 'key';

      const chord = document.createElement('kbd');
      chord.textContent = bind.chord;
      const what = document.createElement('span');
      what.textContent = bind.action;

      row.append(chord, what);
      el.append(row);
    }
  }
}
