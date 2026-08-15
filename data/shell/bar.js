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
  renderBarWidgets(output);
}

function setModule(el, text) {
  if (el && el.textContent !== text) el.textContent = text;
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
  }
  return '';
}

function moduleTitle(name) {
  switch (name) {
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
  el.addEventListener('click', () => {
    const w = el._widget;
    if (!w) return;
    if (w.type === 'disk') {
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
         default path builds its own rather than reusing detached ones. */
      output.widgetsEls = undefined;
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
      const title = moduleTitle(item);
      if (el.title !== title) el.title = title;
    } else {
      el._widget = item;
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
     built them. */
  barWidgets = widgetDefs;
  output.widgetsEls = els.filter((_, i) => barItems[i] !== undefined &&
    typeof barItems[i] !== 'string');
}

/* Each widget is a short string drawn from the status sample (disk, volume)
 * or the weather cache. Every write is guarded like the modules above: an
 * assignment to textContent dirties the element whether or not the string is
 * new, and a dirty element is a repaint. */
function renderBarWidgets(output) {
  const s = lastStatus;
  (output.widgetsEls || []).forEach((el, i) => {
    const w = barWidgets[i];
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
    }
    if (el.textContent !== text) el.textContent = text;
  });
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

/* Start a fetch for every weather widget whose slot is ready. Called on
 * config and on the refresh interval. */
function refreshWeather() {
  const now = Date.now();
  for (const w of barWidgets) {
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

setInterval(renderClocks, 1000);


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
