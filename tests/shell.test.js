/* SPDX-License-Identifier: MIT
 *
 * Shell logic tests, without a browser.
 *
 * The shell is the layout engine: the tiling tree, tabs, the scrolling strip
 * and which windows end up on screen all live in shell.js, and none of it is
 * reachable from the compositor's own tests. Running it under a headless
 * compositor does not help either — the web view renders, but nothing drives
 * the layout, so a broken tree looks exactly like a working one.
 *
 * So the DOM is stubbed just far enough to run the real shell unmodified. This
 * is not a rendering engine: getBoundingClientRect returns fixed numbers and
 * the pixel results mean nothing. What it does check is structure — that four
 * windows make four columns, that consume and expel are inverses, that a
 * tabbed container shows exactly one window and it is the focused one.
 *
 * The last section runs shell.css against the elements the shell just built,
 * through the cascade in css.js. Structure and style are checked in the same
 * process for one reason: the class list on a window has to come from the real
 * geometry.js rather than from a test's idea of it, or the assertion is that
 * the test agrees with itself. See css.js for what that can and cannot show.
 *
 *   node tests/shell.test.js data/shell tiling
 *   node tests/shell.test.js data/shell scrolling
 *   node tests/shell.test.js data/shell solar
 *   node tests/shell.test.js data/shell matrix
 *   node tests/shell.test.js data/shell canvas
 *   node tests/shell.test.js data/shell tiling session
 *
 * Exits non-zero on failure. CI runs all ten combinations in the `shell`
 * job; run one by hand with the lines above when a case fails.
 */
const fs = require('fs');
const css = require('./css.js');

let idSeq = 0;
/* The element the shell last asked to focus. One global because a real
   document has one focus, and the pickers hand it back and forth. */
let focusedEl = null;

class El {
  constructor(tag) {
    this.tagName = tag;
    this.children = [];
    this.parentElement = null;
    const props = {};
    this.style = new Proxy(props, {
      set: (t, k, v) => { t[k] = v; return true; },
      get: (t, k) => {
        if (k === 'setProperty') return (n, v) => { t[n] = v; };
        if (k === 'getPropertyValue') return (n) => t[n] ?? '';
        if (k === 'removeProperty') return (n) => { delete t[n]; };
        return t[k] ?? '';
      },
    });
    this.dataset = {};
    this._classes = new Set();
    this.hidden = false;
    this.textContent = '';
    this._id = ++idSeq;
    this.listeners = {};
    /* Attributes, which until the shell started writing ARIA onto its rows
       nothing here needed: a class list and a `hidden` flag were the whole of
       what the shell set. They are a plain object rather than a Map because
       the assertions read them by name and `attrs['aria-selected']` is what a
       test wants to write. */
    this.attrs = {};
    /* Where the keyboard is. `focus()` records it rather than moving anything,
       because there is no engine here to move it in — what a test needs to
       know is which element the shell *asked* for, which is exactly the
       question a lost focus ring turns into. */
    this._focused = false;
  }
  setAttribute(name, value) { this.attrs[name] = String(value); }
  getAttribute(name) { return this.attrs[name] ?? null; }
  removeAttribute(name) { delete this.attrs[name]; }
  focus() {
    if (focusedEl && focusedEl !== this) focusedEl._focused = false;
    focusedEl = this;
    this._focused = true;
  }
  blur() { this._focused = false; if (focusedEl === this) focusedEl = null; }
  /* A real click, invented. The shell's keyboard navigation activates a row by
     clicking it — one path from "chosen" to "sent", whether a pointer or an
     arrow key chose it — so a stub with no `click()` would make every keyboard
     test silently assert nothing at all. */
  click() { this.dispatch('click', {}); }
  scrollIntoView() {}
  dispatch(type, event = {}) {
    for (const fn of this.listeners[type] ?? []) {
      fn({ target: this, preventDefault() {}, stopPropagation() {}, ...event });
    }
  }
  get className() { return [...this._classes].join(' '); }
  set className(v) { this._classes = new Set(String(v).split(/\s+/).filter(Boolean)); }
  get classList() {
    const self = this;
    return {
      add: (...c) => c.forEach((x) => self._classes.add(x)),
      remove: (...c) => c.forEach((x) => self._classes.delete(x)),
      toggle: (c, on) => { on ? self._classes.add(c) : self._classes.delete(c); },
      contains: (c) => self._classes.has(c),
    };
  }
  append(...nodes) {
    for (const n of nodes) {
      if (n === undefined || n === null) continue;
      if (n.parentElement) n.parentElement.remove_child(n);
      n.parentElement = this;
      this.children.push(n);
    }
  }
  remove_child(n) { this.children = this.children.filter((c) => c !== n); }
  replaceChildren(...nodes) {
    for (const c of this.children) c.parentElement = null;
    this.children = [];
    this.append(...nodes);
  }
  remove() { if (this.parentElement) this.parentElement.remove_child(this); }
  addEventListener(type, fn) { (this.listeners[type] ??= []).push(fn); }
  /* Taken off again, which until the shell started rebinding a surface's keys
     on a page-level element nothing here needed. A stub that accepted the
     removal and did not perform it would let exactly the bug this exists to
     catch — a handler per opening, on an element that outlives them all —
     pass silently. */
  removeEventListener(type, fn) {
    this.listeners[type] = (this.listeners[type] ?? []).filter((f) => f !== fn);
  }
  querySelector(sel) {
    const want = sel.replace(/^\./, '');
    const hit = (el) => el._classes.has(want) || el.tagName === want;
    const stack = [...this.children];
    while (stack.length) {
      const el = stack.shift();
      if (hit(el)) return el;
      stack.push(...el.children);
    }
    return null;
  }
  getBoundingClientRect() {
    /* Overridable per element, so a test can place a window off the edge of
     * its output and check what gets clipped. */
    const r = this.__rect ??
      { left: 0, top: 0, width: 1920, height: 1050 };
    return { ...r, x: r.left, y: r.top };
  }
  cloneNode() { return buildDesktop(); }
}

function buildDesktop() {
  const root = new El('div');
  const main = new El('main');
  main.className = 'desktop';
  for (const c of ['windows', 'empty', 'notifications']) {
    const el = new El('div');
    el.className = `${c} module`;
    /* The empty desktop's tutorial holds the keymap, which the shell fills in
       from what the compositor sent. Nested as index.html nests it: the shell
       finds it with querySelector under `.empty`, so a flat desktop here would
       leave that code running against nothing. */
    if (c === 'empty') {
      const keys = new El('pre');
      keys.className = 'keys';
      el.append(keys);
    }
    main.append(el);
  }
  /* The bar, nested as index.html nests it rather than flattened alongside
     everything else. The shell finds all of these with querySelector either
     way, but its entrance animation walks the two halves for the modules to
     deal out — so a flat desktop would leave that code running against nothing
     here and against real markup on a real screen. */
  const bar = new El('div');
  bar.className = 'bar';
  for (const [half, contents] of [['bar-left', ['workspaces', 'taskbar']],
    ['bar-right', ['mode', 'tray', 'clock', 'cpu', 'memory', 'load', 'disk',
      'net']]]) {
    const el = new El('div');
    el.className = half;
    for (const c of contents) {
      const child = new El('div');
      child.className = `${c} module`;
      el.append(child);
    }
    bar.append(el);
  }
  main.append(bar);
  root.append(main);
  return root;
}

function buildWindow() {
  const root = new El('div');
  const section = new El('section');
  section.className = 'window';
  const viewport = new El('div');
  viewport.className = 'viewport';
  section.append(viewport);
  root.append(section);
  return root;
}

const sent = [];
/* The compositor answers a view.focus request with a view.focused event. The
 * shell relies on that round trip — its own focusedId only moves when the
 * event comes back — so the stub has to close the loop or every test runs with
 * a stale focus. */
const pendingFocus = [];
const outputsEl = new El('div');
/* Held rather than made inline, so a test can look at what the chooser drew. */
const screencastEl = new El('div');
/* And the same for the notification strip, which additionally has to be one
   element rather than a fresh one per lookup: what a dismissal leaves behind
   is only visible if the container is the one the shell appended to. */
const notificationsEl = new El('div');
/* And the tray menu, for the same reason: what a click on a row leaves behind
   is only visible if the element is the one the shell drew into. */
const trayMenuEl = new El('div');
/* And the clipboard picker. */
const clipboardEl = new El('div');
/* And the launcher, on the same terms: what a launch leaves behind is only
   visible if the element the shell drew into is the one the test reads back. */
const launcherEl = new El('div');
/* And the notification centre, for the same reason: what a forget leaves
   behind is only visible if the element the shell drew into is the one the
   test reads back. */
const notificationCentreEl = new El('div');
const powerEl = new El('div');
/* And the two radio pickers, which are one element each for the same reason:
   what a click on a row left behind is only visible if the element the shell
   drew into is the one the test reads back. */
const networkEl = new El('div');
const bluetoothEl = new El('div');
/* And the settings panel, on the same terms: what a switch was drawn in — and
   what a click on it sent — is only visible if the element the shell drew into
   is the one the test reads back. */
const settingsEl = new El('div');
/* And the on-screen keyboard, on the same terms: what a tap leaves behind is
   only visible if the element the shell drew into is the one read back. */
const oskEl = new El('div');
/* And the calendar under the clock, for the same reason again: which month it
   is showing and which day is marked are only visible if the element the shell
   drew into is the one the test reads back. */
const calendarEl = new El('div');
/* And the lock screen, on the same terms: whether it is up, what it says and
   what it sends are only visible if the element the shell drew into is the one
   the test reads back. */
const lockEl = new El('div');
const desktopTemplate = { content: { cloneNode: () => buildDesktop() } };
const windowTemplate = { content: { cloneNode: () => buildWindow() } };

/* The FLIP inverts positions, forces a reflow, then releases — all in one
 * synchronous batch, so by the time relayout returns every transform is back
 * to ''. The reflow is the only moment the inverted state exists, and reading
 * documentElement.offsetWidth is what triggers it, so that read is where the
 * state gets sampled. */
let flipSnapshot = null;
const documentElement = new El('html');
Object.defineProperty(documentElement, 'offsetWidth', {
  get() {
    flipSnapshot = new Map();
    for (const [id, view] of globalThis.__shell?.views ?? []) {
      flipSnapshot.set(id, view.el.style.transform);
    }
    return 0;
  },
});

const documentListeners = {};
global.document = {
  documentElement,
  getElementById: (id) => ({
    outputs: outputsEl,
    notifications: notificationsEl,
    screencast: screencastEl,
    'tray-menu': trayMenuEl,
    clipboard: clipboardEl,
    launcher: launcherEl,
    'notification-centre': notificationCentreEl,
    'power-picker': powerEl,
    'network-picker': networkEl,
    'bluetooth-picker': bluetoothEl,
    settings: settingsEl,
    osk: oskEl,
    calendar: calendarEl,
    lock: lockEl,
    'desktop-template': desktopTemplate,
    'window-template': windowTemplate,
  }[id]),
  createElement: (tag) => new El(tag),
  /* Listeners on the document itself, which the shell uses for the things
     that hold for the whole desktop rather than for one element — the
     right-click menu it turns off. Recorded so a test can fire one. */
  addEventListener: (type, fn) => { (documentListeners[type] ??= []).push(fn); },
  removeEventListener: (type, fn) => {
    documentListeners[type] = (documentListeners[type] ?? [])
      .filter((f) => f !== fn);
  },
};

const windowListeners = {};
global.window = {
  webkit: { messageHandlers: { viewport: { postMessage: (m) => {
    const msg = JSON.parse(m);
    sent.push(msg);
    if (msg.type === 'view.focus') pendingFocus.push(msg.id);
  } } } },
  addEventListener: (type, fn) => { (windowListeners[type] ??= []).push(fn); },
  removeEventListener: (type, fn) => {
    windowListeners[type] = (windowListeners[type] ?? []).filter((f) => f !== fn);
  },
};
global.ResizeObserver = class { observe() {} unobserve() {} };
/* Frame callbacks run inline, and a timestamp is supplied because the fade
 * tween is driven by elapsed time. It advances by a frame each call so the
 * tween terminates instead of spinning at t=0. */
let fakeClock = 0;
global.requestAnimationFrame = (fn) => { fakeClock += 16; fn(fakeClock); };
global.performance = { now: () => fakeClock };
global.matchMedia = () => ({ matches: false });

/* The tween engine, stubbed.
 *
 * The real one is data/shell/vendor/gsap.min.js and it is not loaded here: it
 * wants a window with a layout, and this file has an object with a Proxy for a
 * style. What the shell asks of it, though, is small and worth exercising
 * rather than stubbing to nothing — a fade whose samples never arrive is a
 * window that opens invisible, and that is exactly the kind of thing these
 * tests are for. So this runs the tween: numeric properties are interpolated
 * on the fake clock above, every callback fires, and the end state is applied.
 *
 * It is not GSAP. Eases are ignored, strings are assigned rather than parsed,
 * and staggers land together. Nothing here checks what an animation looks
 * like; what it checks is what the animation *sends* and what it leaves
 * behind. */
const TWEEN_KEYS = new Set(['duration', 'ease', 'stagger', 'onUpdate',
  'onComplete', 'onStart', 'clearProps', 'overwrite', 'immediateRender']);

function applyTweenValues(target, values) {
  for (const [key, value] of Object.entries(values)) {
    if (TWEEN_KEYS.has(key)) continue;
    if (target.style) target.style[key] = value;
    else target[key] = value;
  }
}

function runTween(targets, from, to) {
  const list = Array.isArray(targets) ? targets : [targets];
  const frames = Math.max(1, Math.round(((to.duration ?? 0.2) * 1000) / 16));

  for (const target of list) {
    if (from) applyTweenValues(target, from);
    /* Only plain objects are interpolated. An element's styles are strings and
       units as far as this file is concerned, so they jump to their end value;
       a number on a bare object is the surface-opacity tween, and its
       intermediate values are the whole point of it. */
    const numeric = !target.style
      ? Object.keys(to).filter((k) => !TWEEN_KEYS.has(k)
        && typeof to[k] === 'number' && typeof target[k] === 'number')
      : [];
    const start = Object.fromEntries(numeric.map((k) => [k, target[k]]));

    for (let frame = 1; frame <= frames; frame++) {
      const t = frame / frames;
      for (const key of numeric) target[key] = start[key] + (to[key] - start[key]) * t;
      to.onUpdate?.();
    }
    applyTweenValues(target, to);
    /* What GSAP's own `clearProps` does at the end: take the inline values off
       again and leave the element to the stylesheet. Modelled because the
       shell uses it to grow a notification into whatever size its text made it
       — a stub that left the tween's own numbers behind would leave every
       arrival pinned to the box it was measured against. */
    if (to.clearProps && target.style) {
      for (const key of String(to.clearProps).split(',')) {
        target.style.removeProperty(key.trim());
      }
    }
  }
  to.onComplete?.();
}

/* `gsap.from` animates *from* the values it is given to whatever the element
 * already has, which is not the same as running the same values twice: a
 * notification growing in is growing to a height nothing has written down. So
 * the destination is read off the target before the start values are applied
 * over it. */
function runFromTween(targets, vars) {
  const list = Array.isArray(targets) ? targets : [targets];
  for (const target of list) {
    const destination = { duration: vars.duration, ease: vars.ease,
      onUpdate: vars.onUpdate, onComplete: vars.onComplete,
      clearProps: vars.clearProps };
    for (const key of Object.keys(vars)) {
      if (TWEEN_KEYS.has(key)) continue;
      destination[key] = target.style ? target.style[key] : target[key];
    }
    runTween(target, vars, destination);
  }
}

global.gsap = {
  /* Accepted and ignored: what it sets — when the engine's frame callback goes
     back to sleep, whether it promotes a layer — is about a browser, and there
     is not one here. */
  config: () => {},
  defaults: () => {},
  to: (targets, vars) => runTween(targets, null, vars),
  from: (targets, vars) => runFromTween(targets, vars),
  fromTo: (targets, from, to) => runTween(targets, from, to),
  set: (targets, vars) => runTween(targets, null, { ...vars, duration: 0 }),
  /* A timeline runs each tween as it is added and there is no clock to hold
     the next one back, so it has finished as soon as it has anything on it.
     onComplete therefore fires once, after the first tween: later ones still
     run, on whatever the callback left behind. That is close enough for the
     one thing that depends on it — a notification's exit ending in the element
     being removed — and it is why nothing here should be given work to do
     after an animation that this file cannot actually sequence. */
  timeline: (config = {}) => {
    let finished = false;
    const finish = () => {
      if (finished) return;
      finished = true;
      config.onComplete?.();
    };
    const self = {
      to: (targets, vars) => { runTween(targets, null, vars); finish(); return self; },
      from: (targets, vars) => { runFromTween(targets, vars); finish(); return self; },
      fromTo: (targets, from, to) => { runTween(targets, from, to); finish(); return self; },
    };
    return self;
  },
};

/* Top-level const/let inside an eval stay in that eval's own scope, so the
 * shell's state is unreachable from out here unless it hands it over. */
/* overviewStateForTest is a function rather than the maps themselves so the
 * test does not depend on where that state is kept. */
const EXPORTS = ';globalThis.__shell = { views, workspaces, outputs, scrollOffsets, overviewThumbs,'
  + ' workspaceOfForTest: workspaceOf,'
  + ' overviewStateForTest: (id) => views.get(id)?.overview ?? {},'
  + ' floatingForTest: (id) => views.get(id)?.floating ?? null,'
  + ' fullscreenOnForTest: fullscreenOn,'
  + ' dynamicOrderForTest: dynamicOrder,'
  + ' TILING_MODES, LAYOUT_MODES,'
  + ' get tilingMode() { return tilingMode; },'
  + ' get layoutMode() { return layoutMode; },'
  /* The solar layout's kernel, which is a pure function of (ids, sun, area)
     and is the one piece of layout arithmetic in the shell that can be checked
     without a browser. Exported through a getter for SOLAR so that a test
     moving the sun's mass sees the same object the shell does. */
  + ' solarForTest: { placements: solarPlacements, sunOf: solarSunOf,'
  + '   recalculate: recalculateSolarLayout, get SOLAR() { return SOLAR; },'
  + '   get field() { return solarField; } },'
  /* The matrix layout's kernel, which is a pure function of (windows, screen)
     — the same case as solar's, and checkable here for the same reason. The
     focus history is handed over as the live array rather than a copy, so a
     test can watch what a view.focused did to it. */
  + ' matrixForTest: { calculate: calculateLayout, capacity: matrixCapacity,'
  + '   order: matrixOrderOf, recalculate: recalculateMatrixLayout,'
  + '   stack: focusStack, get MATRIX() { return MATRIX; } },'
  /* The canvas layout's kernel, which is a pure function of (items, viewport,
     area) — the same case again. The places and the viewports are handed over
     as the live maps rather than as copies, so a test can watch what a pan or
     a newly opened window did to them. */
  + ' canvasForTest: { project: canvasProject, fit: canvasFitViewport,'
  + '   follow: canvasFollow, zoomed: canvasZoomed, bounds: canvasBounds,'
  + '   clamp: canvasClampZoom, places: canvasPlaces,'
  + '   viewport: canvasViewportOf, area: canvasAreaOf,'
  + '   recalculate: recalculateCanvasLayout,'
  /* The reload path: what goes into the session file and what comes back out
     of it. A reload is a new page with both maps empty, so this is the whole
     of what stands between it and a desktop swept into a pile. */
  + '   serialise: serialiseCanvas, restore: restoreCanvas,'
  + '   drop: dropCanvasSlots, get slots() { return canvasSlots; },'
  + '   fill: canvasFillFocused, followMargin: canvasFollowMargin,'
  + '   get CANVAS() { return CANVAS; } },'
  /* The session file as it would be written. The save itself is debounced by
     a real timer, so nothing synchronous can observe the message — and what
     is worth checking is what goes *into* the file rather than when. */
  + ' sessionForTest: { serialise: serialiseSession },'
  /* The tray menu's element, and the document's own listeners, so a test can
     see what was drawn and fire the click that missed it. */
  + ' get trayMenuEl() { return trayMenuEl; },'
  + ' get clipboardEl() { return clipboardEl; },'
  + ' get launcherEl() { return launcherEl; },'
  + ' get notificationCentreEl() { return notificationCentreEl; },'
  /* The age formatter, which is the one piece of the centre with an
     answer a test can state exactly: everything else it draws is text
     handed to it. */
  + ' notificationAgeForTest: notificationAge,'
  + ' get powerEl() { return powerEl; },'
  + ' get networkEl() { return networkEl; },'
  + ' get bluetoothEl() { return bluetoothEl; },'
  + ' get settingsEl() { return settingsEl; },'
  + ' get oskEl() { return oskEl; },'
  /* The calendar's element, and the two pure functions under the clock: what
     the module says for a given moment, and which day the locale starts its
     week on. Both are answers a test can state exactly, which nothing else the
     bar draws is — the rest of it is a number the compositor sampled. */
  + ' get calendarEl() { return calendarEl; },'
  + ' clockTextForTest: clockText,'
  + ' applyClockForTest: applyClock,'
  + ' calendarFirstDayForTest: calendarFirstDay,'
  + ' get lockEl() { return lockEl; },'
  /* The keyboard's own idea of Shift and which page it is on, which nothing
     drawn on the page says directly — a test reading capitalisation off a
     rendered key would be testing toUpperCase rather than the shell. */
  + ' get oskShiftStateForTest() { return oskShiftState; },'
  + ' get oskPageForTest() { return oskPage; },'
  /* So a test can put the double-tap-lock timer somewhere it is not about to
     collide with the fake clock's actual value, without reaching for the
     fake clock itself — bumping that would also move every tween in every
     other section of this file that runs after this one. */
  + ' set oskShiftTappedAtForTest(v) { oskShiftTappedAt = v; },'
  /* What smart gaps decide, which is a question about the tree and the layout
     mode rather than about any pixel: whether this workspace holds a lone
     window that fills the tiling area. */
  + ' smartGapsForTest: { single: singleWindowOn, edge: edgeGapPx,'
  + '   radius: smartRadius },'
  /* Measuring a window and sending the result, which is the one place where
     the page's fractional layout has to become the compositor's whole pixels.
     Exported so a test can hand it a rect off the pixel grid — the browser
     produces those constantly and this harness's stubs never would. */
  + ' reportGeometryForTest: reportGeometry,'
  /* What the weather widget draws for a forecast, which is the one part of
     that widget reachable without a network: the fetch is stubbed nowhere and
     the line it composes is what the bar shows. */
  + ' weatherLineForTest: weatherLine,'
  /* The keymap the compositor sent, which is the only thing the tutorial on
     an empty desktop is drawn from. */
  + ' keybindsForTest: () => keybinds,'
  /* The grid's row count, which decides the whole shape and is a pure function
     of (count, w, h). The arrangement it feeds is checked through the tree the
     mode builds, like the other dynamic ones; this is here because the aspects
     worth checking are the ones no stubbed getBoundingClientRect can produce —
     a 32:9 and a monitor on its end. */
  + ' gridForTest: { rows: gridRows, counts: gridCounts, build: grid },'
  + ' get activeOutput() { return activeOutput; } };';
/* The shell is a set of ordered classic scripts sharing one global scope, so
 * concatenating them in load order and evaluating the result is exactly what
 * the browser does with the <script> tags — the same bindings end up in the
 * same one scope either way.
 *
 * The order is read out of index.html rather than listed here. A second list
 * would be a second thing to keep in step, and the failure when it drifted
 * would be a ReferenceError deep in a test rather than anything naming the
 * cause. index.html is where a browser gets the order, so it is the order. */
const shellDir = process.argv[2];
const document_html = fs.readFileSync(`${shellDir}/index.html`, 'utf8');
const order = [...document_html.matchAll(/<script src="([^"]+)"><\/script>/g)]
  .map((m) => m[1])
  /* Everything the shell was written here, minus what it was given. The
     vendored bundle is a real browser library and this is not a real browser:
     it would be evaluating a minified engine against a Proxy pretending to be
     a style declaration, which tests nothing and fails for reasons about the
     stub. The stub above stands in for it. Excluded by where it lives rather
     than by name, so a second vendored file needs no change here — but note
     that anything put under vendor/ is then untested by this file. */
  .filter((file) => !file.startsWith('vendor/'));

if (order.length === 0) {
  console.error(`no <script src> tags found in ${shellDir}/index.html`);
  process.exit(1);
}

const src = order.map((f) => fs.readFileSync(`${shellDir}/${f}`, 'utf8')).join('\n')
  + '\n' + EXPORTS;
(0, eval)(src);

function emit(message) {
  /* Before the message and again after it. The compositor answers a
     view.focus before it sends whatever happens next, so a round trip the
     shell asked for outside an emit — a picker closing on a click, which
     hands the keyboard back — has to have completed by the time the next
     message is delivered. Draining only afterwards made that answer arrive
     *after* the next event instead of before it, which showed up as a tray
     menu that opened and was immediately closed again by the view.focused
     belonging to the picker that closed before it. */
  drainFocus();
  for (const fn of windowListeners.viewport ?? []) fn({ detail: message });
  drainFocus();
}

/* Bounded, because a shell bug that focuses in a loop should fail the test
   rather than hang it. */
function drainFocus() {
  for (let guard = 0; pendingFocus.length > 0 && guard < 20; guard++) {
    const id = pendingFocus.shift();
    for (const fn of windowListeners.viewport ?? []) {
      fn({ detail: { type: 'view.focused', id } });
    }
  }
}

/* --- drive it --------------------------------------------------------- */

function check(label, cond) {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${label}`);
  if (!cond) process.exitCode = 1;
}

const mode = process.argv[3] ?? 'tiling';
/* A third mode drives the restart path instead of the layout ones: restore a
 * saved layout into an empty session, then bring the applications back and
 * check they land where they were rather than in the order they started. */
const sessionTest = process.argv[4] === 'session';

/* Named because a config message replaces the rules wholesale, so any later
   one — a test changing the gaps, say — has to send these again or it takes
   the window rules out from under everything after it. */
const HARNESS_RULES = [
  { app_id: 'pinned', workspace: 6 },
  { app_id: 'dialogy', floating: true, x: 10, y: 20, width: 300, height: 200 },
];
emit({ type: 'config', layout: mode, rules: HARNESS_RULES });
emit({ type: 'output.layout', outputs: [{
  name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
  usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
  scale: 1, transform: 'normal', modes: [], enabled: true,
}] });

if (sessionTest) {
  const saved = JSON.stringify({
    version: 1,
    layout: mode,
    workspaces: {
      3: { type: 'leaf', app: 'firefox', weight: 1 },
      5: { type: 'split', dir: 'horizontal', layout: 'split', weight: 1,
        active: 0, children: [
          { type: 'leaf', app: 'foot', weight: 1 },
          { type: 'leaf', app: 'foot', weight: 2 },
        ] },
    },
    outputs: { 'DP-1': { workspace: 5 } },
  });

  emit({ type: 'session.restore', state: saved });

  const outs = globalThis.__shell.outputs;
  check('the saved workspace is restored to its output',
    [...outs.values()][0].workspace === 5);

  /* Applications come back in an order that has nothing to do with the
     layout — the browser last, as it usually is. */
  const open = (id, app) => emit({ type: 'view.added', id, title: app,
    app_id: app, output: 'DP-1', min_width: 0, min_height: 0,
    floating: false, width: 800, height: 600 });

  open(11, 'foot');
  open(12, 'foot');
  open(13, 'firefox');

  const ws = globalThis.__shell.workspaceOfForTest;
  check('the browser went back to its own workspace', ws(13) === 3);
  check('both terminals went back to theirs',
    ws(11) === 5 && ws(12) === 5);

  /* And into the shape they left, weights included. */
  const pair = globalThis.__shell.workspaces.get(5).children;
  check('they kept the sizes they had',
    pair.length === 2 && pair[0].weight === 1 && pair[1].weight === 2);

  /* A floating window comes back floating, on its workspace, at its rect —
     none of which is in the tree, so it needs its own record to survive. */
  const floatState = JSON.stringify({
    version: 1, layout: mode, workspaces: {}, outputs: {},
    floating: [{ app: 'pavucontrol', workspace: 4,
      x: 111, y: 222, width: 333, height: 444 }],
  });
  globalThis.__shell.views.clear();
  emit({ type: 'session.restore', state: floatState });
  open(20, 'pavucontrol');

  const record = globalThis.__shell.floatingForTest(20);
  check('a floating window comes back floating', record !== null);
  check('on the workspace it was on', record?.workspace === 4);
  check('at the rect it had',
    record?.x === 111 && record?.y === 222 &&
    record?.width === 333 && record?.height === 444);

  /* An application with no slot is placed normally rather than refused. */
  open(14, 'ghostty');
  check('an application with no saved place still opens',
    ws(14) !== null);

  check('teardown clean', process.exitCode !== 1);
  process.exit(process.exitCode ?? 0);
}

for (let id = 1; id <= 4; id++) {
  emit({ type: 'view.added', id, title: `win${id}`, app_id: 'test',
    output: 'DP-1', min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });
  emit({ type: 'view.focused', id });
}

const layouts = sent.filter((m) => m.type === 'view.layout');
check('windows laid out', new Set(layouts.map((m) => m.id)).size === 4);

/* Closing a window hands focus to its neighbour, not to whatever is first on
 * the workspace. */
{
  const open = (id) => emit({ type: 'view.added', id, title: `c${id}`,
    app_id: 'closer', output: 'DP-1', min_width: 0, min_height: 0,
    floating: false, width: 800, height: 600 });

  for (const id of [70, 71, 72]) open(id);
  emit({ type: 'view.focused', id: 72 });

  const before = sent.length;
  emit({ type: 'view.removed', id: 72 });
  const focus = sent.slice(before).find((m) => m.type === 'view.focus');
  check('closing focuses the neighbour it sat beside', focus?.id === 71);

  /* And not something arbitrary from the other end of the workspace. */
  check('not merely the first window on the workspace', focus?.id !== 70);

  for (const id of [70, 71]) emit({ type: 'view.removed', id });
  emit({ type: 'view.focused', id: 4 });
}

/* A newly opened window takes focus, however it was launched. */
{
  const before = sent.length;
  emit({ type: 'view.added', id: 60, title: 'new', app_id: 'new-app',
    output: 'DP-1', min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });
  check('opening a window focuses it',
    sent.slice(before).some((m) => m.type === 'view.focus' && m.id === 60));

  /* A replayed one is not new: the shell reloads on every edit to it, and
     focusing then would move focus to whatever came last in the list. */
  const mark = sent.length;
  emit({ type: 'view.added', id: 61, title: 'old', app_id: 'old-app',
    output: 'DP-1', min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600, replay: true });
  check('a replayed window does not steal focus',
    !sent.slice(mark).some((m) => m.type === 'view.focus' && m.id === 61));

  /* A rule that puts a window on another workspace was an instruction to leave
     it there, not to be taken there. */
  const away = sent.length;
  emit({ type: 'view.added', id: 62, title: 'pinned', app_id: 'pinned-app',
    output: 'DP-1', min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });
  check('a window a rule sent elsewhere does not pull focus with it',
    !sent.slice(away).some((m) => m.type === 'view.focus' && m.id === 62));

  for (const id of [60, 61, 62]) emit({ type: 'view.removed', id });
  emit({ type: 'view.focused', id: 4 });
}

/* The system tray. The compositor holds the StatusNotifierWatcher name and
 * does every D-Bus call an item answers; the shell is handed a picture, a
 * label and a key, and sends the key back with where the icon is. What is
 * worth checking here is that the snapshot replaces rather than accumulates,
 * and that a click names the item and a position — an application draws its
 * own menu at the coordinates it is given, so a wrong number there is a menu
 * in the corner of the screen. */
{
  const tray = () => globalThis.__shell.outputs.get('DP-1').modules.tray;
  const click = (el, type, event = {}) =>
    (el.listeners[type] ?? []).forEach((fn) => fn({ preventDefault() {}, ...event }));

  emit({ type: 'tray.update', items: [
    { id: ':1.5/StatusNotifierItem', title: 'Nextcloud', status: 'active',
      icon: 'data:image/png;base64,AA==', tooltip: 'Up to date', is_menu: false },
    { id: ':1.9/StatusNotifierItem', title: 'Steam', status: 'passive',
      icon: '', tooltip: '', is_menu: true },
  ] });
  check('every registered item is drawn', tray().children.length === 2);
  check('an item with no icon shows its own initial instead',
    tray().children[1].dataset.fallback === 'S');
  check('and one that asked not to be seen is marked rather than dropped',
    tray().children[1]._classes.has('passive'));
  check('the tooltip is what the item published',
    tray().children[0].title === 'Up to date');

  let before = sent.length;
  click(tray().children[0], 'click');
  const activate = sent.slice(before).find((m) => m.type === 'tray.activate');
  check('a click names the item it landed on',
    activate?.id === ':1.5/StatusNotifierItem' && activate.button === 'primary');
  check('and where the icon is, so the application can place its own window',
    Number.isFinite(activate?.x) && Number.isFinite(activate?.y));

  before = sent.length;
  click(tray().children[0], 'contextmenu');
  check('a right click asks for the menu',
    sent.slice(before).find((m) => m.type === 'tray.activate')?.button === 'menu');

  before = sent.length;
  click(tray().children[0], 'wheel', { deltaY: -1, deltaX: 0 });
  const scroll = sent.slice(before).find((m) => m.type === 'tray.scroll');
  check('the wheel turns into a step, not a pixel count',
    scroll?.delta === 1 && scroll.orientation === 'vertical');

  /* The message is a snapshot: an application exiting arrives as a shorter
     list, not as a removal, and a shell that appended would keep the icon of
     every program that has ever run. */
  emit({ type: 'tray.update', items: [
    { id: ':1.9/StatusNotifierItem', title: 'Steam', status: 'active',
      icon: '', tooltip: '', is_menu: true },
  ] });
  check('a shorter snapshot takes the missing item off the bar',
    tray().children.length === 1 &&
    tray().children[0].dataset.fallback === 'S');

  emit({ type: 'tray.update', items: [] });
  check('and an empty one — the tray switched off — empties the bar',
    tray().children.length === 0);
}

/* A tray item's menu. The compositor reads it off the application over
 * com.canonical.dbusmenu and sends it whole; the shell draws exactly that and
 * decides nothing about what is in it. What is worth checking here is that a
 * chosen row is named back by the application's own id, that a submenu opens
 * without the menu leaving its rectangle, and that a menu is never left on
 * screen with nobody told. */
{
  const menu = () => globalThis.__shell.trayMenuEl;
  const rows = () => menu().children[0]?.children ?? [];
  const click = (el, type = 'click', event = {}) =>
    (el.listeners[type] ?? []).forEach((fn) =>
      fn({ preventDefault() {}, stopPropagation() {}, ...event }));
  const fire = (type) =>
    (documentListeners[type] ?? []).forEach((fn) =>
      fn({ preventDefault() {}, stopPropagation() {} }));

  const open = () => emit({ type: 'tray.menu', id: ':1.5/StatusNotifierItem',
    x: 100, y: 30, items: [
      { id: 1, label: 'Open', kind: 'standard', enabled: true },
      { id: 2, label: '', kind: 'separator', enabled: true },
      { id: 3, label: 'Sync now', kind: 'standard', enabled: false },
      { id: 4, label: 'Automatic', kind: 'standard', enabled: true,
        toggle: 'checkmark', checked: true },
      { id: 5, label: 'Recent', kind: 'standard', enabled: true, children: [
        { id: 6, label: 'notes.md', kind: 'standard', enabled: true },
      ] },
    ] });

  open();
  check('the menu is drawn where the compositor said the icon was',
    menu().hidden === false && menu().style.top === '30px');
  check('every row the application published is there', rows().length === 6);
  check('a separator is a line, not a button',
    rows()[1].tagName === 'div' && rows()[1]._classes.has('tray-menu-separator'));
  check('a disabled row is marked rather than dropped',
    rows()[2]._classes.has('disabled'));
  check('a ticked row says so in text, not in colour alone',
    rows()[3].children[0].textContent.startsWith('✓'));
  check('and a submenu starts closed', rows()[5].hidden === true);

  let before = sent.length;
  click(rows()[2]);
  check('a disabled row sends nothing',
    !sent.slice(before).some((m) => m.type === 'tray.menu.click'));

  before = sent.length;
  click(rows()[4]);
  check('a row with children opens instead of choosing',
    rows()[5].hidden === false &&
    !sent.slice(before).some((m) => m.type === 'tray.menu.click'));

  before = sent.length;
  click(rows()[5]);
  const chosen = sent.slice(before).find((m) => m.type === 'tray.menu.click');
  check('choosing a row names it by the application\'s own id',
    chosen?.item === 6 && chosen.id === ':1.5/StatusNotifierItem');
  check('and the menu goes', menu().hidden === true);
  check('with nothing sent about closing, because the click said so',
    !sent.slice(before).some((m) => m.type === 'tray.menu.closed'));
  check('so nothing of it is drawn over the windows any more',
    !(sent.slice(before).filter((m) => m.type === 'shell.overlay').at(-1)
      ?.rects ?? []).some((r) => r.name === 'tray-menu'));

  open();
  before = sent.length;
  fire('click');
  check('a click that missed the menu takes it down',
    menu().hidden === true);
  check('and the application is told, because nothing else told it',
    sent.slice(before).some((m) => m.type === 'tray.menu.closed' &&
      m.id === ':1.5/StatusNotifierItem'));

  open();
  before = sent.length;
  emit({ type: 'view.focused', id: 4 });
  check('a click on a window closes it too — the shell never sees that click',
    menu().hidden === true &&
    sent.slice(before).some((m) => m.type === 'tray.menu.closed'));

  open();
  emit({ type: 'tray.update', items: [] });
  check('and an application that exits does not leave its menu behind',
    menu().hidden === true);
}

/* The clipboard history picker. The compositor brokers every selection on the
 * session and keeps the last few; this file draws them and sends back which
 * one was chosen. Nothing here reads a selection or knows what a mime type is,
 * which is the point — the history outlives the application that copied it,
 * and that is only possible in the compositor. */
{
  const picker = () => globalThis.__shell.clipboardEl;
  /* The docking box `picker()` names to the compositor is only ever the
     output-sized frame the dialog is centred in — see renderClipboard's own
     comment for why. Everything actually drawn is one level in, inside the
     `.clipboard-dialog` that box holds. */
  const dialog = () => picker().children[0];
  const rows = () => (dialog()?.children[0]?.children ?? [])
    .filter((el) => el._classes.has('clipboard-row'));
  const click = (el, event = {}) => (el.listeners.click ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {}, ...event }));
  const fire = (type) => (documentListeners[type] ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));

  emit({ type: 'clipboard.history', entries: [
    { id: 3, text: 'the newest thing' },
    { id: 2, text: 'a\nmultiline\nentry' },
    { id: 1, text: 'the oldest thing' },
  ] });
  /* Kept, but nothing drawn: the message arrives on every copy, and drawing
     a picker nobody opened would be a composited frame of the whole desktop
     per copy. */
  check('the history is kept but not drawn until it is asked for',
    picker().children.length === 0);

  let before = sent.length;
  emit({ type: 'shell.command', command: 'clipboard', args: [] });
  check('the binding opens the picker', picker().hidden === false);
  check('and it asks the compositor what is in the clipboard now',
    sent.slice(before).some((m) => m.type === 'clipboard.query'));
  check('every entry is a row, newest first', rows().length === 3 &&
    rows()[0].children[0].textContent === 'the newest thing');
  /* What is pasted is what was copied; what is drawn is one line, or a row
     could be as tall as the screen. */
  check('a multi-line entry is drawn on one line',
    rows()[1].children[0].textContent === 'a multiline entry');

  before = sent.length;
  click(rows()[2]);
  check('choosing a row asks the compositor to put it back on the clipboard',
    sent.slice(before).some((m) => m.type === 'clipboard.paste' && m.id === 1));
  check('and the picker goes', picker().hidden === true);
  check('so nothing of it is drawn over the windows any more',
    !(sent.slice(before).filter((m) => m.type === 'shell.overlay').at(-1)
      ?.rects ?? []).some((r) => r.name === 'clipboard'));

  emit({ type: 'shell.command', command: 'clipboard', args: [] });
  before = sent.length;
  click(rows()[0], { target: rows()[0].children[1] });
  check('the cross on a row forgets that entry rather than pasting it',
    sent.slice(before).some((m) => m.type === 'clipboard.forget' && m.id === 3) &&
    !sent.slice(before).some((m) => m.type === 'clipboard.paste'));
  check('and forgetting one leaves the picker open', picker().hidden === false);

  before = sent.length;
  const footer = dialog().children[1];
  click(footer.children[0]);
  check('and forgetting everything names no entry at all',
    sent.slice(before).some((m) => m.type === 'clipboard.forget' &&
      m.id === undefined));

  emit({ type: 'clipboard.history', entries: [] });
  check('an emptied history redraws the open picker',
    rows().length === 0);

  before = sent.length;
  fire('click');
  check('a click that missed takes the picker down', picker().hidden === true);
  emit({ type: 'shell.command', command: 'clipboard', args: [] });
  emit({ type: 'shell.command', command: 'clipboard', args: [] });
  check('and the binding closes it again when it is already open',
    picker().hidden === true);
}

/* The launcher. The compositor scans the .desktop directories — the page
 * cannot read XDG_DATA_DIRS — and sends back the rows to draw; this file
 * draws them, sends the filter on every keystroke, and sends back the id of
 * the row that was chosen. What is checked here is that the page asks for the
 * keyboard when it opens, draws the list it is given, and gives the keyboard
 * back when it goes away. */
{
  const picker = () => globalThis.__shell.launcherEl;
  const dialog = () => picker().children[0];
  const input = () => dialog()?.children[0];
  const list = () => dialog()?.children[1];
  const rows = () => (list()?.children ?? [])
    .filter((el) => el._classes.has('launcher-row'));
  const click = (el, event = {}) => (el.listeners.click ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {}, ...event }));
  const fire = (type) => (documentListeners[type] ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));
  const key = (el, k) => (el.listeners.keydown ?? [])
    .forEach((fn) => fn({ key: k, preventDefault() {}, stopPropagation() {} }));
  const apps = [
    { id: 0, name: 'Firefox', icon: 'data:image/png;base64,x', detail: 'web, browser' },
    { id: 1, name: 'Terminal' },
    { id: 2, name: 'htop', detail: 'console' },
  ];

  /* The window that has the keyboard before the picker takes it. */
  emit({ type: 'view.added', id: 7, title: 'Work', app_id: 'work',
    output: 'DP-1', min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });
  emit({ type: 'view.focused', id: 7 });

  emit({ type: 'launcher.list', apps, generation: 1 });
  /* Kept, but nothing drawn: the message arrives on every keystroke, and
     drawing a picker nobody opened would be a composited frame of the whole
     desktop per keystroke. */
  check('the list is kept but not drawn until it is asked for',
    picker().children.length === 0);

  let before = sent.length;
  emit({ type: 'shell.command', command: 'launcher', args: [] });
  check('the binding opens the picker', picker().hidden === false);
  check('and it asks the compositor for the list',
    sent.slice(before).some((m) => m.type === 'launcher.query' &&
      m.filter === undefined));
  check('and it takes the keyboard, remembering who had it',
    sent.slice(before).some((m) => m.type === 'shell.focus'));

  before = sent.length;
  emit({ type: 'launcher.list', apps, generation: 2 });
  check('every application is a row, in the order the compositor sent it',
    rows().length === 3 &&
    rows()[0].children[1].children[0].textContent === 'Firefox');
  check('a row with no icon draws a letter',
    rows()[1].children[0].textContent === 'T');
  check('a row with no detail is one line',
    rows()[1].children[1].children.length === 1);

  /* Typing in the field is a filter for the compositor, not for the page:
     the list it holds may already be stale, and only the scan knows. */
  const field = input();
  field.value = 'fi';
  before = sent.length;
  (field.listeners.input ?? []).forEach((fn) => fn());
  check('a keystroke sends the filter to the compositor',
    sent.slice(before).some((m) => m.type === 'launcher.query' &&
      m.filter === 'fi'));

  before = sent.length;
  key(field, 'ArrowDown');
  check('the highlight steps without a round trip',
    rows()[1]._classes.has('kbd-here'));
  /* Said as well as painted. A class is not in the accessibility tree, so a
     reader following the field's own list has nothing to follow unless the
     row is marked. */
  check('and the row the keyboard is on says so to a reader',
    rows()[1].getAttribute('aria-selected') === 'true' &&
    rows()[0].getAttribute('aria-selected') === 'false');
  key(field, 'ArrowDown');
  key(field, 'ArrowDown');
  check('and it wraps at the end', rows()[0]._classes.has('kbd-here'));
  key(field, 'End');
  check('End goes to the last row', rows()[2]._classes.has('kbd-here'));
  key(field, 'Home');
  check('and Home back to the first', rows()[0]._classes.has('kbd-here'));

  before = sent.length;
  key(field, 'Enter');
  check('launching asks the compositor to start the highlighted row',
    sent.slice(before).some((m) => m.type === 'launcher.launch' && m.id === 0));
  check('with the generation of the list the row was drawn from',
    sent.slice(before).some((m) => m.type === 'launcher.launch' &&
      m.id === 0 && m.generation === 2));
  check('and the picker goes', picker().hidden === true);
  check('giving the keyboard back to the window that had it',
    sent.slice(before).some((m) => m.type === 'view.focus' && m.id === 7));
  check('so nothing of it is drawn over the windows any more',
    !(sent.slice(before).filter((m) => m.type === 'shell.overlay').at(-1)
      ?.rects ?? []).some((r) => r.name === 'launcher'));

  emit({ type: 'shell.command', command: 'launcher', args: [] });
  check('reopening re-shows the last list rather than a blank',
    rows().length === 3);
  before = sent.length;
  click(rows()[2]);
  check('a click on a row launches that row, whichever is highlighted',
    sent.slice(before).some((m) => m.type === 'launcher.launch' && m.id === 2 &&
      m.generation === 2));
  check('and the picker goes', picker().hidden === true);

  emit({ type: 'shell.command', command: 'launcher', args: [] });
  before = sent.length;
  key(input(), 'Escape');
  check('Escape takes the picker down without launching',
    picker().hidden === true &&
    !sent.slice(before).some((m) => m.type === 'launcher.launch'));

  emit({ type: 'shell.command', command: 'launcher', args: [] });
  before = sent.length;
  fire('click');
  check('a click that missed takes the picker down', picker().hidden === true);
  emit({ type: 'shell.command', command: 'launcher', args: [] });
  emit({ type: 'shell.command', command: 'launcher', args: [] });
  check('and the binding closes it again when it is already open',
    picker().hidden === true);

  /* The window the test focused goes away with the test: a view left in the
     tree is a window the next section's counts do not expect. */
  emit({ type: 'view.removed', id: 7 });
}

/* The notification centre. A popup is a moment; this is the record of it, kept
 * by the compositor because the shell is a page that restarts and reloads.
 * What is checked here is that the page draws the list it is given, sends the
 * two verbs back, and does not throw away what it cannot act on. */
{
  const centre = () => globalThis.__shell.notificationCentreEl;
  const dialog = () => centre().children[0];
  const rows = () => (dialog()?.children[0]?.children ?? [])
    .filter((el) => el._classes.has('notification-centre-row'));
  const click = (el, event = {}) => (el.listeners.click ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {}, ...event }));
  const fire = (type) => (documentListeners[type] ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));
  const now = Math.floor(Date.now() / 1000);

  emit({ type: 'notification.history', entries: [
    { id: 3, app_name: 'chat', summary: 'newest', body: 'a message',
      urgency: 1, timeout: -1, actions: [], at: now - 30 },
    { id: 2, app_name: 'mail', summary: 'middle', body: 'another',
      urgency: 1, timeout: -1,
      actions: [{ key: 'default', label: 'Open' }, { key: 'read', label: 'Mark read' }],
      at: now - 7200 },
    { id: 1, app_name: 'backup', summary: 'oldest', body: '', urgency: 0,
      timeout: -1, actions: [], at: 0 },
  ] });
  check('the history is kept but not drawn until the centre is asked for',
    centre().children.length === 0);

  let before = sent.length;
  emit({ type: 'shell.command', command: 'notifications', args: [] });
  check('the binding opens the centre', centre().hidden === false);
  check('and it asks the compositor for the history now',
    sent.slice(before).some((m) => m.type === 'notification.list'));
  check('every notification is a row, newest first', rows().length === 3 &&
    rows()[0].children[1].textContent === 'newest');

  /* The body is drawn in full rather than on one line: a centre that shows
     the first line answers "was there something?" and not "what was it?". */
  check('the body is drawn under the summary',
    rows()[0].children[2].textContent === 'a message');

  before = sent.length;
  click(rows()[0].children[0].children[2]);
  check('the cross on a row forgets that notification',
    sent.slice(before).some((m) => m.type === 'notification.forget' && m.id === 3));
  check('and forgetting one leaves the centre open', centre().hidden === false);

  before = sent.length;
  const buttons = rows()[1].children[3];
  click(buttons.children[0]);
  check('an action button sends the same message the popup sends',
    sent.slice(before).some((m) => m.type === 'notification.action' &&
      m.id === 2 && m.action === 'read'));

  before = sent.length;
  click(rows()[1]);
  check('and clicking a row with a default action invokes it',
    sent.slice(before).some((m) => m.type === 'notification.action' &&
      m.id === 2 && m.action === 'default'));

  before = sent.length;
  click(rows()[2]);
  check('while a row with no default action does nothing at all',
    !sent.slice(before).some((m) => m.type === 'notification.action'));

  before = sent.length;
  const footer = dialog().children[1];
  click(footer.children[0]);
  check('clearing names no notification at all',
    sent.slice(before).some((m) => m.type === 'notification.forget' &&
      m.id === undefined));

  emit({ type: 'notification.history', entries: [] });
  check('an emptied history redraws the open centre', rows().length === 0);

  before = sent.length;
  fire('click');
  check('a click that missed takes the centre down', centre().hidden === true);
  check('so nothing of it is drawn over the windows any more',
    !(sent.slice(before).filter((m) => m.type === 'shell.overlay').at(-1)
      ?.rects ?? []).some((r) => r.name === 'notifications-centre'));

  emit({ type: 'shell.command', command: 'notifications', args: [] });
  emit({ type: 'shell.command', command: 'notifications', args: [] });
  check('and the binding closes it again when it is already open',
    centre().hidden === true);

  /* The ages. Seconds are not drawn — eleven seconds ago and forty are the
     same thing to somebody reading a list — and an unstamped notification
     draws as nothing rather than as 1970. */
  const age = globalThis.__shell.notificationAgeForTest;
  const stamp = 1_700_000_000;
  check('a notification from seconds ago is "just now"',
    age(stamp, stamp * 1000 + 30_000) === 'just now');
  check('minutes are minutes', age(stamp, stamp * 1000 + 5 * 60_000) === '5m ago');
  check('hours are hours', age(stamp, stamp * 1000 + 3 * 3600_000) === '3h ago');
  check('and anything older than a day is a date rather than arithmetic',
    !/ago$/.test(age(stamp, stamp * 1000 + 48 * 3600_000)));
  check('an unstamped notification says nothing about when it arrived',
    age(0) === '');
}

/* The clock's format, and the calendar under it.
 *
 * The formatting is the one part of the bar that is a pure function of a
 * moment and a config: everything else the bar draws is a number the
 * compositor sampled, and the assertion would be that the test agrees with
 * itself. So it is checked directly, against several locales — which is the
 * whole point of the change, since what it replaced passed the literal
 * 'en-US' and assembled the hour out of getHours().
 *
 * A locale-specific assertion is skipped where the engine running the tests
 * has no data for that locale. Node ships full ICU and does; a trimmed build
 * would otherwise fail here for a reason that has nothing to do with the
 * shell, and the shell's own answer in that case — the English fallback — is
 * checked separately below. */
{
  const shell = globalThis.__shell;
  const clockOf = (config, when) => {
    shell.applyClockForTest(config);
    return shell.clockTextForTest(when);
  };
  /* A Saturday afternoon, so both halves of the twelve-hour question have an
     answer worth reading: 14:05 is 2:05 PM. */
  const when = new Date(2026, 7, 22, 14, 5, 9);
  const hasGerman = (() => {
    try {
      return new Intl.DateTimeFormat('de-DE', { month: 'long' })
        .format(when).startsWith('Aug');
    } catch (e) {
      return false;
    }
  })();

  const shipped = clockOf(null, when);
  check('with no config the clock still draws its glyph and the day',
    shipped.startsWith('󰥔 ') && shipped.includes('22'));
  check('and the minute it was given, whichever way the locale writes the hour',
    /\b05\b/.test(shipped));

  /* The locale decides the order and the separator as well as the names, which
     is why the date and the time are one Intl call rather than two joined with
     a comma here. */
  const german = clockOf({ locale: 'de-DE', hour12: false }, when);
  if (hasGerman) {
    check('a locale draws that locale\'s month', german.includes('Aug'));
  }
  check('and a 24-hour desk gets 14:05, not 2:05',
    german.includes('14:05') && !/[AP]M/.test(german));

  const american = clockOf({ locale: 'en-US', hour12: true }, when);
  check('a 12-hour desk gets the afternoon said as one',
    /2:05/.test(american) && /PM/i.test(american));
  check('and the same desk asked for 24 hours gets 14:05',
    clockOf({ locale: 'en-US', hour12: false }, when).includes('14:05'));

  /* hour12 absent is the locale's own answer, which is the case somebody who
     sets nothing but a locale is asking for. */
  check('an hour nobody chose is the hour the locale writes',
    clockOf({ locale: 'en-GB' }, when).includes('14:05')
    && /PM/i.test(clockOf({ locale: 'en-US' }, when)));

  /* A tag Intl cannot parse throws a RangeError, and this runs from a timer
     once a second: unguarded, one bad character in a config file is a clock
     that stops and takes every output after the first with it. */
  const nonsense = clockOf({ locale: 'not a tag!' }, when);
  check('a language tag the engine cannot parse still draws a clock',
    nonsense.startsWith('󰥔 ') && nonsense.includes('22'));

  /* The format string. A template is the whole module — no glyph is added,
     because adding one would leave no way to ask for a clock without it. */
  check('a format string is expanded, and expanded exactly',
    clockOf({ locale: 'de-DE', format: '%Y-%m-%d %H:%M:%S' }, when)
      === '2026-08-22 14:05:09');
  check('and it is the whole module: no glyph nobody asked for',
    !clockOf({ format: '%H:%M' }, when).includes('󰥔'));
  check('%I and %p are the twelve-hour pair',
    /^02:05 PM$/.test(clockOf({ locale: 'en-US', format: '%I:%M %p' }, when)));
  check('%e and %k pad with spaces where %d and %H pad with zeroes',
    clockOf({ format: '%e|%d' }, new Date(2026, 7, 3, 4, 5)) === ' 3|03'
    && clockOf({ format: '%k|%H' }, new Date(2026, 7, 3, 4, 5)) === ' 4|04');
  check('%F, %T and %j are the compounds they are everywhere else',
    clockOf({ format: '%F %T %j' }, when) === '2026-08-22 14:05:09 234');
  if (hasGerman) {
    check('the named conversions come from the locale, not from a table here',
      clockOf({ locale: 'de-DE', format: '%A %B' }, when) === 'Samstag August');
  }
  check('a percent is a percent and an unknown conversion is left to be seen',
    clockOf({ format: '100%% %Q' }, when) === '100% %Q');

  /* Which day the week starts on is the locale's, not this file's: en-US and
     en-GB are the same language and disagree about it, so nothing short of
     asking about the region can be right. */
  shell.applyClockForTest({ locale: 'en-US' });
  check('a US week starts on Sunday', shell.calendarFirstDayForTest() === 0);
  shell.applyClockForTest({ locale: 'en-GB' });
  check('and a British one on Monday, in the same language',
    shell.calendarFirstDayForTest() === 1);
  shell.applyClockForTest({ locale: 'de-DE' });
  check('as does a German one', shell.calendarFirstDayForTest() === 1);
  shell.applyClockForTest({ locale: 'ar-EG' });
  check('and a week can start on a Saturday, which two answers cannot say',
    shell.calendarFirstDayForTest() === 6);

  /* The table behind all of that, for the engine with no `Intl.Locale` to ask:
     three answers, not two, and the language subtag is not a region — "en"
     uppercased is a two-letter string, and reading it as one is how en-US came
     back as Monday. */
  check('the fallback table knows the three answers apart',
    calendarFirstDayOfRegion('US') === 0
    && calendarFirstDayOfRegion('EG') === 6
    && calendarFirstDayOfRegion('FR') === 1
    && calendarFirstDayOfRegion('EN') === 1);

  /* --- the grid ------------------------------------------------------- */

  /* On a locale whose digits and month names this file can compare with, since
     what the checks below are about is the grid rather than the names in it —
     the names are the section above. */
  shell.applyClockForTest({ locale: 'en-GB' });

  const calendar = () => shell.calendarEl;
  const panel = () => calendar().children[0];
  const cells = () => panel()?.children[1]?.children ?? [];
  const dayCells = () => [...cells()].filter((el) => el._classes.has('calendar-day'));
  const weekdayCells = () =>
    [...cells()].filter((el) => el._classes.has('calendar-weekday'));
  const title = () => panel()?.children[0]?.children[1]?.textContent;
  const arrow = (i) => panel().children[0].children[i];
  const todayButton = () => panel().children[2].children[0];
  const clickEl = (el) => (el.listeners.click ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));
  const fire = (type) => (documentListeners[type] ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));
  const overlayRects = (from) => sent.slice(from)
    .filter((m) => m.type === 'shell.overlay').at(-1)?.rects ?? null;

  const host = shell.outputs.get(shell.activeOutput);
  const clockEl = host.modules.clock;
  /* Where a clock really sits: the right-hand end of the bar. The stub answers
     every measurement with the whole screen otherwise, which would make the
     clamp below untestable — a panel hanging off a 1920-wide clock is a panel
     hanging off nothing in particular. */
  clockEl.__rect = { left: 1700, top: 4, width: 120, height: 24 };

  let before = sent.length;
  clickEl(clockEl);
  check('clicking the clock opens the calendar', calendar().hidden === false);
  check('under the clock that was clicked, not over the middle of the desk',
    calendar().style.top === '32px' && calendar().style.left === '1641px');
  check('and the compositor is told where it is, or it is behind the windows',
    (overlayRects(before) ?? []).length > 0);

  check('a week of headings and six weeks of days, always the same six',
    weekdayCells().length === 7 && dayCells().length === 42);
  check('the week starts on the day the locale starts it on',
    weekdayCells()[0].textContent === calendarWeekdayName(calendarFirstDay()));
  const today = new Date();
  const marked = dayCells().filter((el) => el._classes.has('today'));
  check('exactly one day is marked as today',
    marked.length === 1 && marked[0].textContent === String(today.getDate()));
  check('and it is a day of this month rather than a neighbour\'s',
    !marked[0]._classes.has('adjacent'));
  check('the days either side of the month are drawn, dimmed',
    dayCells().some((el) => el._classes.has('adjacent')));

  const thisMonth = title();
  clickEl(arrow(2));
  check('the next arrow steps the month', title() !== thisMonth);
  check('and nothing is marked on a month that does not hold today',
    dayCells().filter((el) => el._classes.has('today')).length === 0);
  check('with the panel still where it was: paging must not move it',
    calendar().style.top === '32px' && calendar().style.left === '1641px');

  clickEl(arrow(0));
  check('and the previous arrow steps back to it', title() === thisMonth);

  /* Twelve steps forward is a year, which is the arithmetic that goes wrong
     when a month is stepped by adding one to a number rather than by asking
     Date for the answer. */
  for (let i = 0; i < 12; i++) clickEl(arrow(2));
  check('a year forward is the same month again, in a different year',
    title() !== thisMonth
    && title().replace(/\d+/, '') === thisMonth.replace(/\d+/, ''));
  clickEl(todayButton());
  check('and today\'s date, which is also the way back, is the way back',
    title() === thisMonth
    && dayCells().filter((el) => el._classes.has('today')).length === 1);

  before = sent.length;
  clickEl(clockEl);
  check('clicking the clock again takes it down', calendar().hidden === true);
  check('and the rectangle over the windows goes with it',
    (overlayRects(before) ?? []).length === 0);

  clickEl(clockEl);
  fire('click');
  check('a click that missed the calendar takes it down too',
    calendar().hidden === true);

  clickEl(clockEl);
  clickEl(panel());
  check('but a click on the calendar itself does not: it is mostly text',
    calendar().hidden === false);
  fire('click');

  /* Reopening starts on today rather than where it was left, which is the
     difference between a glance and something that has to be read. */
  clickEl(clockEl);
  clickEl(arrow(2));
  clickEl(clockEl);
  clickEl(clockEl);
  check('and it always reopens on this month, not on the one paged to',
    title() === thisMonth);
  fire('click');

  /* The keyboard's way in, for a desk with no pointer on the bar — the pair
     the network and power pickers each have. */
  emit({ type: 'shell.command', command: 'calendar', args: [] });
  check('the shell verb opens it as well', calendar().hidden === false);
  emit({ type: 'shell.command', command: 'calendar', args: [] });
  check('and closes it again', calendar().hidden === true);

  /* And the bar going away takes it with it. Under `bar: auto` that happens a
     second after it was opened — the bar is only up while Mod4 is held — and a
     dropdown pointing at a bar nobody can see is a rectangle the compositor
     goes on drawing over the windows. Driven here with the per-output toggle,
     which is the same relayout by a different route. */
  clickEl(clockEl);
  emit({ type: 'shell.command', command: 'bar.toggle', args: [] });
  check('hiding the bar takes the calendar hanging off it down',
    calendar().hidden === true);
  emit({ type: 'shell.command', command: 'bar.toggle', args: [] });
  check('and the bar comes back without it', calendar().hidden === true);

  /* The config path, rather than the exported function: the block has to
     survive the compositor's own message, and `clock` absent has to be the
     shipped behaviour rather than an error. */
  emit({ type: 'config', layout: mode,
    clock: { locale: 'de-DE', hour12: false, format: '%d.%m.%Y %H:%M' } });
  check('the config message carries the clock block to the page',
    shell.clockTextForTest(when) === '22.08.2026 14:05');
  emit({ type: 'config', layout: mode });
  check('and a config without one goes back to the shipped shape',
    shell.clockTextForTest(when).startsWith('󰥔 '));

  delete clockEl.__rect;
}

/* Placement on a two-monitor desk. The shell is one page spanning the whole
 * layout — two 2560x1440 monitors are one 5120x1440 canvas to it — so a
 * picker centred at 50%/50% of the page lands on the seam between two
 * monitors rather than in the middle of either. `#clipboard` is instead
 * docked over the active output's own rect from renderClipboard, the same
 * way `#osk` is docked from renderOsk, and this is the one thing the harness
 * can actually observe about that without a real layout engine: the inline
 * style renderClipboard set from `output.rect`. */
{
  const picker = () => globalThis.__shell.clipboardEl;

  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 2560, height: 1440,
      usable_x: 0, usable_y: 30, usable_width: 2560, usable_height: 1410,
      scale: 1, transform: 'normal', modes: [], enabled: true },
    { name: 'DP-2', x: 2560, y: 0, width: 2560, height: 1440,
      usable_x: 2560, usable_y: 30, usable_width: 2560, usable_height: 1410,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });
  emit({ type: 'shell.command', command: 'output.focus', args: ['DP-2'] });

  emit({ type: 'shell.command', command: 'clipboard', args: [] });
  check('the docking box sits over the active output, not the middle of the page',
    picker().style.left === '2560px' && picker().style.top === '0px' &&
    picker().style.width === '2560px' && picker().style.height === '1440px');
  emit({ type: 'shell.command', command: 'clipboard', args: [] });

  /* Back to the single-output desk every other test in this file assumes. */
  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
      usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });
}

/* The two radio pickers. NetworkManager and BlueZ are on the system bus, which
 * the page cannot reach; the compositor reads them and sends a snapshot, and
 * everything here is what the shell does with one. What is worth checking is
 * that a row sends the verb the compositor accepts and nothing else — a picker
 * that names an access point by object path, or asks to connect to a network
 * it should be asking for a passphrase for, is a picker that does nothing at
 * all on a real desktop. */
{
  const picker = () => globalThis.__shell.networkEl;
  /* The docking box `picker()` names to the compositor is only ever the
     output-sized frame the dialog is centred in — see renderNetworkPicker's
     own comment for why. Everything actually drawn is one level in, inside
     the `.radio-dialog` that box holds. */
  const dialog = () => picker().children[0];
  const rows = () => (dialog()?.children[1]?.children ?? [])
    .map((item) => item.children[0]);
  const click = (el, event = {}) => (el.listeners.click ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {}, ...event }));
  const key = (el, k) => (el.listeners.keydown ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {}, key: k }));
  const fire = (type) => (documentListeners[type] ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));

  const snapshot = (extra = {}) => ({
    type: 'network.update',
    available: true,
    wireless: true,
    enabled: true,
    state: 'connected',
    ssid: 'kitchen',
    access_points: [
      { ssid: 'kitchen', strength: 88, security: 'wpa2', known: true, active: true },
      { ssid: 'office', strength: 70, security: 'wpa2', known: true, active: false },
      { ssid: 'neighbour', strength: 61, security: 'wpa2', known: false, active: false },
      { ssid: 'cafe', strength: 40, security: '', known: false, active: false },
      { ssid: 'campus', strength: 30, security: 'enterprise', known: false, active: false },
    ],
    ...extra,
  });

  emit(snapshot());
  check('a snapshot is kept but nothing is drawn until the picker is asked for',
    picker().children.length === 0);

  let before = sent.length;
  emit({ type: 'shell.command', command: 'network', args: [] });
  check('the binding opens the picker', picker().hidden === false);
  /* Absent `enabled` is the open: a scan is the radio transmitting, so the
     compositor does not start one until something says it is being looked
     at. */
  check('and says it is open, which is what starts the scan',
    sent.slice(before).some((m) => m.type === 'network.scan' &&
      m.enabled === undefined));
  check('every access point the compositor listed is a row',
    rows().length === 5);
  check('the one in use says so',
    rows()[0]._classes.has('active') &&
    rows()[0].children[2].textContent === 'connected');
  check('a network with a saved connection is marked as one',
    rows()[1].children[2].textContent === 'saved');
  check('an open one says that rather than nothing',
    rows()[3].children[2].textContent === 'open');
  /* The one word that changes what the row does: a passphrase is the wrong
     question for an enterprise network, and the picker has to know before it
     offers a box. */
  check('and an enterprise one is named as what it is',
    rows()[4].children[2].textContent === 'enterprise');

  before = sent.length;
  click(rows()[0]);
  check('clicking the network in use leaves it',
    sent.slice(before).some((m) => m.type === 'network.disconnect'));

  before = sent.length;
  click(rows()[1]);
  check('a saved network is joined with no passphrase asked for',
    sent.slice(before).some((m) => m.type === 'network.connect' &&
      m.ssid === 'office' && m.passphrase === undefined));

  before = sent.length;
  click(rows()[3]);
  check('and so is an open one',
    sent.slice(before).some((m) => m.type === 'network.connect' &&
      m.ssid === 'cafe' && m.passphrase === undefined));

  before = sent.length;
  click(rows()[4]);
  check('an enterprise network is not offered a passphrase box it cannot use',
    !sent.slice(before).some((m) => m.type === 'network.connect') &&
    dialog().children[2]?._classes.has('radio-error') === true);

  /* The passphrase box, which is the one part of this shell that receives real
     typed text. The out-of-process shell is a Wayland client, so keys reach it
     the way they reach any client — once something moves the keyboard, which
     is what `shell.focus` is for. */
  emit({ type: 'view.focused', id: 3 });
  emit(snapshot());
  before = sent.length;
  click(rows()[2]);
  check('an unknown secured network asks for a passphrase rather than joining',
    !sent.slice(before).some((m) => m.type === 'network.connect'));

  const box = () => dialog().children[1].children[2].children[1];
  check('the box is under the row it belongs to',
    box()?._classes.has('radio-passphrase') === true);
  const input = () => box().children[0];
  check('and it is a password field, not a plain one',
    input().type === 'password');

  before = sent.length;
  key(input(), 'Enter');
  check('an empty passphrase sends nothing at all',
    !sent.slice(before).some((m) => m.type === 'network.connect'));

  /* A snapshot arrives several times a second while the radio scans, and a
     redraw rebuilds the picker from nothing — which would throw away a
     half-typed passphrase on a message nobody sent. */
  input().value = 'part';
  emit(snapshot({ scanning: true }));
  check('a snapshot arriving mid-passphrase does not rebuild the box under it',
    input().value === 'part');

  input().value = 'hunter2';
  before = sent.length;
  key(input(), 'Enter');
  check('Enter joins the network with what was typed',
    sent.slice(before).some((m) => m.type === 'network.connect' &&
      m.ssid === 'neighbour' && m.passphrase === 'hunter2'));
  /* Joining takes seconds and fails often — a mistyped passphrase is the usual
     reason — and a picker that closed on submit would take the answer with
     it. */
  check('and the picker stays up to show what came of it',
    picker().hidden === false);
  /* The keyboard stays with it. It used to go back to the window here, which
     was right when the box was the only part of this picker a keyboard could
     reach; now the rows can be steered, and handing the keyboard away while
     the list is still on screen would put it back out of reach. */
  check('and keeps the keyboard, because the list is still there to steer',
    !sent.slice(before).some((m) => m.type === 'view.focus' && m.id === 3));

  click(rows()[2]);
  before = sent.length;
  key(input(), 'Escape');
  check('Escape abandons the box without joining anything',
    !sent.slice(before).some((m) => m.type === 'network.connect') &&
    dialog().children[1].children[2].children.length === 1);
  check('and leaves the picker up rather than taking it down with the box',
    picker().hidden === false);

  before = sent.length;
  click(dialog().children[0].children[1]);
  /* Absent `enabled` rather than the opposite of what was drawn: the snapshot
     the picker drew from may already be stale, and a computed opposite would
     turn the radio back on a moment after somebody turned it off. */
  check('the header switch toggles the radio without saying which way',
    sent.slice(before).some((m) => m.type === 'network.radio' &&
      m.enabled === undefined));

  emit(snapshot({ error: 'Secrets were required, but not provided' }));
  check('and what NetworkManager said about a refusal is drawn on the picker',
    dialog().children.at(-1).textContent.includes('Secrets were required'));

  before = sent.length;
  fire('click');
  check('a click that missed takes the picker down', picker().hidden === true);
  check('and stops the scan, which is a radio nobody is looking at',
    sent.slice(before).some((m) => m.type === 'network.scan' &&
      m.enabled === false));

  emit({ type: 'network.update', available: false });
  emit({ type: 'shell.command', command: 'network', args: [] });
  check('no NetworkManager is said rather than drawn as an empty list',
    dialog().children[1].children[0].textContent.includes('NetworkManager'));
  emit({ type: 'network.update', available: true, wireless: true, enabled: false });
  check('and a radio that is switched off says that instead',
    dialog().children[1].children[0].textContent.includes('switched off'));
  emit({ type: 'shell.command', command: 'network', args: [] });
}

/* Placement on a two-monitor desk — see the clipboard's own placement test
 * above for why 50%/50% is the wrong centre and what the harness can check
 * about the fix instead. This is the bug as it was actually reported: the
 * Wi-Fi picker opened from the bar's `net` module landing between two
 * monitors rather than on the one that was clicked. */
{
  const picker = () => globalThis.__shell.networkEl;

  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 2560, height: 1440,
      usable_x: 0, usable_y: 30, usable_width: 2560, usable_height: 1410,
      scale: 1, transform: 'normal', modes: [], enabled: true },
    { name: 'DP-2', x: 2560, y: 0, width: 2560, height: 1440,
      usable_x: 2560, usable_y: 30, usable_width: 2560, usable_height: 1410,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });
  emit({ type: 'shell.command', command: 'output.focus', args: ['DP-2'] });

  emit({ type: 'shell.command', command: 'network', args: [] });
  check('the docking box sits over the active output, not the middle of the page',
    picker().style.left === '2560px' && picker().style.top === '0px' &&
    picker().style.width === '2560px' && picker().style.height === '1440px');
  emit({ type: 'shell.command', command: 'network', args: [] });

  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
      usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });
}

/* The Bluetooth picker, which is the same overlay with different verbs. */
{
  const picker = () => globalThis.__shell.bluetoothEl;
  /* The docking box `picker()` names to the compositor is only ever the
     output-sized frame the dialog is centred in — see renderBluetoothPicker's
     own comment for why. Everything actually drawn is one level in, inside
     the `.radio-dialog` that box holds. */
  const dialog = () => picker().children[0];
  const rows = () => dialog()?.children[1]?.children ?? [];
  const click = (el, event = {}) => (el.listeners.click ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {}, ...event }));
  const fire = (type) => (documentListeners[type] ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));

  emit({ type: 'bluetooth.update', available: true, powered: true,
    discovering: true, adapter: 'thinkpad', devices: [
      { address: 'AA:BB:CC:DD:EE:01', name: 'mouse', icon: 'input-mouse',
        paired: true, trusted: true, connected: true },
      { address: 'AA:BB:CC:DD:EE:02', name: 'headset', icon: 'audio-headset',
        paired: true, trusted: true, connected: false },
      { address: 'AA:BB:CC:DD:EE:03', name: 'speaker', icon: '',
        paired: false, trusted: false, connected: false, rssi: -74 },
    ] });
  check('a snapshot is kept but nothing is drawn until the picker is asked for',
    picker().children.length === 0);

  let before = sent.length;
  emit({ type: 'shell.command', command: 'bluetooth', args: [] });
  check('the binding opens the picker', picker().hidden === false);
  check('and starts the discovery, which is the radio transmitting',
    sent.slice(before).some((m) => m.type === 'bluetooth.scan' &&
      m.enabled === undefined));
  check('every device is a row', rows().length === 3);
  check('a device the adapter can hear says how loudly',
    rows()[2].children[2].textContent === '-74 dBm');

  before = sent.length;
  click(rows()[2]);
  /* One verb for a row somebody tapped. The compositor pairs, trusts and
     connects in that order — doing it here would be three messages and an
     order the shell has no reason to know. */
  check('tapping a device that is not connected connects it, by address',
    sent.slice(before).some((m) => m.type === 'bluetooth.device' &&
      m.address === 'AA:BB:CC:DD:EE:03' && m.action === 'connect'));

  before = sent.length;
  click(rows()[0]);
  check('and tapping the one that is disconnects it',
    sent.slice(before).some((m) => m.type === 'bluetooth.device' &&
      m.address === 'AA:BB:CC:DD:EE:01' && m.action === 'disconnect'));

  before = sent.length;
  click(rows()[1], { target: rows()[1].children[3] });
  check('the cross on a paired device forgets it rather than connecting',
    sent.slice(before).some((m) => m.type === 'bluetooth.device' &&
      m.address === 'AA:BB:CC:DD:EE:02' && m.action === 'forget') &&
    !sent.slice(before).some((m) => m.action === 'connect'));

  before = sent.length;
  click(dialog().children[0].children[1]);
  check('the header switch toggles the adapter without saying which way',
    sent.slice(before).some((m) => m.type === 'bluetooth.power' &&
      m.enabled === undefined));

  before = sent.length;
  fire('click');
  check('a click that missed takes the picker down', picker().hidden === true);
  check('and stops the discovery with it',
    sent.slice(before).some((m) => m.type === 'bluetooth.scan' &&
      m.enabled === false));

  emit({ type: 'bluetooth.update', available: false });
  emit({ type: 'shell.command', command: 'bluetooth', args: [] });
  check('no adapter is said rather than drawn as an empty list',
    dialog().children[1].children[0].textContent.includes('No Bluetooth'));
  emit({ type: 'shell.command', command: 'bluetooth', args: [] });
}

/* Placement on a two-monitor desk — see the clipboard's own placement test
 * above for why 50%/50% is the wrong centre and what the harness can check
 * about the fix instead. */
{
  const picker = () => globalThis.__shell.bluetoothEl;

  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 2560, height: 1440,
      usable_x: 0, usable_y: 30, usable_width: 2560, usable_height: 1410,
      scale: 1, transform: 'normal', modes: [], enabled: true },
    { name: 'DP-2', x: 2560, y: 0, width: 2560, height: 1440,
      usable_x: 2560, usable_y: 30, usable_width: 2560, usable_height: 1410,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });
  emit({ type: 'shell.command', command: 'output.focus', args: ['DP-2'] });

  emit({ type: 'shell.command', command: 'bluetooth', args: [] });
  check('the docking box sits over the active output, not the middle of the page',
    picker().style.left === '2560px' && picker().style.top === '0px' &&
    picker().style.width === '2560px' && picker().style.height === '1440px');
  emit({ type: 'shell.command', command: 'bluetooth', args: [] });

  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
      usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });
}

/* The on-screen keyboard. Unlike every picker above, what it sends is not a
 * fact read off a snapshot but an instruction — press this key — so what is
 * worth checking is that a tap on a drawn key is the keysym `osk.key` says it
 * is, that Shift and Caps Lock change what gets sent rather than only what is
 * drawn, and that the two independent reasons it can be on screen — the
 * compositor's `osk.wanted` and a person's own Mod4+Shift+k — behave the way
 * their own comments in osk.js say they do. */
{
  const el = () => globalThis.__shell.oskEl;
  const panel = () => el().children[0];
  const row = (i) => panel().children[i];
  const press = (button) => (button.listeners.pointerdown ?? [])
    .forEach((fn) => fn({ preventDefault() {}, pointerId: 1 }));
  const release = (button) => (button.listeners.pointerup ?? [])
    .forEach((fn) => fn({ preventDefault() {}, pointerId: 1 }));
  const tap = (button) => { press(button); release(button); };
  const clickIt = (button) => (button.listeners.click ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));
  const keysOf = (m) => m.filter((x) => x.type === 'osk.key');

  check('the keyboard is not drawn until something asks for it',
    el().children.length === 0);

  let before = sent.length;
  emit({ type: 'osk.wanted', wanted: true });
  check('a text-input becoming enabled brings it up on its own',
    el().hidden === false);
  check('on the letters page, to start', globalThis.__shell.oskPageForTest === 'letters');
  check('the top row is the ten letters of a QWERTY keyboard',
    row(0).children.length === 10 && row(0).children[0].textContent === 'q');
  check('the middle row is nine letters, no Shift or Backspace on it',
    row(1).children.length === 9);
  check('the bottom letter row carries Shift and Backspace either side of seven letters',
    row(2).children.length === 9 &&
    row(2).children[1].textContent === 'z');

  before = sent.length;
  tap(row(0).children[0]);
  check('tapping "q" presses and releases its own keysym, nothing else',
    keysOf(sent.slice(before)).length === 2 &&
    sent[before].keysym === 'q'.codePointAt(0) && sent[before].pressed === true &&
    sent[before + 1].keysym === 'q'.codePointAt(0) && sent[before + 1].pressed === false);

  /* Shift: one tap arms a single capital letter, sent as the base keysym
     wrapped in a real Shift_L rather than as the capital's own keysym — see
     the file banner in osk.js for why sending 'A' directly would not work. */
  clickIt(row(2).children[0]);
  check('one tap on Shift arms the next letter', globalThis.__shell.oskShiftStateForTest === 'upper');
  check('and the keyboard redraws showing capitals',
    row(0).children[0].textContent === 'Q');

  before = sent.length;
  tap(row(0).children[0]);
  const oneShot = keysOf(sent.slice(before));
  check('a capital is a real Shift_L held around the base letter, not the capital\'s own keysym',
    oneShot.length === 4 &&
    oneShot[0].keysym === 0xffe1 && oneShot[0].pressed === true &&
    oneShot[1].keysym === 'q'.codePointAt(0) && oneShot[1].pressed === true &&
    oneShot[2].keysym === 'q'.codePointAt(0) && oneShot[2].pressed === false &&
    oneShot[3].keysym === 0xffe1 && oneShot[3].pressed === false);
  check('and the one-shot is consumed: the next letter is lower case again',
    globalThis.__shell.oskShiftStateForTest === 'lower');

  /* Caps Lock: a double tap on Shift, entered the same way a phone keyboard
     enters it since there is no separate key here to dedicate to it. The
     lock timer is reset well outside the double-tap window first, or this
     block's own first click would read as a double tap of the single tap
     above rather than as the first of a pair of its own. */
  globalThis.__shell.oskShiftTappedAtForTest = -Infinity;
  clickIt(row(2).children[0]);
  clickIt(row(2).children[0]);
  check('a double tap on Shift locks it',
    globalThis.__shell.oskShiftStateForTest === 'caps');

  before = sent.length;
  tap(row(0).children[0]);
  const capped = keysOf(sent.slice(before));
  check('a locked capital sends the base keysym with no Shift wrapped around it — '
    + 'the real Caps Lock modifier is already doing the casing',
    capped.length === 2 && capped[0].keysym === 'q'.codePointAt(0));
  check('and Caps Lock does not consume itself the way the one-shot does',
    globalThis.__shell.oskShiftStateForTest === 'caps');

  clickIt(row(2).children[0]);
  check('a third tap on Shift, not a double tap, leaves Caps Lock',
    globalThis.__shell.oskShiftStateForTest === 'lower');

  /* Symbols: the same base-plus-real-Shift trick, over a different pair of
     rows, so a shifted symbol's own keysym is never sent either. */
  const pageToggle = () => row(3).children[0];
  check('the bottom row opens with the page toggle', pageToggle().textContent === '?123');
  clickIt(pageToggle());
  check('the page toggle switches to symbols', globalThis.__shell.oskPageForTest === 'symbols');
  check('the number row is drawn plain', row(0).children[0].textContent === '1');

  before = sent.length;
  tap(row(0).children[0]);
  check('an unshifted digit is sent as itself, no Shift involved',
    keysOf(sent.slice(before)).length === 2 &&
    sent[before].keysym === '1'.codePointAt(0));

  /* Reset again: without it this tap would land inside the double-tap window
     of the click that left Caps Lock above and lock it right back on rather
     than arming a one-shot Shift. */
  globalThis.__shell.oskShiftTappedAtForTest = -Infinity;
  clickIt(row(2).children[0]);
  check('Shift is drawn on the symbols page too', row(0).children[0].textContent === '!');
  before = sent.length;
  tap(row(0).children[0]);
  const shiftedSymbol = keysOf(sent.slice(before));
  check('a shifted symbol is the digit\'s own base keysym under a real Shift, not "!"\'s own',
    shiftedSymbol[0].keysym === 0xffe1 &&
    shiftedSymbol[1].keysym === '1'.codePointAt(0));
  clickIt(pageToggle());
  check('the toggle reads ABC on the symbols page and switches back',
    globalThis.__shell.oskPageForTest === 'letters');

  /* Backspace, Space, Enter and the arrows: each its own fixed keysym, none
     of them touched by Shift. */
  before = sent.length;
  tap(row(2).children.at(-1));
  check('Backspace sends the BackSpace keysym',
    keysOf(sent.slice(before))[0].keysym === 0xff08);

  const bottomRow = row(3);
  before = sent.length;
  tap(bottomRow.children[2]);
  check('the wide middle key of the bottom row is Space',
    keysOf(sent.slice(before))[0].keysym === 0x0020);
  before = sent.length;
  tap(bottomRow.children.at(-1));
  check('and the last key of it is Enter',
    keysOf(sent.slice(before))[0].keysym === 0xff0d);

  const navRow = row(4);
  before = sent.length;
  tap(navRow.children[1]);
  check('the arrow row sends Left, Up, Down, Right in that order',
    keysOf(sent.slice(before))[0].keysym === 0xff51);

  /* A key held down when the keyboard is hidden out from under it — an
     `osk.wanted: false` arriving mid-press — must still send its release, or
     the client the last tap landed on would be left with that key logically
     down forever. */
  before = sent.length;
  press(row(0).children[0]);
  emit({ type: 'osk.wanted', wanted: false });
  check('hiding the keyboard mid-press releases whatever was held',
    keysOf(sent.slice(before)).some((m) => m.pressed === false));
  check('and the keyboard actually went away', el().hidden === true);

  /* The two independent reasons the keyboard can be up. */
  emit({ type: 'shell.command', command: 'osk', args: [] });
  check('Mod4+Shift+k opens it by hand', el().hidden === false);
  emit({ type: 'osk.wanted', wanted: false });
  check('a manual open outlives the field it was for losing focus, wanted or not',
    el().hidden === false);
  emit({ type: 'shell.command', command: 'osk', args: [] });
  check('the same binding closes a manually-opened keyboard', el().hidden === true);

  emit({ type: 'osk.wanted', wanted: true });
  check('wanted still opens it once nothing is pinning it open', el().hidden === false);
  emit({ type: 'shell.command', command: 'osk', args: [] });
  check('the hide button (the same toggle) dismisses it even though the field is still enabled',
    el().hidden === true);
  /* `osk.wanted` is only ever sent on the edge — see its own doc comment in
     event.rs — so the compositor never repeats a `true` the shell has not
     seen turn `false` first; a dismissal only has to survive until the next
     *change*, which is what this checks. */
  emit({ type: 'osk.wanted', wanted: false });
  emit({ type: 'osk.wanted', wanted: true });
  check('a fresh want — the field losing focus and gaining it again — gets to ask again',
    el().hidden === false);

  emit({ type: 'shell.command', command: 'osk', args: [] });
  emit({ type: 'osk.wanted', wanted: false });
}

/* The `osk` config key: 'auto' (the default), 'manual' and 'off'. Whether
 * `osk.wanted` is ever sent true is the compositor's own decision — see
 * sync_osk_wanted's tests in input.rs, which this file cannot reach — so what
 * is left to check here is the one thing only the shell can enforce: 'off'
 * refuses the chord, and drops a keyboard that was already pinned open the
 * moment the setting reaches it, rather than leaving it up until something
 * else changes. 'manual' is not tested beyond "the chord still works", since
 * as far as this file can see it behaves exactly like 'auto'. */
{
  const el = () => globalThis.__shell.oskEl;
  const config = (osk) => emit({ type: 'config', layout: mode, rules: HARNESS_RULES, osk });

  emit({ type: 'shell.command', command: 'osk', args: [] });
  check('the chord opens the keyboard under the default auto mode', el().hidden === false);
  emit({ type: 'shell.command', command: 'osk', args: [] });
  check('and closes it again', el().hidden === true);

  config('manual');
  emit({ type: 'shell.command', command: 'osk', args: [] });
  check('manual leaves the chord working — only the automatic raise is its own to '
    + 'suppress, and that half already happened compositor-side', el().hidden === false);
  emit({ type: 'shell.command', command: 'osk', args: [] });
  check('and it still closes', el().hidden === true);

  config('off');
  let before = sent.length;
  emit({ type: 'shell.command', command: 'osk', args: [] });
  check('off refuses the chord: nothing is sent for it and the keyboard stays down',
    sent.length === before && el().hidden === true);

  /* A keyboard pinned open by hand under a more permissive setting must not
     survive a reload that turns it off — see applyOskMode's own comment in
     osk.js for why 'off' is the one transition that reaches across a pin. */
  config('auto');
  emit({ type: 'shell.command', command: 'osk', args: [] });
  check('back under auto, the chord opens it again', el().hidden === false);
  before = sent.length;
  config('off');
  check('and turning the setting off closes an already-pinned keyboard immediately',
    el().hidden === true);
  check('by releasing it in the shell alone — nothing is sent for a reload closing it',
    !sent.slice(before).some((m) => m.type === 'osk.key'));

  /* Restored for whatever runs after this block. */
  config('auto');
}

/* Notifications are the compositor's on D-Bus and the shell's on screen, and
 * each one is drawn over the output of the app that sent it. 'test' matches no
 * open window, so it falls back to the active output — the one output there is. */
{
  const strip = () => globalThis.__shell.outputs.get('DP-1').notificationsEl;

  emit({ type: 'notification.add', id: 7, app_name: 'test',
    summary: 'hello', body: 'world', urgency: 1, timeout: 0,
    actions: [{ key: 'reply', label: 'Reply' }] });
  check('a notification arriving is drawn on its output',
    strip().children.length === 1);

  /* Nothing translucent, and nothing transformed. The compositor draws this
     strip over a window by redrawing the page cropped to the rectangle the
     page reported, and what is behind the notification inside that rectangle
     is the wallpaper rather than the window — so a notification that fades is
     a rectangle of desktop background fading in over whatever it covered, and
     one that scales leaves the same background around its edges. Both are
     what closing one used to look like. The element is held here rather than
     read back off the strip because the exit ends by removing it. */
  const leaving = strip().children[0];
  check('an arriving notification leaves nothing inline behind it',
    leaving.style.opacity === '' && leaving.style.height === '');
  check('and is never drawn scaled', leaving.style.scale === ''
    && leaving.style.transform === '');

  /* The entrance is checked at the source as well as at the element, because
     what an arrival leaves behind cannot show what it looked like on the way
     in: `clearProps` takes the inline values off at the end, so a tween that
     faded the whole way and one that never touched opacity end identically.
     What can be stated is that neither tween names either property. */
  {
    const motion = fs.readFileSync(`${shellDir}/motion.js`, 'utf8');
    const tweens = motion
      .slice(motion.indexOf('function animateNotificationIn'),
        motion.indexOf('The screen-share chooser'));
    check('neither notification tween asks for opacity or a transform',
      !/\bopacity\s*:/.test(tweens) && !/\bscale\s*:/.test(tweens)
      && !/\b(x|y)\s*:\s*-?\d/.test(tweens));
  }

  const before = sent.length;
  emit({ type: 'notification.close', id: 7 });
  check('an application withdrawing one sends nothing back',
    !sent.slice(before).some((m) => String(m.type).startsWith('notification')));

  check('and the one leaving is not made translucent either',
    leaving.style.opacity === '');
  check('nor scaled on its way out', leaving.style.scale === ''
    && leaving.style.transform === '');
  /* What it does instead: the box itself goes to nothing, so the rectangle
     the compositor is given and the picture inside it shrink together. */
  check('what collapses is the box', leaving.style.height === 0
    && leaving.style.paddingTop === 0 && leaving.style.borderTopWidth === 0);

  /* One that has been dismissed animates out, and the removal is what the
     animation is handed rather than something that has already happened — so
     the thing worth checking is that the element does eventually go. A
     notification left behind is not a stale animation, it is a rectangle of
     shell composited over a window for the rest of the session. */
  check('and the element goes with it', strip().children.length === 0);
  check('so the strip stops being drawn over the windows',
    !(sent.slice(before).filter((m) => m.type === 'shell.overlay').at(-1)
      ?.rects ?? []).some((r) => r.width > 0 && r.height > 0));

  /* A critical notification never expires on its own, so it must still be
     there after any timer would have run. */
  emit({ type: 'notification.add', id: 8, app_name: 'test',
    summary: 'critical', body: '', urgency: 2, timeout: -1, actions: [] });
  check('a critical notification is kept', true);
  emit({ type: 'notification.close', id: 8 });
}

/* A notification lands on the output where its source window is. A chat client
 * open on the right monitor should not pop its message in the left one's
 * corner, and the reverse. Resolved by app_id; a window placed on one output
 * and a window of a different app on the other mean each notification belongs
 * to a different corner. */
{
  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
      usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
    { name: 'DP-3', x: 1920, y: 0, width: 1920, height: 1080,
      usable_x: 1920, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });

  /* Put a window of each app on its own monitor. `chat` on DP-1, `mail` on
     DP-3 — a notification from either must follow its window, not the output
     that happens to be active. */
  emit({ type: 'shell.command', command: 'output.focus', args: ['DP-1'] });
  emit({ type: 'view.added', id: 71, title: 'chat', app_id: 'chat',
    output: 'DP-1', min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });
  emit({ type: 'shell.command', command: 'output.focus', args: ['DP-3'] });
  emit({ type: 'view.added', id: 72, title: 'mail', app_id: 'mail',
    output: 'DP-3', min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });

  const left = globalThis.__shell.outputs.get('DP-1').notificationsEl;
  const right = globalThis.__shell.outputs.get('DP-3').notificationsEl;

  emit({ type: 'notification.add', id: 80, app_name: 'mail',
    summary: 'you have mail', body: '', urgency: 1, timeout: 0, actions: [] });
  check('a notification follows its window to the right output',
    right.children.length === 1 && left.children.length === 0);

  emit({ type: 'notification.close', id: 80 });

  emit({ type: 'notification.add', id: 81, app_name: 'chat',
    summary: 'hi', body: '', urgency: 1, timeout: 0, actions: [] });
  check('and a different app on the other monitor goes the other way',
    left.children.length === 1 && right.children.length === 0);

  emit({ type: 'notification.close', id: 81 });

  /* A notification from an app with no window at all has no output to claim,
     so it falls back to the one being looked at. */
  emit({ type: 'shell.command', command: 'output.focus', args: ['DP-3'] });
  emit({ type: 'notification.add', id: 82, app_name: 'daemon',
    summary: 'background', body: '', urgency: 1, timeout: 0, actions: [] });
  check('an app with no window falls back to the active output',
    right.children.length === 1 && left.children.length === 0);
  emit({ type: 'notification.close', id: 82 });

  for (const id of [71, 72]) emit({ type: 'view.removed', id });
}

/* A monitor going away while a notification is up on it.
 *
 * This is what a screen coming back from DPMS looks like from here: a
 * DisplayPort connector drops and reconnects, so the shell is handed a layout
 * without that output and then one with it again. The rectangles it reports
 * are per output and are reported by walking the outputs that exist — so
 * whatever the departing one had floating is never revisited, and the
 * compositor goes on drawing that piece of shell over the windows for the rest
 * of the session. That is the grey box on the screen after a resume that no
 * click and no timer will take away. */
{
  const layout = (names) => emit({ type: 'output.layout', outputs: names.map(
    (name, at) => ({ name, x: at * 1920, y: 0, width: 1920, height: 1080,
      usable_x: at * 1920, usable_y: 30, usable_width: 1920,
      usable_height: 1050, scale: 1, transform: 'normal', modes: [],
      enabled: true })) });

  layout(['DP-1', 'DP-3']);
  emit({ type: 'shell.command', command: 'output.focus', args: ['DP-3'] });
  emit({ type: 'notification.add', id: 90, app_name: 'daemon',
    summary: 'still here', body: '', urgency: 1, timeout: 0, actions: [] });

  const left = globalThis.__shell.outputs.get('DP-1').notificationsEl;
  check('a notification is up on the monitor that is about to go',
    globalThis.__shell.outputs.get('DP-3').notificationsEl.children.length === 1);

  const before = sent.length;
  layout(['DP-1']);
  const rects = sent.slice(before)
    .filter((m) => m.type === 'shell.overlay').at(-1)?.rects ?? [];

  /* Still open as far as the application that sent it is concerned, so it
     moves to a screen that is still there rather than being thrown away. */
  check('the notification moves to a monitor that is still there',
    left.children.length === 1);
  check('and only one rectangle is drawn above the windows, not two',
    rects.length === 1);

  /* And dismissing it clears that one too: a rect left behind by the monitor
     that went would survive this and be composited for ever. */
  const dismissed = sent.length;
  emit({ type: 'notification.close', id: 90 });
  const after = sent.slice(dismissed)
    .filter((m) => m.type === 'shell.overlay').at(-1)?.rects ?? [];
  check('so nothing of the shell is left over the windows',
    after.length === 0 && left.children.length === 0);

  layout(['DP-1', 'DP-3']);
}

/* Window rules place a window before it is ever laid out, so it never appears
 * somewhere and jumps. */
{
  const open = (id, app) => emit({ type: 'view.added', id, title: app,
    app_id: app, output: 'DP-1', min_width: 0, min_height: 0,
    floating: false, width: 800, height: 600 });

  open(50, 'pinned-app');
  check('a rule sends a window to its workspace',
    globalThis.__shell.workspaceOfForTest(50) === 6);

  open(51, 'dialogy-thing');
  const rect = globalThis.__shell.floatingForTest(51);
  check('a rule can float a window', rect !== null);
  check('with the rect the rule gave',
    rect?.x === 10 && rect?.width === 300);

  /* An application no rule mentions is untouched. */
  open(52, 'ordinary');
  check('an unmatched window is placed normally',
    globalThis.__shell.floatingForTest(52) === null);

  for (const id of [50, 51, 52]) emit({ type: 'view.removed', id });
  /* Put focus back where the rest of the file expects it. */
  emit({ type: 'view.focused', id: 4 });
}


/* Opening a window fades it in: the compositor cannot be told this in CSS,
 * because the window's contents are a surface the shell does not draw. */
const fades = sent.filter((m) => m.type === 'view.opacity' && m.id === 4);
check('a new window fades in from zero',
  fades.length > 1 && fades[0].opacity === 0);
check('and ends fully opaque',
  fades[fades.length - 1].opacity === 1);

if (mode === 'tiling') {
  emit({ type: 'shell.command', command: 'layout.tabbed', args: [] });
  const shownNow = new Set();
  for (const [id, v] of globalThis.__shell.views) if (!v.el.hidden) shownNow.add(id);
  check('tabbed shows exactly one window', shownNow.size === 1);
  check('the visible one is the focused one', shownNow.has(4));

  emit({ type: 'shell.command', command: 'layout.stacked', args: [] });
  emit({ type: 'shell.command', command: 'layout.toggle', args: [] });
  check('survives tabbed -> stacked -> split', process.exitCode !== 1);

  emit({ type: 'view.focused', id: 2 });
  emit({ type: 'shell.command', command: 'layout.tabbed', args: [] });
  const after = sent.filter((m) => m.type === 'view.layout' && m.id === 2);
  check('focused window stays laid out while tabbed', after.length > 0);

  /* Dynamic arrangements. Structure only: the stub returns one fixed rect for
   * every element, so a measured width would say nothing. What is checked is
   * the shape of the tree the mode built, and that every window survived it —
   * an arrangement that loses a window is the failure that matters. */
  emit({ type: 'shell.command', command: 'layout.toggle', args: [] });
  const root = () => globalThis.__shell.workspaces.get(1);
  const ids = () => globalThis.__shell.dynamicOrderForTest(root());
  const opened = ids().length;

  emit({ type: 'shell.command', command: 'layout.mode', args: ['master-stack'] });
  check('master-stack puts one window beside the rest',
    root().children.length === 2
    && root().children[0].type === 'leaf'
    && root().children[1].type === 'split');
  check('the stack runs down the side',
    root().dir === 'horizontal' && root().children[1].dir === 'vertical');
  check('and no window was lost', ids().length === opened);

  emit({ type: 'shell.command', command: 'layout.mode', args: ['spiral'] });
  {
    /* Each window takes half of what is left, turning ninety degrees each
       time — so directions must alternate all the way down the nest. */
    let node = root();
    let alternated = true;
    let depth = 0;
    while (node && node.type === 'split' && node.children.length === 2) {
      const inner = node.children[1];
      if (inner && inner.type === 'split') {
        if (inner.dir === node.dir) alternated = false;
        depth++;
      }
      node = inner;
    }
    check('spiral alternates direction at every level', alternated && depth > 0);
    check('spiral keeps every window', ids().length === opened);
  }

  emit({ type: 'shell.command', command: 'layout.mode', args: ['bsp'] });
  check('bsp cuts a wide screen across first', root().dir === 'horizontal');
  check('bsp keeps every window', ids().length === opened);

  {
    /* And the other way up, which is the whole of what separates bsp from the
       spiral: it answers the region in front of it rather than following a
       fixed turn.
     *
     * This is the test the layout did not have, and its absence is why the
     * bug it now covers survived. workspaceAspect() read `output.width` and
     * `output.height`, which no output record has: syncOutputs stores them
     * under `output.rect`. Both comparisons were `undefined > 0`, so every cut
     * bsp ever made came from the 16:9 guess meant as the fallback for a
     * workspace that is not on screen. On a 16:9 monitor that is invisible —
     * bspPick compares w against h and nothing else, so the guess and the
     * truth agree at every level — and the existing check above passes either
     * way. Turning the screen on its end is what tells them apart. */
    const area = globalThis.__shell.outputs
      .get(globalThis.__shell.activeOutput).windowsEl;
    const landscape = area.__rect;
    area.__rect = { left: 0, top: 0, width: 1080, height: 1920 };
    emit({ type: 'shell.command', command: 'layout.mode', args: ['manual'] });
    emit({ type: 'shell.command', command: 'layout.mode', args: ['bsp'] });
    check('and cuts a tall screen down it instead',
      root().dir === 'vertical');
    check('the aspect it reads is the tiling area, not the whole output',
      /windowsAreaOf/.test(src));
    area.__rect = landscape;
    emit({ type: 'shell.command', command: 'layout.mode', args: ['manual'] });
    emit({ type: 'shell.command', command: 'layout.mode', args: ['bsp'] });
    check('and follows the screen back when it is turned upright',
      root().dir === 'horizontal');
  }

  {
    /* The grid. Every window the same size, in rows — so what is checked is
       that the rows are the ones a person would draw, that they hold every
       window, and that the count follows the screen rather than being fixed.
       The row count is a pure function of (count, w, h) and is checked
       directly: the shapes worth knowing about are on a 32:9 and on a monitor
       stood on its end, and the stub has one rect for everything. */
    const { rows, counts, build } = globalThis.__shell.gridForTest;
    const [WIDE, TALL] = [[16, 9], [9, 16]];

    check('four windows on 16:9 are two rows of two',
      rows(4, ...WIDE) === 2 && counts(4, 2).join() === '2,2');
    check('nine are three rows of three',
      rows(9, ...WIDE) === 3 && counts(9, 3).join() === '3,3,3');
    check('six are two rows of three',
      rows(6, ...WIDE) === 2 && counts(6, 2).join() === '3,3');
    check('two sit side by side rather than one above the other',
      rows(2, ...WIDE) === 1);
    check('one window is one row', rows(1, ...WIDE) === 1);

    check('an ultrawide gets fewer rows than 16:9 for the same windows',
      rows(4, 32, 9) < rows(4, ...WIDE));
    check('and a screen on its end gets more',
      rows(4, ...TALL) > rows(4, ...WIDE));

    /* An odd count cannot fill a lattice, and the choice is between a wider
       cell in the last row and a hole in the grid. A hole is screen nobody can
       use, so the remainder goes to the earlier rows and every row stays
       full. */
    check('a remainder is spread over the earlier rows, not piled in the last',
      counts(5, 2).join() === '3,2' && counts(7, 3).join() === '3,2,2');
    /* One label for the sweep rather than twenty: what is being checked is
       that no count has a shape that loses a window or asks for an empty
       row, and the interesting part is which n fails, if any. */
    let ragged = 0;
    for (let n = 1; n <= 32; n++) {
      const shape = counts(n, rows(n, ...WIDE));
      if (shape.reduce((a, b) => a + b, 0) !== n || shape.some((c) => c < 1)) {
        ragged = n;
        break;
      }
    }
    check('every count from one to thirty-two fills every row it asks for',
      ragged === 0);

    /* A row of one is that window, not a container around it: both render the
       same, and the extra level is one focus_parent walks and the session
       writes down for no reason. */
    const three = build([1, 2, 3], ...WIDE);
    check('a row holding one window is that window',
      three.children.length === 2
      && three.children[0].type === 'split'
      && three.children[1].type === 'leaf');
    check('rows stack down the screen and windows run across them',
      three.dir === 'vertical' && three.children[0].dir === 'horizontal');
    check('a single row is the root itself rather than a row inside it',
      build([1, 2], ...WIDE).dir === 'horizontal'
      && build([1, 2], ...WIDE).children.every((c) => c.type === 'leaf'));
    check('no windows is no children', build([], ...WIDE).children.length === 0);

    /* And through the shell, on the tree the mode actually builds. */
    emit({ type: 'shell.command', command: 'layout.mode', args: ['grid'] });
    check('grid keeps every window', ids().length === opened);
    check('grid is reachable by name from layout.mode',
      globalThis.__shell.tilingMode === 'grid');
    check('and by cycling, like every other arrangement',
      globalThis.__shell.TILING_MODES.includes('grid'));
    check('every cell is the same weight, so none is favoured',
      root().children.every((c) => c.weight === 1
        && (c.type === 'leaf' || c.children.every((g) => g.weight === 1))));
    emit({ type: 'shell.command', command: 'layout.mode', args: ['bsp'] });
  }

  {
    /* Rebuilding is what resets resize weights, so an arrangement that is
       already the shape it should be must be left alone — otherwise every
       relayout during a divider drag throws away the weight the drag is
       setting. The old cache did this by remembering the window list; the
       shape comparison that replaced it has to keep the property. */
    const [first] = root().children;
    first.weight = 2.5;
    /* Anything that runs a relayout. Focusing a window that already has focus
       still rebuilds the tree, which is the path being checked. */
    emit({ type: 'view.focused', id: ids()[0] });
    check('an arrangement that has not changed keeps its resize weights',
      root().children[0].weight === 2.5);

    /* But a change in the window set still rebuilds, weights and all — which
       is what every dynamic tiler does and what makes the mode dynamic. */
    emit({ type: 'view.added', id: 91, title: 'weighty', app_id: 'weighty',
      output: 'DP-1', min_width: 0, min_height: 0, floating: false,
      width: 800, height: 600 });
    check('and a window opening does rebuild it',
      root().children[0].weight === 1);
    emit({ type: 'view.removed', id: 91 });
  }

  /* Back to manual, and the tree is left alone again. */
  emit({ type: 'shell.command', command: 'layout.mode', args: ['manual'] });
  const manualShape = JSON.stringify(root());
  emit({ type: 'shell.command', command: 'layout.toggle', args: [] });
  emit({ type: 'shell.command', command: 'layout.toggle', args: [] });
  check('manual is not rearranged behind your back',
    JSON.stringify(root()) === manualShape);

  /* No argument cycles, so one key can walk the modes. */
  emit({ type: 'shell.command', command: 'layout.mode', args: [] });
  check('layout.mode with no argument moves on',
    globalThis.__shell.tilingMode !== 'manual');
  emit({ type: 'shell.command', command: 'layout.mode', args: ['manual'] });

  /* An unknown name must not leave the shell in a mode that does not exist. */
  emit({ type: 'shell.command', command: 'layout.mode', args: ['fibonacci'] });
  check('an unknown mode name is not adopted',
    globalThis.__shell.TILING_MODES.includes(globalThis.__shell.tilingMode));
  emit({ type: 'shell.command', command: 'layout.mode', args: ['manual'] });

  /* A window opening in a dynamic mode rearranges rather than landing beside
     the focused one — that is the whole difference from manual. */
  emit({ type: 'shell.command', command: 'layout.mode', args: ['master-stack'] });
  emit({ type: 'view.added', id: 90, title: 'extra', app_id: 'test',
    output: 'DP-1', min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });
  check('a new window joins the arrangement',
    globalThis.__shell.dynamicOrderForTest(root()).includes(90));
  check('and master-stack still has exactly two branches',
    root().children.length === 2);

  emit({ type: 'view.removed', id: 90 });
  check('closing it rearranges back', ids().length === opened);
  emit({ type: 'shell.command', command: 'layout.mode', args: ['manual'] });

  {
    /* Dragging the gap between two windows, with something between them in
       the tree that is not on screen.
     *
       An unclaimed session slot is a leaf with no window, so it renders
       nothing — and the divider used to be told which gap it was by counting
       the elements that did render. Against `children` that number names a
       different pair: with a slot first, the gap between the two windows
       carried index 0, and dragging it resized the slot. */
    const home = root();
    const shape = JSON.stringify(home);
    const [firstWindow, secondWindow] = ids();
    home.children = [
      { type: 'leaf', id: -500, app: 'ghost', weight: 1 },
      { type: 'leaf', id: firstWindow, weight: 1 },
      { type: 'leaf', id: secondWindow, weight: 1 },
    ];
    home.dir = 'horizontal';
    emit({ type: 'view.focused', id: firstWindow });

    const area = globalThis.__shell.outputs
      .get(globalThis.__shell.activeOutput).windowsEl;
    const divider = area.querySelector('.divider');
    check('a gap between the two windows is drawn', divider !== null);
    if (divider) {
      const [ghost, left, right] = home.children;
      for (const fn of divider.listeners.mousedown ?? []) {
        fn({
          preventDefault() {}, stopPropagation() {},
          currentTarget: divider, clientX: 100, clientY: 100,
        });
      }
      for (const fn of windowListeners.mousemove ?? []) {
        fn({ clientX: 160, clientY: 100 });
      }
      for (const fn of windowListeners.mouseup ?? []) fn({});

      check('dragging it resizes the two windows it sits between',
        left.weight !== 1 && right.weight !== 1);
      check('and leaves the slot nobody can see alone', ghost.weight === 1);
    }

    Object.assign(home, JSON.parse(shape));
    emit({ type: 'view.focused', id: firstWindow });
  }

  {
    /* Mod4 + right drag, from an edge other than the one the layout used to
       assume. The compositor names the corner the press landed nearest; which
       sibling gives up the space follows from it, so that in every case the
       edge under the hand is the one that moves.
     *
       Before this, every drag pulled the bottom right corner: taking hold of
       a window on its left and pulling left made it *smaller*, because the
       only edge that ever moved was the far one. */
    const home = root();
    const shape = JSON.stringify(home);
    const [firstWindow, secondWindow] = ids();
    home.children = [
      { type: 'leaf', id: firstWindow, weight: 1 },
      { type: 'leaf', id: secondWindow, weight: 1 },
    ];
    home.dir = 'horizontal';
    emit({ type: 'view.focused', id: firstWindow });

    const weights = () => home.children.map((child) => child.weight);

    /* The right-hand window, dragged by its left edge: it grows, and the one
       it takes the space from is the neighbour that edge faces. */
    emit({ type: 'shell.command', command: 'layout.resize.delta',
      args: [String(secondWindow), '-100', '0', 'top-left'] });
    let [left, right] = weights();
    check('dragging a left edge left grows that window', right > 1);
    check('and the neighbour it faces gives up the space', left < 1);

    Object.assign(home, JSON.parse(shape));
    home.children = [
      { type: 'leaf', id: firstWindow, weight: 1 },
      { type: 'leaf', id: secondWindow, weight: 1 },
    ];
    home.dir = 'horizontal';

    /* The same window by its right edge, which is where a resize used to
       start whatever the hand was on: the other direction grows it. */
    emit({ type: 'shell.command', command: 'layout.resize.delta',
      args: [String(secondWindow), '100', '0', 'bottom-right'] });
    [left, right] = weights();
    check('and dragging its right edge right grows it too', right > 1);

    Object.assign(home, JSON.parse(shape));
    home.children = [
      { type: 'leaf', id: firstWindow, weight: 1 },
      { type: 'leaf', id: secondWindow, weight: 1 },
    ];
    home.dir = 'horizontal';

    /* The leftmost window has nothing to its left to trade with, so its left
       edge trades with the other side instead — which still grows it, and is
       the only thing that keeps a drag on an outermost edge from doing
       nothing at all. */
    emit({ type: 'shell.command', command: 'layout.resize.delta',
      args: [String(firstWindow), '-100', '0', 'top-left'] });
    [left, right] = weights();
    check('the outermost edge has no neighbour and takes from the other side',
      left > 1 && right < 1);

    Object.assign(home, JSON.parse(shape));
    emit({ type: 'view.focused', id: firstWindow });
  }
} else if (mode === 'scrolling') {
  const before = sent.length;
  emit({ type: 'shell.command', command: 'layout.focus', args: ['left'] });
  check('focus left asks for another window',
    sent.slice(before).some((m) => m.type === 'view.focus'));

  const cols = () => globalThis.__shell.workspaces.get(1).children.length;
  check('four windows make four columns', cols() === 4);

  emit({ type: 'view.focused', id: 1 });
  emit({ type: 'shell.command', command: 'layout.consume', args: [] });
  check('consume merges two columns into one', cols() === 3);

  emit({ type: 'shell.command', command: 'layout.expel', args: [] });
  check('expel splits it back out', cols() === 4);

  emit({ type: 'view.focused', id: 4 });
  emit({ type: 'shell.command', command: 'layout.column.width', args: [] });
  emit({ type: 'shell.command', command: 'layout.column.height', args: [] });
  emit({ type: 'shell.command', command: 'window.move', args: ['left'] });
  emit({ type: 'shell.command', command: 'layout.focus', args: ['last'] });
  check('consume/expel/width/move all run', process.exitCode !== 1);

  /* The four this block opened, ignoring any a rules test opened and closed. */
  const laidOut = new Set(sent.filter((m) => m.type === 'view.layout')
    .map((m) => m.id).filter((id) => id <= 4));
  check('every window still reachable', laidOut.size === 4);

  /* Tab walks the whole strip and wraps, including the columns scrolled off
     the screen. The compositor's own cycle cannot: a column past the edge is
     reported as not on screen, which is exactly what stops its surface being
     drawn onto the monitor beside it, and the compositor steps through what is
     on screen. */
  {
    emit({ type: 'view.focused', id: 1 });
    const walked = [];
    for (let i = 0; i < 5; i++) {
      const mark = sent.length;
      emit({ type: 'shell.command', command: 'layout.focus', args: ['next'] });
      const focus = sent.slice(mark).find((m) => m.type === 'view.focus');
      if (!focus) break;
      walked.push(focus.id);
      emit({ type: 'view.focused', id: focus.id });
    }
    check('tab reaches every window on the strip',
      new Set(walked).size === 4);
    check('and wraps at the end rather than stopping',
      walked.length === 5 && walked[4] === walked[0]);

    emit({ type: 'view.focused', id: 1 });
    const mark = sent.length;
    emit({ type: 'shell.command', command: 'layout.focus', args: ['prev'] });
    const back = sent.slice(mark).find((m) => m.type === 'view.focus');
    check('and shift+tab wraps the other way, off the front of the strip',
      back !== undefined && back.id !== 1);
  }
} else if (mode === 'matrix') {
  /* The matrix.
   *
   * The other layout that computes rectangles, and the other one this harness
   * can check properly: calculateLayout() is a pure function of (windows,
   * screen) with no DOM in it at all, so it can be handed a synthetic screen
   * and asked what it produced. Where those rectangles land on a real panel is
   * tests/layout.test.js, which needs one. */
  const matrix = globalThis.__shell.matrixForTest;
  const SCREEN = { x: 0, y: 0, width: 1920, height: 1050 };
  const { gap, primaryRatio, minSlotHeight } = matrix.MATRIX;
  const lay = (n, screen = SCREEN) =>
    matrix.calculate(Array.from({ length: n }, (_, i) => i + 1), screen);

  {
    const one = lay(1);
    check('one window is the whole screen', one.length === 1
      && one[0].width === SCREEN.width && one[0].height === SCREEN.height);
    /* Sixty per cent with a hole beside it is not a layout anybody asked for:
       the split only means something once there is something to put in it. */
    check('and it is the primary', one[0].tier === 'primary');
  }

  {
    const two = lay(2);
    const [primary, second] = two;
    check('the focused window takes the share of the width it is meant to',
      Math.abs(primary.width - (SCREEN.width - gap) * primaryRatio) <= 1);
    check('at full height, on the left',
      primary.x === 0 && primary.y === 0 && primary.height === SCREEN.height);
    check('and the column is what is left of the width, less the gap',
      second.x === primary.width + gap
      && second.x + second.width === SCREEN.width);
    /* Two windows: there is nothing to give the other half of the column to,
       so the second one has all of it rather than half and a hole. */
    check('a lone second window has the whole column',
      second.height === SCREEN.height);
  }

  {
    /* The halving itself. Each slot takes half of what the one before it left,
       and the last one placed takes the remainder — so the column is full to
       the bottom whatever the count. */
    const four = lay(4);
    const slots = four.filter((g) => g.tier === 'slot');
    check('every window past the first is a slot in the column',
      slots.length === 3 && four.length === 4);
    check('and each slot is about half of the one above it',
      Math.abs(slots[1].height - slots[0].height / 2) <= gap
      && Math.abs(slots[2].height - slots[1].height) <= gap);
    check('the slots do not overlap',
      slots.every((s, i) => i === 0
        || s.y === slots[i - 1].y + slots[i - 1].height + gap));
    check('and the column reaches the bottom of the screen',
      slots.at(-1).y + slots.at(-1).height === SCREEN.height);
  }

  {
    /* Logarithmic depth. The bound is a minimum height, not a count, so how
       many windows the column shows is a function of how tall it is — and it
       is the *same* function the placement uses, which is why capacity() is
       asked rather than a number being written down here. */
    const capacity = matrix.capacity(SCREEN.height, gap, minSlotHeight);
    check('a column divides into a handful of slots, not a hundred',
      capacity >= 3 && capacity <= 6);
    check('and a shorter screen divides into fewer',
      matrix.capacity(SCREEN.height / 4, gap, minSlotHeight) < capacity);
    check('never into none, however short',
      matrix.capacity(10, gap, minSlotHeight) === 1);

    const many = lay(40);
    check('every window handed in comes back with a rectangle',
      many.length === 40);

    const drawn = many.filter((g) => !g.hidden);
    check('but only as many are drawn as the column has room for',
      drawn.length === capacity + 1);
    check('and nothing drawn is under the minimum',
      drawn.filter((g) => g.tier !== 'primary')
        .every((g) => g.height >= minSlotHeight));

    /* Past the bound they are stacked in the deepest slot: one on top and the
       rest in the buffer behind it, all at the same rectangle so that
       whichever comes forward needs no second arithmetic. */
    const stacked = many.filter((g) => g.tier === 'stacked');
    check('the overflow is stacked in the last slot',
      stacked.length === 40 - capacity
      && stacked.every((g) => g.slot === capacity - 1));
    check('exactly one of the stack is visible',
      stacked.filter((g) => !g.hidden).length === 1
      && stacked[0].hidden === false);
    check('and they all share one rectangle',
      stacked.every((g) => g.x === stacked[0].x && g.y === stacked[0].y
        && g.width === stacked[0].width && g.height === stacked[0].height));
  }

  {
    /* The gap is the theme's, not this file's. A layout that hard-coded eight
       pixels would drift the moment a config theme set --gap to anything else,
       and the drift is invisible until you look at two windows side by side. */
    const wide = matrix.calculate([1, 2, 3], SCREEN, { gap: 40 });
    check('the gap between the primary and the column is the one asked for',
      wide[1].x === wide[0].width + 40);
    check('and so is the gap between two slots',
      wide[2].y === wide[1].y + wide[1].height + 40);
    check('a wider gap leaves the primary narrower rather than overflowing',
      wide[0].width < lay(3)[0].width
      && wide[1].x + wide[1].width === SCREEN.width);

    /* And the outer one, which is the padding on `.windows`: an absolutely
       positioned field is laid out against the padding box rather than inside
       it, so nothing takes that off unless this does. Measured through the live
       path, since it is matrixAreaOf that has to do the subtracting. */
    const workspace = globalThis.__shell.workspaceOfForTest(1);
    const live = matrix.recalculate(workspace);
    const inset = matrix.MATRIX.gap;
    check('the layout is inset from the edge of the tiling area', live.length > 0
      && live[0].x === inset && live[0].y === inset);
    check('on all four sides, not merely the two it starts at',
      live.every((g) => g.x + g.width <= 1920 - inset
        && g.y + g.height <= 1050 - inset));
    check('and the column still reaches the inset bottom',
      live.filter((g) => g.tier !== 'primary').at(-1).y
      + live.filter((g) => g.tier !== 'primary').at(-1).height === 1050 - inset);
  }

  {
    /* Deterministic, which is the whole claim: the same two arguments give the
       same rectangles, and nothing about when it was called reaches them. */
    check('the layout is a function of its arguments and nothing else',
      JSON.stringify(lay(7)) === JSON.stringify(lay(7)));
    check('and it survives a screen with nothing to divide',
      matrix.calculate([], SCREEN).length === 0
      && matrix.calculate([1], { x: 0, y: 0, width: 0, height: 0 }).length === 0);

    /* On screen by construction rather than by clamping. */
    check('no window is placed off the screen it was given',
      lay(12).every((g) => g.x >= 0 && g.y >= 0
        && g.x + g.width <= SCREEN.width
        && g.y + g.height <= SCREEN.height));
  }

  {
    /* The state transition. Focus is the entire input to the order, so a
       view.focused has to move the window it names to the front of the history
       and leave everything else in the order it already had. */
    emit({ type: 'view.focused', id: 3 });
    check('focusing a window puts it at the head of the history',
      matrix.stack[0] === 3);
    const behind = matrix.stack.slice(1).filter((id) => id <= 4 && id !== 1);
    emit({ type: 'view.focused', id: 1 });
    check('and the one it displaced is next',
      matrix.stack[0] === 1 && matrix.stack[1] === 3);
    check('with the rest in the order they already had',
      matrix.stack.slice(2).filter((id) => id <= 4).join() === behind.join());

    const workspace = globalThis.__shell.workspaceOfForTest(1);
    const order = matrix.order(workspace);
    check('which is the order the layout is built in',
      order[0] === 1 && order[1] === 3);
    const live = matrix.recalculate(workspace, SCREEN);
    check('so the window in focus is the one holding the primary slot',
      live[0].id === 1 && live[0].tier === 'primary');
  }

  {
    /* Leaving the layout has to undo it, for the same reason solar does: a
       window still carrying a slot's left/top would be placed in the middle of
       a tiling column, and one still hidden under a stack would stay invisible
       for the rest of the session. */
    const view = globalThis.__shell.views.get(2);
    check('the matrix positions a window absolutely',
      view.el.classList.contains('tile') && view.el.style.left !== '');
    emit({ type: 'config', layout: 'tiling' });
    check('and leaving it clears that off again',
      view.el.style.left === '' && view.el.style.top === ''
      && !view.el.classList.contains('tile') && view.el.hidden === false);
    emit({ type: 'config', layout: 'matrix' });
  }
  const laidOut = new Set(sent.filter((m) => m.type === 'view.layout')
    .map((m) => m.id).filter((id) => id <= 4));
  check('every window still reachable', laidOut.size === 4);
} else if (mode === 'canvas') {
  /* The canvas.
   *
   * Two things worth checking and one that cannot be. The projection is a pure
   * function of (items, viewport, area) and is checked directly. So is what
   * panning, zooming and following do to a viewport. What cannot be checked
   * here is that a click lands on the window it appears to — that is the
   * compositor's hit test, it has no scale in it, and the zoom cap at 1.0 is
   * this layout's answer to that until it does. See the header of canvas.js.
   */
  const canvas = globalThis.__shell.canvasForTest;
  /* The one output this harness has. Named once: the live checks below move
     its workspace about, and a name repeated eight times is a name that gets
     changed in seven places. */
  const S_NAME = 'DP-1';
  const AREA = { x: 8, y: 8, width: 1904, height: 1034 };
  const near = (a, b, slack = 0.5) => Math.abs(a - b) <= slack;
  const at = (x, y, zoom = 1) => ({ x, y, zoom });
  const item = (id, x, y, width = 400, height = 300) =>
    ({ id, rect: { x, y, width, height } });

  {
    /* The projection, at 1:1. A window at the world origin is drawn at the top
       left of the area — not of the output, which is the whole point of the
       area carrying an inset. */
    const [one] = canvas.project([item(1, 0, 0)], at(0, 0), AREA);
    check('a window at the origin is drawn at the top left of the area',
      one.x === AREA.x && one.y === AREA.y);
    check('and at 1:1 it is drawn at its own size',
      one.width === 400 && one.height === 300 && one.scale === 1);

    /* Panning right is looking further right, so the windows move left. The
       map convention, not the scrollbar one. */
    const [panned] = canvas.project([item(1, 0, 0)], at(200, 0), AREA);
    check('panning right moves the windows left',
      panned.x === AREA.x - 200 && panned.y === AREA.y);
  }

  {
    /* The invariant the whole layout exists for: zooming out changes what is
       *drawn*, never what the client is. A window half-size on screen is still
       reported at its own width, with the factor beside it — geometry.js sends
       both and the compositor scales the buffer, so nothing is reconfigured by
       a pan or a zoom. */
    const [small] = canvas.project([item(1, 0, 0)], at(0, 0, 0.5), AREA);
    check('zooming out does not resize the client',
      small.width === 400 && small.height === 300);
    check('it only says what to draw it at', small.scale === 0.5);
    check('and the drawing is half as far across the screen',
      small.x === AREA.x && canvas.project([item(1, 400, 0)],
        at(0, 0, 0.5), AREA)[0].x === AREA.x + 200);
  }

  {
    /* Off the edge of the view is not drawn at all, which is what keeps a
       plane holding fifty windows costing what four do. Overlapping the edge
       is a different answer: that one is drawn and cropped. */
    const far = canvas.project([item(1, 100000, 0)], at(0, 0), AREA)[0];
    check('a window past the edge of the view is off screen', far.offscreen);
    const straddling = canvas.project(
      [item(1, AREA.width - 100, 0)], at(0, 0), AREA)[0];
    check('one hanging over it is still drawn', !straddling.offscreen);
    const behind = canvas.project([item(1, -399, 0)], at(0, 0), AREA)[0];
    check('and one pixel of a window is enough', !behind.offscreen);
  }

  {
    /* Fitting. The cap is the interesting half: a plane that would fit at 3x
       is shown at 1.0, because 1.0 is the only scale the compositor can route
       a click through. */
    const spread = [item(1, 0, 0), item(2, 4000, 2000)];
    const fitted = canvas.fit(spread, AREA);
    check('fitting a wide plane zooms out', fitted.zoom < 1);
    check('but never past the floor',
      fitted.zoom >= canvas.CANVAS.minZoom);

    const tight = canvas.fit([item(1, 0, 0)], AREA);
    check('and fitting something that already fits does not zoom in',
      tight.zoom === 1);

    /* Centred, so the fit lands on the middle of the plane rather than on its
       top left corner — which is what a clamped zoom would otherwise do. */
    const placed = canvas.project(spread, fitted, AREA);
    const centre = {
      x: (placed[0].x + placed[1].x + placed[1].width * fitted.zoom) / 2,
      y: (placed[0].y + placed[1].y + placed[1].height * fitted.zoom) / 2,
    };
    check('with the plane centred in the area',
      near(centre.x, AREA.x + AREA.width / 2, 2)
      && near(centre.y, AREA.y + AREA.height / 2, 2));
  }

  {
    /* Following focus moves as little as it can: a window already on screen
       does not move the view at all, which is what stops every focus change
       from sliding the whole plane about. */
    const viewport = at(0, 0);
    const still = canvas.follow({ x: 200, y: 200, width: 400, height: 300 },
      viewport, AREA);
    check('following a window already in view moves nothing',
      still.x === viewport.x && still.y === viewport.y);

    const away = { x: 3000, y: 0, width: 400, height: 300 };
    const moved = canvas.follow(away, viewport, AREA);
    check('following one off to the right pans right',
      moved.x > viewport.x);
    /* Flush with the area, which is where the gaps already are: the area is
       the output inset by the edge gap, and the projection puts that inset in
       front of every place. A margin on top of it would be a second gap. */
    check('by just enough to fit it, and no gap on top of the area',
      canvas.followMargin(AREA) === 0
      && near(moved.x, away.x + away.width - AREA.width));
    check('and following never changes the zoom',
      moved.zoom === viewport.zoom);

    /* An area that lost the gap — what smart gaps do to a plane holding one
       window — gets it back here, so following leaves the configured gap
       whatever the area is doing. */
    const bare = { x: 0, y: 0, width: 1920, height: 1050 };
    const off = canvas.follow(away, viewport, bare);
    check('an area with no gap of its own is followed with one',
      canvas.followMargin(bare) > 0
      && near(off.x,
        away.x + away.width + canvas.followMargin(bare) - bare.width));

    /* An oversized window shows its start rather than its end: both branches
       fire and the top-left one wins. */
    const huge = canvas.follow({ x: 0, y: 0, width: 9000, height: 9000 },
      at(500, 500), AREA);
    check('an oversized window is followed to its top left',
      near(huge.x, 0) && near(huge.y, 0));
  }

  {
    /* Zoom is multiplicative about the middle of the screen, so out and back
       in is where it started. Additive steps do not compose and anchoring at
       the viewport origin walks the plane sideways on every press. */
    const start = at(400, 400, 0.8);
    const out = canvas.zoomed(start, 1 / canvas.CANVAS.zoomStep, AREA);
    const back = canvas.zoomed(out, canvas.CANVAS.zoomStep, AREA);
    check('zooming out and back in returns to the same view',
      near(back.x, start.x) && near(back.y, start.y)
      && near(back.zoom, start.zoom, 1e-9));

    let zoom = at(0, 0, 1);
    for (let i = 0; i < 6; i++) zoom = canvas.zoomed(zoom, 2, AREA);
    check('and zooming in stops at 1:1', zoom.zoom === 1);
    check('which is the cap the compositor can hit-test',
      canvas.clamp(4) === 1 && canvas.CANVAS.maxZoom === 1);
  }

  {
    /* And now against the live shell, which has four windows open on one
       workspace. Every one of them is placed: the plane is where windows are,
       and a window with no place would be a window that is nowhere. */
    const workspace = globalThis.__shell.workspaceOfForTest(1);
    const live = canvas.recalculate(workspace, AREA);
    check('every window on the workspace is on the plane',
      new Set(live.map((p) => p.id)).size >= 4);

    /* Nothing overlaps exactly: opening several windows without moving any
       cascades them, or three terminals are one terminal with two behind. */
    const spots = live.map((p) => `${p.x},${p.y}`);
    check('and no two of them are in exactly the same place',
      new Set(spots).size === spots.length);

    const before = { ...canvas.viewport(workspace) };
    emit({ type: 'shell.command', command: 'canvas.pan', args: ['right'] });
    check('panning moves the view',
      canvas.viewport(workspace).x > before.x);

    emit({ type: 'shell.command', command: 'canvas.zoom', args: ['out'] });
    check('zooming out takes it below 1:1',
      canvas.viewport(workspace).zoom < 1);

    emit({ type: 'shell.command', command: 'canvas.fit', args: [] });
    check('fitting keeps it there or further out',
      canvas.viewport(workspace).zoom <= 1);

    emit({ type: 'shell.command', command: 'canvas.home', args: [] });
    check('and home comes back to 1:1',
      canvas.viewport(workspace).zoom === 1);

    /* Tab reaches a window that has been panned off the plane.
     *
     * A window outside the view is left out of the render and reported to the
     * compositor as not on screen, which is what stops its surface being
     * painted into a hole that is not there — and the compositor's own cycle
     * walks what is on screen. So Mod4+Tab could reach the windows around the
     * view and nothing beyond them, and getting to the one you had parked
     * meant panning to it first: the manoeuvre this layout exists to avoid. */
    const far = canvas.places.get(4);
    canvas.places.set(4, { ...far, x: far.x + 100000 });
    /* A focus event ends in a relayout, which is what re-renders the plane. */
    emit({ type: 'view.focused', id: 1 });

    const parked = globalThis.__shell.views.get(4);
    check('the parked window is off the plane and not drawn',
      parked.el.hidden === true);

    const reached = new Set();
    for (let i = 0; i < 8; i++) {
      const mark = sent.length;
      emit({ type: 'shell.command', command: 'layout.focus', args: ['next'] });
      const focus = sent.slice(mark).find((m) => m.type === 'view.focus');
      if (!focus) break;
      reached.add(focus.id);
      /* The compositor answers a view.focus with view.focused, and the shell
         only steps on from where focus actually is. */
      emit({ type: 'view.focused', id: focus.id });
    }
    check('tab reaches the window nobody can see', reached.has(4));
    check('and every other window on the plane as well',
      [1, 2, 3, 4].every((id) => reached.has(id)));

    /* Focusing it panned the view onto it, which is what makes "reached"
       worth anything: a focused window nobody can see is the same bug wearing
       a different hat. */
    emit({ type: 'view.focused', id: 4 });
    check('and following it brought it back on screen',
      globalThis.__shell.views.get(4).el.hidden === false);

    /* And a floating window, which on a plane is a window like any other: the
       canvas gives it a place and pans with it. It is out of the tree, so a
       cycle built from the tree alone walks straight past it — the plane's own
       order is the one that has everything on it. */
    emit({ type: 'view.focused', id: 3 });
    emit({ type: 'shell.command', command: 'layout.float.toggle', args: [] });
    emit({ type: 'view.focused', id: 1 });

    const afloat = new Set();
    for (let i = 0; i < 8; i++) {
      const mark = sent.length;
      emit({ type: 'shell.command', command: 'layout.focus', args: ['next'] });
      const focus = sent.slice(mark).find((m) => m.type === 'view.focus');
      if (!focus) break;
      afloat.add(focus.id);
      emit({ type: 'view.focused', id: focus.id });
    }
    check('tab reaches a floating window on the plane', afloat.has(3));

    emit({ type: 'view.focused', id: 3 });
    emit({ type: 'shell.command', command: 'layout.float.toggle', args: [] });

    /* Back the other way, which is the same cycle read backwards. */
    emit({ type: 'view.focused', id: 1 });
    const mark = sent.length;
    emit({ type: 'shell.command', command: 'layout.focus', args: ['prev'] });
    const back = sent.slice(mark).find((m) => m.type === 'view.focus');
    check('shift+tab steps the other way', back !== undefined && back.id !== 1);

    canvas.places.set(4, far);
    emit({ type: 'view.focused', id: 1 });

    /* Moving a window edits the plane rather than the tree — the tree is what
       every other layout reads, and the canvas leaves it exactly as it found
       it. */
    const tree = JSON.stringify(globalThis.__shell.workspaces.get(workspace));
    const place = { ...canvas.places.get(4) };
    emit({ type: 'view.focused', id: 4 });
    emit({ type: 'shell.command', command: 'window.move', args: ['right'] });
    check('moving a window moves it across the plane',
      canvas.places.get(4).x === place.x + canvas.CANVAS.moveStep);
    check('and leaves the tree alone',
      JSON.stringify(globalThis.__shell.workspaces.get(workspace)) === tree);
  }

  {
    /* Two planes do not crowd each other.
     *
     * A workspace nothing has drawn yet starts empty, and the first window on
     * it belongs in the middle of it — not cascaded past windows on another
     * workspace it will never share a screen with. That is what an empty
     * second monitor looks like, and comparing against every place in the map
     * rather than the ones on this plane got it visibly wrong. */
    const workspace = globalThis.__shell.workspaceOfForTest(1);
    const other = workspace === 9 ? 8 : 9;
    const outs = globalThis.__shell.outputs;
    const host = outs.get(S_NAME);
    const wasWorkspace = host.workspace;

    host.workspace = other;
    emit({ type: 'view.added', id: 30, title: 'alone', app_id: 'alone',
      output: S_NAME, min_width: 0, min_height: 0, floating: false,
      width: 800, height: 600 });

    const area = canvas.area(host);
    const view = canvas.viewport(other);
    const alone = canvas.places.get(30);
    check('the first window on an empty plane is in the middle of it',
      alone !== undefined
      && Math.abs((alone.x + alone.width / 2)
        - (view.x + area.width / 2)) <= 1
      && Math.abs((alone.y + alone.height / 2)
        - (view.y + area.height / 2)) <= 1);

    /* And a second one steps off it — by enough to see, not merely by not
       being identical. */
    emit({ type: 'view.added', id: 31, title: 'beside', app_id: 'beside',
      output: S_NAME, min_width: 0, min_height: 0, floating: false,
      width: 800, height: 600 });
    const beside = canvas.places.get(31);
    check('and the next one is offset far enough to see',
      Math.abs(beside.x - alone.x) >= canvas.CANVAS.cascade
      && Math.abs(beside.y - alone.y) >= canvas.CANVAS.cascade);

    emit({ type: 'view.removed', id: 31 });

    /* Filling leaves the gaps even under smart gaps, which is the case that
       made this measure the output instead of trusting the area: edgeGapPx
       drops the inner gap for a workspace holding one window, and a plane
       holding one window would otherwise fill to the bare edge of the
       monitor. Smart gaps are about a tiled workspace with nothing to divide
       and have nothing to say about a plane. */
    /* Carrying the harness's rules, because a config message replaces them
       and the window rules are what a dozen checks below this one stand on. */
    emit({ type: 'config', layout: 'canvas', rules: HARNESS_RULES,
      gaps: { inner: 8, outer: 0, smart: true } });
    emit({ type: 'view.focused', id: 30 });
    const bare = canvas.area(host);
    /* The area lost the inner gap, and the follow margin is where it comes
       back: zero when the area already carries the gap, the whole of it when
       smart gaps have taken it off. */
    const margin = canvas.followMargin(bare);
    check('the test set smart gaps up: the area itself lost the inner gap',
      bare.x === 0 && margin > 0);

    const zoom = canvas.viewport(other).zoom;
    emit({ type: 'shell.command', command: 'canvas.fill', args: [] });
    const lone = canvas.places.get(30);
    const screen = host.windowsEl.getBoundingClientRect();
    const gap = bare.x + margin;
    check('filling a lone window still stops where the gaps begin',
      near(lone.width, (screen.width - gap * 2) / zoom)
      && near(lone.height, (screen.height - gap * 2) / zoom));

    emit({ type: 'config', layout: 'canvas', rules: HARNESS_RULES,
      gaps: { inner: 8, outer: 0, smart: false } });
    emit({ type: 'view.removed', id: 30 });
    host.workspace = wasWorkspace;
    emit({ type: 'view.focused', id: 4 });
  }

  {
    /* The pointer gestures, which is how a canvas is actually used.
     *
     * All three arrive as deltas in *screen* pixels from the compositor — it
     * knows where the pointer went and nothing about the plane — so all three
     * have to divide by the zoom. Checked at 0.5 for exactly that reason: at
     * 1.0 a missing division is invisible, which is how it would have shipped.
     */
    const workspace = globalThis.__shell.workspaceOfForTest(1);
    emit({ type: 'view.focused', id: 3 });
    emit({ type: 'shell.command', command: 'canvas.zoom', args: ['0.5'] });
    const zoom = canvas.viewport(workspace).zoom;
    check('the view is zoomed out for these', zoom === 0.5);

    const before = { ...canvas.places.get(3) };
    emit({ type: 'shell.command',
      command: 'layout.move.delta', args: ['3', '100', '60'] });
    const dragged = canvas.places.get(3);
    check('Mod4 + left drag moves the window across the plane',
      dragged.x === before.x + 100 / zoom
      && dragged.y === before.y + 60 / zoom);
    check('and it is not floated to do it',
      globalThis.__shell.floatingForTest(3) === null);

    emit({ type: 'shell.command',
      command: 'layout.resize.delta', args: ['3', '80', '40'] });
    const resized = canvas.places.get(3);
    check('Mod4 + right drag resizes it, in world units',
      resized.width === before.width + 80 / zoom
      && resized.height === before.height + 40 / zoom);

    /* The corner the hand took hold of, which the compositor works out from
       which quarter of the window the press landed in and sends along with
       the delta. Dragging the top left corner up and left grows the window
       and leaves the bottom right one where it was — the whole point of
       resizing from an edge other than the one the layout used to assume. */
    {
      const start = { ...canvas.places.get(3) };
      emit({ type: 'shell.command', command: 'layout.resize.delta',
        args: ['3', '-60', '-20', 'top-left'] });
      const pulled = canvas.places.get(3);
      check('a drag on the top left corner grows the window',
        pulled.width === start.width + 60 / zoom
        && pulled.height === start.height + 20 / zoom);
      check('and the bottom right corner stays where it was',
        pulled.x + pulled.width === start.x + start.width
        && pulled.y + pulled.height === start.y + start.height);
    }

    /* A corner that runs into the minimum stops moving with the pointer: a
       window that has stopped shrinking must not keep sliding across the
       plane from an edge that can no longer move. */
    {
      const start = { ...canvas.places.get(3) };
      emit({ type: 'shell.command', command: 'layout.resize.delta',
        args: ['3', '99999', '99999', 'top-left'] });
      const floored = canvas.places.get(3);
      check('a top left drag stops at the minimum',
        floored.width === canvas.CANVAS.minSize
        && floored.height === canvas.CANVAS.minSize);
      check('and stops sliding there too',
        floored.x + floored.width === start.x + start.width
        && floored.y + floored.height === start.y + start.height);
    }

    /* A resize cannot go to nothing: a rectangle too small to take hold of is
       one that cannot be grown again. */
    emit({ type: 'shell.command',
      command: 'layout.resize.delta', args: ['3', '-99999', '-99999'] });
    const tiny = canvas.places.get(3);
    check('and never below the minimum',
      tiny.width === canvas.CANVAS.minSize
      && tiny.height === canvas.CANVAS.minSize);

    /* Mod4+r fills the screen without going fullscreen. Checked at 0.5 like
       everything else in this block: the size is in world units, so at half
       zoom a screen-filling window is twice the screen wide on the plane, and
       a missing division would be invisible at 1.0. */
    const filling = { ...canvas.viewport(workspace) };
    const host = globalThis.__shell.outputs.get(S_NAME);
    const screen = host.windowsEl.getBoundingClientRect();
    const area = canvas.area(host);
    /* Where the gaps begin, which is the area's own edge here: without smart
       gaps in play the area already carries the whole of it, so the follow
       margin on top is zero. */
    const gap = area.x + canvas.followMargin(area);
    check('and the follow margin adds nothing to an area that has the gap',
      canvas.followMargin(area) === 0 && gap > 0);
    emit({ type: 'shell.command', command: 'canvas.fill', args: [] });
    const filled = canvas.places.get(3);
    check('canvas.fill sizes the focused window to the screen, less the gaps',
      near(filled.width, (screen.width - gap * 2) / filling.zoom)
      && near(filled.height, (screen.height - gap * 2) / filling.zoom));
    /* Drawn where the gap ends: the projection puts area.x in front of the
       place, so the place carries the difference between the two. */
    check('and starts where the gaps do',
      near(filled.x * filling.zoom + area.x - filling.x * filling.zoom, gap)
      && near(filled.y * filling.zoom + area.y - filling.y * filling.zoom, gap));
    check('and moves the view not at all: it is a resize, not a pan',
      near(canvas.viewport(workspace).x, filling.x)
      && near(canvas.viewport(workspace).y, filling.y)
      && canvas.viewport(workspace).zoom === filling.zoom);
    check('and the window is not fullscreen',
      globalThis.__shell.fullscreenOnForTest(workspace) !== 3);

    /* Dragging the desktop moves the view the other way: the plane follows the
       hand, rather than sliding out from under it. */
    const view = { ...canvas.viewport(workspace) };
    emit({ type: 'shell.command',
      command: 'canvas.pan.delta', args: ['120', '40'] });
    const panned = canvas.viewport(workspace);
    check('dragging the desktop pans the plane with the hand',
      panned.x === view.x - 120 / zoom && panned.y === view.y - 40 / zoom);

    /* And every window on the plane moves with it rather than easing after it.
       A pan moves all of them, so none carries the dragged window's own class
       — the container says it for the lot, and only while the hand is down. */
    const out = globalThis.__shell.outputs.get(globalThis.__shell.activeOutput);
    check('a pan drag draws the plane without its animations',
      out.windowsEl.classList.contains('gesture'));
    emit({ type: 'shell.command', command: 'layout.drag.end', args: [] });
    check('and the release puts the animations back',
      !out.windowsEl.classList.contains('gesture'));

    emit({ type: 'shell.command', command: 'canvas.home', args: [] });
    emit({ type: 'view.focused', id: 4 });
  }

  {
    /* A client's minimum size scales with the picture.
     *
     * addView puts the minimum on the element so flexbox enforces it, which is
     * right in a layout made of flexboxes and wrong in one that draws a window
     * smaller than it is: `min-width` is in drawn pixels and beats `width`, so
     * an unscaled minimum stops the element shrinking partway through a zoom
     * out. reportGeometry then divides the measured size by the scale and asks
     * the compositor to make the *client* bigger — so zooming out grows every
     * client instead of shrinking its picture, and a click lands where the
     * window is laid out rather than where it is drawn. Chrome shows it first,
     * having the largest minimum of anything most people run. */
    const workspace = globalThis.__shell.workspaceOfForTest(1);
    emit({ type: 'view.added', id: 35, title: 'chrome', app_id: 'chrome',
      output: S_NAME, min_width: 500, min_height: 400, floating: false,
      width: 900, height: 700 });
    const view = globalThis.__shell.views.get(35);
    check('the client named a minimum size',
      view.minWidth === 500 && view.minHeight === 400);

    emit({ type: 'view.focused', id: 35 });
    emit({ type: 'shell.command', command: 'canvas.zoom', args: ['0.5'] });
    const zoom = canvas.viewport(workspace).zoom;
    check('and the drawn minimum comes down with the zoom',
      zoom === 0.5
      && view.el.style.minWidth === `${Math.round(500 * zoom)}px`
      && view.el.style.minHeight === `${Math.round(400 * zoom)}px`);

    /* The place is the size the client is *asked* to be, so it is the one
       thing that must never go under the client's own minimum — a window drawn
       at a size it refused has a picture and an idea of itself that disagree. */
    emit({ type: 'shell.command',
      command: 'layout.resize.delta', args: ['35', '-9999', '-9999'] });
    const place = canvas.places.get(35);
    check('and the place never goes below what the client accepts',
      place.width === 500 && place.height === 400);

    /* Leaving the layout hands the unscaled minimum back, or a window that had
       been zoomed out arrives in a tiling column able to shrink to a quarter of
       what the client accepts. */
    /* Through `layout.model` rather than a `config` message: a config without
       a `rules` key clears the window rules, and the block below needs the one
       that floats its dialog. */
    emit({ type: 'shell.command', command: 'layout.model', args: ['tiling'] });
    check('and leaving the canvas restores the client\'s own minimum',
      view.el.style.minWidth === '500px'
      && view.el.style.minHeight === '400px');
    emit({ type: 'shell.command', command: 'layout.model', args: ['canvas'] });

    emit({ type: 'view.removed', id: 35 });
    emit({ type: 'shell.command', command: 'canvas.home', args: [] });
    emit({ type: 'view.focused', id: 4 });
  }

  {
    /* A floating window is on the plane like everything else.
     *
     * Solar and the matrix leave floating windows out, because a dialog floats
     * precisely so that it will not be tiled. A plane is not a division of
     * space and nothing on it is tiled, so there is nothing for that argument
     * to say here — and leaving them out is not neutral: relayoutAll writes a
     * floating window's rect straight onto the element in screen coordinates,
     * so one left off the plane sits still while everything around it pans.
     * Which windows the compositor floats is not visible to the person at the
     * screen, so it arrives as "this one window ignores me". */
    const workspace = globalThis.__shell.workspaceOfForTest(1);
    /* The view as it stands *before* the window arrives, because that is the
       one its place is worked out against — focus follows the new window a
       moment later, and reading the viewport afterwards would be comparing the
       rect against a plane that has since moved under it. */
    const opened = { ...canvas.viewport(workspace) };
    emit({ type: 'view.added', id: 34, title: 'dialogy', app_id: 'dialogy',
      output: S_NAME, min_width: 0, min_height: 0, floating: false,
      width: 300, height: 200 });
    check('the harness floated it, as its rule says',
      globalThis.__shell.floatingForTest(34) !== null);
    const place = canvas.places.get(34);
    check('and a floating window is given a place on the plane',
      place !== undefined);

    /* At the rect it was told to open at, rather than wherever a new window
       would go: a rule that says where an application opens still decides. */
    const rule = globalThis.__shell.floatingForTest(34);
    const area = canvas.area(globalThis.__shell.outputs.get(S_NAME));
    check('at the rect it was opened with, not in the middle of the screen',
      Math.abs(place.x - (opened.x + rule.x - area.x)) <= 1
      && Math.abs(place.y - (opened.y + rule.y - area.y)) <= 1
      && place.width === rule.width && place.height === rule.height);

    const before = globalThis.__shell.views.get(34).el.style.left;
    emit({ type: 'shell.command', command: 'canvas.pan', args: ['right'] });
    const after = globalThis.__shell.views.get(34).el.style.left;
    check('and it pans with the plane rather than staying on the screen',
      before !== '' && after !== '' && before !== after);

    emit({ type: 'view.removed', id: 34 });
    emit({ type: 'shell.command', command: 'canvas.home', args: [] });
    emit({ type: 'view.focused', id: 4 });
  }

  {
    /* A dialog opens on the window it belongs to.
     *
     * The rectangle a dialog arrives with is chosen to centre it on the
     * *screen*, which is right wherever the window it belongs to is also on
     * the screen. On a plane with no edges the parent can be anywhere, and the
     * dialog would open in the middle of the view attached to nothing while
     * the window that raised it sat off to one side. The compositor knows
     * whose dialog it is — it reads the same parent link to decide the window
     * floats at all — and now says so. */
    const workspace = globalThis.__shell.workspaceOfForTest(1);
    const host = canvas.places.get(2);
    /* Put the parent somewhere the view is definitely not centred on, and put
       it back afterwards — the blocks below read this window's place and a
       test that moves the furniture should move it back. */
    const wasAt = { x: host.x, y: host.y };
    host.x = 6000;
    host.y = 4000;

    emit({ type: 'view.added', id: 36, title: 'save as', app_id: 'dialogy',
      output: S_NAME, min_width: 0, min_height: 0, floating: false,
      parent: 2, width: 300, height: 200 });
    const dialog = canvas.places.get(36);
    check('a dialog is placed on its parent, not in the middle of the view',
      dialog !== undefined
      && Math.abs((dialog.x + dialog.width / 2)
        - (host.x + host.width / 2)) <= 1
      && Math.abs((dialog.y + dialog.height / 2)
        - (host.y + host.height / 2)) <= 1);
    check('and keeps the size it was opened at',
      dialog.width === 300 && dialog.height === 200);

    /* A dialog whose parent the compositor could not name is not a dialog as
       far as this is concerned, and falls back to the rect it came with.
       Against the view as it stands *before* it arrives, which is the one its
       place is worked out from — focus follows it a moment later. */
    const opened = { ...canvas.viewport(workspace) };
    emit({ type: 'view.added', id: 37, title: 'orphan', app_id: 'dialogy',
      output: S_NAME, min_width: 0, min_height: 0, floating: false,
      width: 300, height: 200 });
    const orphan = canvas.places.get(37);
    const rule = globalThis.__shell.floatingForTest(37);
    const area = canvas.area(globalThis.__shell.outputs.get(S_NAME));
    check('and one with no parent still opens where it was told to',
      orphan !== undefined
      && Math.abs(orphan.x - (opened.x + rule.x - area.x)) <= 1
      && Math.abs(orphan.y - (opened.y + rule.y - area.y)) <= 1);

    /* A parent whose client refused to be as small as it was asked to be.
     *
     * The compositor raises a configure to the client's minimum rather than
     * sending a request it knows will be refused, and said nothing about it —
     * so the shell held a rectangle for a window that is a different size, and
     * a dialog centred on that rectangle came out half the difference off.
     * Which is exactly how this was found. */
    const asked = { width: host.width, height: host.height };
    emit({ type: 'view.configured', id: 2,
      width: asked.width + 400, height: asked.height + 200 });
    const corrected = canvas.places.get(2);
    check('the place takes the size the client was actually given',
      corrected.width === asked.width + 400
      && corrected.height === asked.height + 200);
    check('and the client\'s minimum with it, on the axis that was raised',
      globalThis.__shell.views.get(2).minWidth === corrected.width
      && globalThis.__shell.views.get(2).minHeight === corrected.height);

    emit({ type: 'view.added', id: 39, title: 'save', app_id: 'dialogy',
      output: S_NAME, min_width: 0, min_height: 0, floating: false,
      parent: 2, width: 300, height: 200 });
    const onCorrected = canvas.places.get(39);
    check('so a dialog centres on the window that is really there',
      Math.abs((onCorrected.x + onCorrected.width / 2)
        - (corrected.x + corrected.width / 2)) <= 1
      && Math.abs((onCorrected.y + onCorrected.height / 2)
        - (corrected.y + corrected.height / 2)) <= 1);
    emit({ type: 'view.removed', id: 39 });

    /* And a dialog told whose it is *after* it has opened, which is what a
       portal file chooser is: another process's window, parented over
       xdg-foreign once an export and an import have gone round, long after it
       mapped. Until that message it is a window belonging to nothing, and the
       canvas has already put it in the middle of the view. */
    emit({ type: 'view.added', id: 38, title: 'open file', app_id: 'portal',
      output: S_NAME, min_width: 0, min_height: 0, floating: false,
      width: 300, height: 200 });
    const stray = { ...canvas.places.get(38) };
    check('a dialog with no parent yet is placed somewhere of its own',
      Math.abs((stray.x + stray.width / 2)
        - (host.x + host.width / 2)) > 1);

    emit({ type: 'view.parent', id: 38, parent: 2 });
    const settled = canvas.places.get(38);
    check('and moves onto its parent once it is named',
      Math.abs((settled.x + settled.width / 2)
        - (host.x + host.width / 2)) <= 1
      && Math.abs((settled.y + settled.height / 2)
        - (host.y + host.height / 2)) <= 1);

    /* But not a window someone has already dragged: a late message from a
       client does not get to undo a placement a person made. */
    emit({ type: 'shell.command',
      command: 'layout.move.delta', args: ['38', '300', '150'] });
    const dragged = { ...canvas.places.get(38) };
    emit({ type: 'view.parent', id: 38, parent: 2 });
    check('a dialog the user has moved stays where they put it',
      canvas.places.get(38).x === dragged.x
      && canvas.places.get(38).y === dragged.y);

    emit({ type: 'view.removed', id: 36 });
    emit({ type: 'view.removed', id: 37 });
    emit({ type: 'view.removed', id: 38 });
    host.x = wasAt.x;
    host.y = wasAt.y;
    emit({ type: 'view.focused', id: 4 });
  }

  {
    /* A window sent to another workspace lands where it looked like it was.
     *
     * Each plane's coordinates are its own — nothing relates workspace 1's
     * origin to workspace 7's — so carrying the numbers across names a
     * different place. Both ways that fails are worse than they sound: the
     * window arrives off the screen and sending it away looks like losing it,
     * and then focus follows it into view and drags the whole destination
     * plane across to find it, so the windows already there are the ones that
     * vanish. Sending one window away moved everything.
     *
     * What crosses is the position on the *screen*: the offset it had from the
     * corner of its old view, it has from the corner of the new one. */
    const from = globalThis.__shell.workspaceOfForTest(1);
    const to = 7;
    const canvasView = canvas.viewport(to);
    canvasView.x = 4000;
    canvasView.y = 1500;

    /* Focus first, then read the view: focusing a window pans the plane to
       bring it in, so a snapshot taken before would be of a view the move no
       longer happens from. */
    emit({ type: 'view.focused', id: 2 });
    const source = { ...canvas.viewport(from) };
    const before = { ...canvas.places.get(2) };
    const offset = {
      x: (before.x - source.x) * source.zoom,
      y: (before.y - source.y) * source.zoom,
    };

    emit({ type: 'shell.command', command: 'workspace.move',
      args: [String(to)] });
    check('the window went to the other workspace',
      globalThis.__shell.workspaceOfForTest(2) === to);

    const after = canvas.places.get(2);
    const target = canvas.viewport(to);
    check('and its place is rewritten for the plane it arrived on',
      after.x !== before.x);
    check('keeping the offset it had from the corner of the view',
      Math.abs((after.x - target.x) * target.zoom - offset.x) <= 1
      && Math.abs((after.y - target.y) * target.zoom - offset.y) <= 1);

    /* Which is the whole point: it is already in sight, so nothing has to pan
       to find it and the windows already on that plane stay where they are. */
    const settled = { ...canvas.viewport(to) };
    emit({ type: 'view.focused', id: 2 });
    check('so following it moves the destination plane not at all',
      canvas.viewport(to).x === settled.x
      && canvas.viewport(to).y === settled.y);

    emit({ type: 'shell.command', command: 'workspace.move',
      args: [String(from)] });
    emit({ type: 'view.focused', id: 4 });
  }

  {
    /* Surviving a reload.
     *
     * A reload is a new page: both maps come back empty and every window is
     * replayed. The plane *is* the layout here, so losing it loses more than a
     * reload costs any other model — hence a place in the session file, keyed
     * by application as the floating rects are, claimed as the windows return.
     */
    const saved = globalThis.__shell.sessionForTest.serialise();
    check('the session file carries the plane',
      Array.isArray(saved.canvas?.places) && saved.canvas.places.length >= 4);
    check('with an application on each place, which is what claims it',
      saved.canvas.places.every((p) => typeof p.app === 'string' && p.app
        && Number.isFinite(p.x) && Number.isFinite(p.y)
        && p.width > 0 && p.height > 0));
    check('and where each plane was being looked at from',
      saved.canvas.viewports !== undefined
      && Object.keys(saved.canvas.viewports).length > 0);

    /* And back the other way: a replayed window takes the place its
       application left rather than being seeded in the middle. */
    const workspace = globalThis.__shell.workspaceOfForTest(1);
    canvas.restore({ places: [{ app: 'returning', workspace,
      x: 4321, y: 1234, width: 640, height: 480 }], viewports: {} });
    check('a saved place waits to be claimed', canvas.slots.length === 1);

    emit({ type: 'view.added', id: 32, title: 'returning',
      app_id: 'returning', output: S_NAME, min_width: 0, min_height: 0,
      floating: false, width: 800, height: 600, replay: true });
    const back = canvas.places.get(32);
    check('and a window coming back lands on it',
      back?.x === 4321 && back?.y === 1234
      && back?.width === 640 && back?.height === 480);
    check('which takes the place out of the list', canvas.slots.length === 0);

    /* A place nothing came back for is dropped rather than handed to whatever
       opens next. */
    canvas.restore({ places: [{ app: 'never', workspace,
      x: 10, y: 10, width: 100, height: 100 }], viewports: {} });
    canvas.drop();
    emit({ type: 'view.added', id: 33, title: 'later', app_id: 'later',
      output: S_NAME, min_width: 0, min_height: 0, floating: false,
      width: 800, height: 600 });
    const later = canvas.places.get(33);
    check('an unclaimed place is not given to a later window',
      later !== undefined && !(later.x === 10 && later.y === 10));

    /* The viewport comes back too, so a reload does not scroll the plane back
       to where it started. */
    canvas.restore({ places: [], viewports: { [workspace]: {
      x: 250, y: 125, zoom: 0.5 } } });
    const view = canvas.viewport(workspace);
    check('and the view is where it was left',
      view.x === 250 && view.y === 125 && view.zoom === 0.5);

    emit({ type: 'view.removed', id: 32 });
    emit({ type: 'view.removed', id: 33 });
    emit({ type: 'shell.command', command: 'canvas.home', args: [] });
  }

  {
    /* Leaving the layout has to undo it, for the same reason solar and the
       matrix do: a window still carrying a place would be positioned in the
       middle of a tiling column. The places themselves survive on purpose —
       coming back should find the plane as it was left. */
    const view = globalThis.__shell.views.get(2);
    check('the canvas positions a window absolutely',
      view.el.classList.contains('plane') && view.el.style.left !== '');
    const kept = { ...canvas.places.get(2) };
    emit({ type: 'config', layout: 'tiling' });
    check('and leaving it clears that off again',
      view.el.style.left === '' && view.el.style.top === ''
      && !view.el.classList.contains('plane') && view.el.hidden === false);
    check('but the plane remembers where everything was',
      canvas.places.get(2).x === kept.x && canvas.places.get(2).y === kept.y);
    emit({ type: 'config', layout: 'canvas' });
    check('and going back puts it back there',
      canvas.places.get(2).x === kept.x);
  }

  const laidOut = new Set(sent.filter((m) => m.type === 'view.layout')
    .map((m) => m.id).filter((id) => id <= 4));
  check('every window still reachable', laidOut.size === 4);
} else {
  /* Solar.
   *
   * The only layout here whose rectangles are arithmetic rather than flexbox,
   * which is exactly the part a stubbed DOM can check and the part it usually
   * cannot: getBoundingClientRect returns a fixed number, so what
   * reportGeometry sends under this harness is fiction — but
   * recalculateSolarLayout() is a pure function of (ids, sun, area) and can be
   * handed a synthetic output and asked what it produced. Where the rectangles
   * land on a real screen is tests/layout.test.js, which needs one. */
  const solar = globalThis.__shell.solarForTest;
  const AREA = { x: 0, y: 0, width: 1920, height: 1050 };
  const place = (ids, sun) =>
    solar.placements({ ids, sun, area: AREA }).here;
  const by = (list, id) => list.find((p) => p.id === id);

  {
    const one = place([1], 1);
    check('one window is the whole system', one.length === 1);
    const sun = one[0];
    check('and it is the sun', sun.tier === 'sun');

    /* Sixty per cent of the area, at the area's aspect ratio: the square root
       on each axis is what makes the product come out at the fraction asked
       for rather than at its square. Two pixels of slack for the rounding on
       each side. */
    const share = (sun.width * sun.height) / (AREA.width * AREA.height);
    check('the sun takes the share of the screen it is meant to',
      Math.abs(share - solar.SOLAR.sunArea) < 0.005);
    check('at the output-s aspect ratio', Math.abs(
      (sun.width / sun.height) - (AREA.width / AREA.height)) < 0.01);
    check('centred', Math.abs((sun.x + sun.width / 2) - AREA.width / 2) <= 1
      && Math.abs((sun.y + sun.height / 2) - AREA.height / 2) <= 1);
    check('drawn at full size and fully opaque',
      sun.scale === 1 && sun.opacity === 1);
    /* The compositor gives the shell two z-bands and this is how the model
       reaches the top one — without it the window being typed into is behind
       its own orbits. */
    check('and lifted above its orbits', sun.lift === true);
  }

  {
    /* On screen by construction rather than by clamping: the projection is
       onto the boundary of a rectangle inset by the window's own size, so
       there is no angle at which anything can leave the output. This is the
       property that makes the clamping code that would otherwise be needed
       unnecessary, so it is worth checking at every count. */
    let escaped = null;
    for (let n = 1; n <= 30 && escaped === null; n++) {
      const ids = Array.from({ length: n }, (_, i) => i + 1);
      for (const p of place(ids, 1)) {
        const w = p.width * p.scale;
        const h = p.height * p.scale;
        if (p.x < AREA.x || p.y < AREA.y
          || p.x + w > AREA.x + AREA.width + 1
          || p.y + h > AREA.y + AREA.height + 1) escaped = `${n}: ${p.id}`;
      }
    }
    check('no window ever leaves the output, at any count', escaped === null);
  }

  {
    const ids = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    const all = place(ids, 1);
    check('every window is placed', all.length === ids.length);
    check('exactly one sun',
      all.filter((p) => p.tier === 'sun').length === 1);

    const inner = all.filter((p) => p.tier === 'inner');
    const outer = all.filter((p) => p.tier === 'outer');
    check('the inner orbit fills before the outer one',
      inner.length === solar.SOLAR.innerSlots.length);
    check('and the rest go cold', outer.length === ids.length - 1 - inner.length);

    check('a cold window is drawn small',
      outer.every((p) => p.scale === solar.SOLAR.outerScale));
    /* Drawn small, not made small. A focus change reshuffles every orbit, so
       giving a cold client a genuinely smaller rectangle would reconfigure
       half the workspace several times a second. */
    check('but is not itself resized',
      outer.every((p) => p.width === inner[0].width));
    check('the tiers are dimmed apart',
      inner.every((p) => p.opacity === solar.SOLAR.opacity.inner)
      && outer.every((p) => p.opacity === solar.SOLAR.opacity.outer));
    check('and nothing but the sun is lifted',
      all.filter((p) => p.lift).length === 1);
  }

  {
    /* Fixed slots, not redistribution. Opening a third window must leave the
       first two exactly where they were: a layout in which everything shifts
       whenever anything opens is one you cannot build any habit around. */
    const three = place([1, 2, 3], 1);
    const four = place([1, 2, 3, 4], 1);
    check('opening a window does not move the ones already there',
      [1, 2, 3].every((id) => {
        const a = by(three, id);
        const b = by(four, id);
        return a.x === b.x && a.y === b.y;
      }));

    /* Focusing a satellite swaps it with the sun and leaves everything else
       alone, which is the other half of the same property. */
    const focusedTwo = place([1, 2, 3, 4], 2);
    check('and focusing a satellite swaps only it and the sun',
      by(focusedTwo, 2).tier === 'sun'
      && by(focusedTwo, 1).x === by(four, 2).x
      && by(focusedTwo, 3).x === by(four, 3).x);
  }

  {
    /* An empty area is the case a monitor being reconfigured goes through, and
       a NaN written into a style there loses the window for good. */
    check('a degenerate output places nothing rather than placing nonsense',
      solar.placements({ ids: [1, 2], sun: 1,
        area: { x: 0, y: 0, width: 0, height: 0 } }).here.length === 0);
    check('and every coordinate is a real number',
      place([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 1)
        .every((p) => [p.x, p.y, p.width, p.height].every(Number.isFinite)));
  }

  /* --- driven through the shell, rather than called --- */

  const root = () => globalThis.__shell.workspaces.get(1);
  const shape = () => JSON.stringify(root());

  {
    const before = shape();
    emit({ type: 'shell.command', command: 'solar.spin', args: ['1'] });
    check('spinning leaves the tree alone', shape() === before);

    const rects = () => new Map(sent.filter((m) => m.type === 'view.layout')
      .map((m) => [m.id, `${m.x},${m.y}`]));
    const wasFocused = globalThis.__shell.solarForTest.sunOf(1);
    emit({ type: 'shell.command', command: 'solar.spin', args: ['1'] });
    check('and does not move focus off the sun',
      globalThis.__shell.solarForTest.sunOf(1) === wasFocused);
    check('but does move the satellites', rects().size > 0);
  }

  {
    const mass = solar.SOLAR.sunArea;
    emit({ type: 'shell.command', command: 'solar.mass', args: ['1'] });
    check('the sun can be grown', solar.SOLAR.sunArea > mass);
    emit({ type: 'shell.command', command: 'solar.mass', args: ['-1'] });
    check('and shrunk back', Math.abs(solar.SOLAR.sunArea - mass) < 1e-9);

    /* Bounded at both ends: a sun at the whole output leaves no ring, and one
       at nothing leaves no centre. */
    for (let i = 0; i < 40; i++) {
      emit({ type: 'shell.command', command: 'solar.mass', args: ['1'] });
    }
    check('and cannot be grown past its limit',
      solar.SOLAR.sunArea === solar.SOLAR.sunAreaMax);
    for (let i = 0; i < 40; i++) {
      emit({ type: 'shell.command', command: 'solar.mass', args: ['-1'] });
    }
    check('nor shrunk past it', solar.SOLAR.sunArea === solar.SOLAR.sunAreaMin);
    while (solar.SOLAR.sunArea < mass) {
      emit({ type: 'shell.command', command: 'solar.mass', args: ['1'] });
    }
  }

  {
    /* Ray-cast focus. Under this harness every measured rect is the same fixed
       number, so which window it picks is not a meaningful question — that it
       asks for one at all, and that it never asks for the window it was cast
       from, is. One ray per assertion: emit() replays the focus the shell asks
       for, so the sun has already moved by the time a second one is cast. */
    for (const direction of ['left', 'right', 'up', 'down']) {
      emit({ type: 'view.focused', id: 1 });
      const before = sent.length;
      emit({ type: 'shell.command', command: 'solar.ray', args: [direction] });
      const asked = sent.slice(before).filter((m) => m.type === 'view.focus');
      check(`a ray cast ${direction} asks for a window`, asked.length > 0);
      check(`and ${direction} never lands on the sun it left`,
        asked.every((m) => m.id !== 1));
    }
  }

  {
    /* Every tier's opacity reaches the compositor, because no stylesheet can:
       the frame is the shell's and the contents are a surface it never
       touches. Enough windows to overflow the inner orbit, because until it
       overflows there is no cold tier to dim. */
    const cold = [];
    for (let id = 120; id < 120 + solar.SOLAR.innerSlots.length + 3; id++) {
      cold.push(id);
      emit({ type: 'view.added', id, title: `cold${id}`, app_id: 'cold',
        output: 'DP-1', min_width: 0, min_height: 0, floating: false,
        width: 800, height: 600 });
    }
    const dimmed = sent.filter((m) => m.type === 'view.opacity'
      && m.opacity === solar.SOLAR.opacity.outer);
    check('cold windows are dimmed over IPC', dimmed.length > 0);
    check('and drawn small, which is the whole reason they are not resized',
      sent.filter((m) => m.type === 'view.layout'
        && m.scale === solar.SOLAR.outerScale).length > 0);
    for (const id of cold) emit({ type: 'view.removed', id });
  }

  {
    /* Leaving the layout has to undo it. A window still carrying an orbit's
       left/top would be positioned in the middle of a tiling column, and one
       still at 0.4 would stay dim for the rest of the session. */
    const view = globalThis.__shell.views.get(2);
    emit({ type: 'config', layout: 'tiling' });
    check('leaving solar clears the orbit off a window',
      view.el.style.left === '' && view.el.style.top === ''
      && !view.el.classList.contains('orbit'));
    check('and puts its opacity back',
      sent.filter((m) => m.type === 'view.opacity' && m.id === 2)
        .at(-1)?.opacity === 1);
    emit({ type: 'config', layout: 'solar' });
    check('and coming back rebuilds it', globalThis.__shell.views.get(2)
      .el.classList.contains('orbit'));
  }

  {
    /* layout.model is the runtime switch, and an unknown name must not leave
       the shell in a layout that does not exist. */
    emit({ type: 'shell.command', command: 'layout.model', args: ['nonesuch'] });
    check('an unknown layout name is not adopted',
      globalThis.__shell.LAYOUT_MODES.includes(globalThis.__shell.layoutMode));
    emit({ type: 'shell.command', command: 'layout.model', args: ['solar'] });
    check('and it can be asked for by name',
      globalThis.__shell.layoutMode === 'solar');
  }

  const laidOut = new Set(sent.filter((m) => m.type === 'view.layout')
    .map((m) => m.id).filter((id) => id <= 4));
  check('every window still reachable', laidOut.size === 4);
}

if (mode === 'scrolling') {
  /* Off the end of the strip must carry focus to the next monitor. Before this
   * the leftmost and rightmost columns trapped focus on one screen. */
  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
      usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
    { name: 'DP-3', x: 1920, y: 0, width: 1920, height: 1080,
      usable_x: 1920, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });

  const outs = globalThis.__shell.outputs;
  outs.get('DP-1').el.__rect = { left: 0, top: 0, width: 1920, height: 1080 };
  outs.get('DP-3').el.__rect =
    { left: 1920, top: 0, width: 1920, height: 1080 };

  /* Stand on the rightmost column of the left monitor and keep going right. */
  emit({ type: 'shell.command', command: 'layout.focus', args: ['last'] });
  const start = globalThis.__shell.activeOutput;
  emit({ type: 'shell.command', command: 'layout.focus', args: ['right'] });
  check('past the last column focus moves to the next monitor',
    globalThis.__shell.activeOutput !== start);

  emit({ type: 'shell.command', command: 'layout.focus', args: ['left'] });
  check('and back again', globalThis.__shell.activeOutput === start);

  /* ...unless it was turned off, which is what someone wants when the edge of
   * one screen is a keypress away from losing their place on it. */
  emit({ type: 'config', layout: 'scrolling', logo: true, tutorial: true,
    focus_crosses_outputs: false });

  emit({ type: 'shell.command', command: 'layout.focus', args: ['last'] });
  const held = globalThis.__shell.activeOutput;
  emit({ type: 'shell.command', command: 'layout.focus', args: ['right'] });
  check('with crossing off, the end of the strip is where focus stops',
    globalThis.__shell.activeOutput === held);

  emit({ type: 'shell.command', command: 'layout.focus', args: ['first'] });
  emit({ type: 'shell.command', command: 'layout.focus', args: ['left'] });
  check('and the same at the other end',
    globalThis.__shell.activeOutput === held);

  /* Up and down fall off the strip the same way, so they honour it too —
   * otherwise the setting would hold for h and l and not for j and k. */
  emit({ type: 'shell.command', command: 'layout.focus', args: ['up'] });
  check('and vertically, which falls through the same way',
    globalThis.__shell.activeOutput === held);

  /* Asking for a monitor by name is not falling off the end of one, and still
   * works — the setting is about the accident, not the intent. */
  emit({ type: 'shell.command', command: 'output.focus', args: ['right'] });
  check('an explicit output.focus still crosses',
    globalThis.__shell.activeOutput !== held);

  /* Back on, and the original behaviour returns without a reload. */
  emit({ type: 'config', layout: 'scrolling', logo: true, tutorial: true,
    focus_crosses_outputs: true });
  emit({ type: 'shell.command', command: 'layout.focus', args: ['last'] });
  const again = globalThis.__shell.activeOutput;
  emit({ type: 'shell.command', command: 'layout.focus', args: ['right'] });
  check('turning it back on restores crossing',
    globalThis.__shell.activeOutput !== again);

  /* A config that says nothing about it must not turn it off: absent means on,
   * and the compositor omits keys it has no opinion about. */
  emit({ type: 'config', layout: 'scrolling', logo: true, tutorial: true });
  emit({ type: 'shell.command', command: 'layout.focus', args: ['last'] });
  const silent = globalThis.__shell.activeOutput;
  emit({ type: 'shell.command', command: 'layout.focus', args: ['right'] });
  check('a config that omits the key leaves crossing on',
    globalThis.__shell.activeOutput !== silent);

  /* Put the focus back where the rest of the file expects it. These checks
   * walked onto the second monitor, whose workspace is empty, and everything
   * after this reads the columns of whichever output is active. */
  emit({ type: 'shell.command', command: 'layout.focus', args: ['left'] });
  check('the strip is back on the monitor the other tests use',
    globalThis.__shell.activeOutput === start);
}

if (mode === 'scrolling') {
  /* A full-width column has to fit the space a window may occupy, which is the
   * tiling area minus its padding. Measured against the border box instead, it
   * came out two gaps too wide and ran off the right edge.
   *
   * Checked on the column element the shell sizes, not on the reported
   * geometry: the stub returns one fixed rect for every element, so a measured
   * width would say nothing about the layout. */
  const ws0 = globalThis.__shell.outputs
    .get(globalThis.__shell.activeOutput).workspace;
  const cols0 = globalThis.__shell.workspaces.get(ws0).children;
  const widened = cols0[0];
  const savedWidth = widened.width;
  widened.width = 1;
  emit({ type: 'shell.command', command: 'layout.focus', args: ['first'] });

  const stripOf = () => {
    for (const o of globalThis.__shell.outputs.values()) {
      const el = o.windowsEl.children[0];
      if (el?.classList?.contains('strip')) return el;
    }
    return null;
  };
  const columnWidths = () => (stripOf()?.children ?? [])
    .filter((c) => c.classList.contains('column'))
    .map((c) => parseInt(c.style.width, 10));

  /* The stub reports a 1920-wide tiling area and an 8px gap, so the space a
     window may occupy is 1904. */
  const full = columnWidths()[0];
  check('a full-width column fits inside the padding', full <= 1904);

  widened.width = savedWidth;

  /* Two half-width columns and the divider between them must fit exactly, or
     moving focus from one to the other scrolls the strip and everything
     visibly shifts. */
  if (cols0.length >= 2) {
    cols0[0].width = 1 / 2;
    cols0[1].width = 1 / 2;
    const offsets = globalThis.__shell.scrollOffsets;
    emit({ type: 'shell.command', command: 'layout.focus', args: ['first'] });

    /* Two halves plus the divider between them must come to the whole width,
       not more. */
    const halves = columnWidths().slice(0, 2);
    check('two halves and a divider fill the width exactly',
      halves[0] + halves[1] + 8 <= 1904);

    emit({ type: 'shell.command', command: 'layout.focus', args: ['first'] });
    const at = offsets.get(ws0) ?? 0;
    emit({ type: 'shell.command', command: 'layout.focus', args: ['right'] });
    check('switching between two halves does not scroll',
      (offsets.get(ws0) ?? 0) === at);
  }

  /* Resizing in the strip means changing the column's own width. The tiling
   * path shifts flex weights between siblings, which the strip ignores
   * entirely — columns are laid out at a fixed size — so a drag did nothing. */
  const ws = globalThis.__shell.workspaces;
  const activeWs = globalThis.__shell.outputs
    .get(globalThis.__shell.activeOutput).workspace;
  const columns = ws.get(activeWs).children;
  const target = columns[0];
  const first = target.width;

  const firstId = target.type === 'leaf'
    ? target.id : [...ws.get(activeWs).children].length && null;
  emit({ type: 'view.focused', id: firstId ?? 1 });
  emit({ type: 'shell.command',
    command: 'layout.resize.delta', args: [String(firstId ?? 1), '192', '0'] });

  check('a horizontal drag widens the column', target.width > first);

  /* And the neighbour keeps its width: columns do not share space. */
  const neighbour = columns[1];
  const kept = neighbour.width;
  emit({ type: 'shell.command',
    command: 'layout.resize.delta', args: [String(firstId ?? 1), '192', '0'] });
  check('the next column keeps its width', neighbour.width === kept);

  /* Clamped, so a drag cannot shrink a column to nothing. */
  for (let i = 0; i < 40; i++) {
    emit({ type: 'shell.command',
      command: 'layout.resize.delta',
      args: [String(firstId ?? 1), '-192', '0'] });
  }
  check('column width is clamped above zero', target.width >= 0.1);

  /* Smart gaps in the strip.
   *
   * A column keeps the width it was given, so a lone half-width column does not
   * touch its own screen edge whatever the padding is — dropping the edge gap
   * there only widened it and slid it sideways, which is not what smart gaps
   * are for. Only a full-width column counts. */
  {
    const out = globalThis.__shell.outputs.get(globalThis.__shell.activeOutput);
    const home = out.workspace;
    const alone = home === 7 ? 9 : 7;
    const smart = globalThis.__shell.smartGapsForTest;

    emit({ type: 'config', layout: mode,
      gaps: { inner: 8, outer: 0, smart: true } });
    emit({ type: 'shell.command', command: 'workspace.switch',
      args: [String(alone)] });
    emit({ type: 'view.added', id: 320, title: 'lone', app_id: 'lone',
      output: 'DP-1', min_width: 0, min_height: 0, floating: false,
      width: 800, height: 600 });
    emit({ type: 'view.focused', id: 320 });

    const column = globalThis.__shell.workspaces.get(alone).children[0];
    column.width = 1 / 2;
    emit({ type: 'shell.command', command: 'layout.focus', args: ['first'] });
    check('a lone half-width column keeps the full edge gap',
      !smart.single(alone)
      && !out.windowsEl.classList.contains('smart-single')
      && smart.edge(alone) === 8);

    column.width = 1;
    emit({ type: 'shell.command', command: 'layout.focus', args: ['first'] });
    check('a lone full-width column drops the inner gap',
      smart.single(alone)
      && out.windowsEl.classList.contains('smart-single')
      && smart.edge(alone) === 0);
    /* Smart radius is its own setting and is off unless asked for. It used to
       follow the gaps, which meant a radius somebody had set went unhonoured
       on every desktop with one window on it — read, reasonably, as the radius
       being broken. */
    check('but leaves the corners alone, which is a separate setting',
      smart.radius() === false
      && !out.windowsEl.classList.contains('smart-square'));

    emit({ type: 'config', layout: mode, border: { smart: true } });
    emit({ type: 'shell.command', command: 'layout.focus', args: ['first'] });
    check('and squares them when it is asked for',
      smart.radius() === true
      && out.windowsEl.classList.contains('smart-square'));
    check('which the compositor is told about per window',
      sent.some((m) => m.type === 'view.layout' && m.id === 320
        && m.square === true));

    /* Set apart: gaps still smart, corners explicitly not.

       The config message alone has to do it. Nothing measures on its own — a
       desktop nobody is touching runs no geometry pass — so a setting that
       arrives over the socket and waits for the next thing to happen is a
       setting that appears to do nothing: this is the whole of "the radius
       does nothing until I open a second window", where opening one is what
       finally lays the desktop out again. */
    emit({ type: 'config', layout: mode, border: { smart: false } });
    check('a config message lays the desktop out by itself',
      !out.windowsEl.classList.contains('smart-square'));

    emit({ type: 'shell.command', command: 'layout.focus', args: ['first'] });
    check('border.smart false keeps the corners while the gaps stay smart',
      smart.radius() === false
      && !out.windowsEl.classList.contains('smart-square')
      && out.windowsEl.classList.contains('smart-single'));

    emit({ type: 'config', layout: mode, border: { smart: true } });
    emit({ type: 'shell.command', command: 'layout.focus', args: ['first'] });
    check('and true squares them again',
      out.windowsEl.classList.contains('smart-square'));

    emit({ type: 'view.removed', id: 320 });
    emit({ type: 'shell.command', command: 'workspace.switch',
      args: [String(home)] });
    emit({ type: 'config', layout: mode,
      gaps: { inner: 8, outer: 0, smart: false }, border: { smart: false } });
  }
}

/* Moving a window animates: the tree is rebuilt from scratch on every relayout,
 * so there is no property change on a retained element for CSS to transition.
 * The window elements are retained, and get offset back to where they came from
 * so they slide into place. */
{
  const views = globalThis.__shell.views;
  /* And only once the hand has let go. A window is not animated toward a
     pointer that is still dragging it, so a drag left running by an earlier
     test — the delta cases above, in the layouts that have them — is a shell
     correctly declining to slide anything. The compositor ends a gesture on
     the button release; nothing else here does, so this stands in for it. */
  emit({ type: 'shell.command', command: 'layout.drag.end', args: [] });
  /* Must be a window that is actually on screen: earlier tests may have left a
     tabbed container showing only one of them, and a hidden window is skipped. */
  const entry = [...views].find(([, v]) => !v.el.hidden);
  const [id, view] = entry;
  const el = view.el;

  /* Report one position while the old layout is measured and another once the
     new one has landed — the difference a real layout engine would produce. */
  let call = 0;
  el.getBoundingClientRect = () => {
    call += 1;
    const left = call === 1 ? 400 : 0;
    return { left, top: 0, width: 800, height: 600, x: left, y: 0 };
  };

  flipSnapshot = null;
  emit({ type: 'view.focused', id });

  check('a moved window is offset back to where it came from',
    flipSnapshot?.get(id) === 'translate(400px, 0px)');
  check('and released to slide into place', el.style.transform === '');

  delete el.getBoundingClientRect;
}

if (mode === 'scrolling') {
  /* A three-finger swipe scrolls the strip under the fingers, then settles on
   * whichever column it was left on — without that last step the next relayout
   * would scroll straight back to wherever focus happened to be. */
  const outs = globalThis.__shell.outputs;
  const ws = outs.get(globalThis.__shell.activeOutput).workspace;
  const offsets = globalThis.__shell.scrollOffsets;

  emit({ type: 'shell.command', command: 'layout.focus', args: ['first'] });
  const before = offsets.get(ws) ?? 0;

  emit({ type: 'shell.command', command: 'gesture.scroll', args: ['600'] });
  check('a swipe moves the strip', (offsets.get(ws) ?? 0) > before);

  const focusBefore = sent.length;
  emit({ type: 'shell.command', command: 'gesture.settle', args: [] });
  check('and settles onto a column',
    sent.slice(focusBefore).some((m) => m.type === 'view.focus'));

  /* Scrolling left of the first column is not a thing. */
  emit({ type: 'shell.command', command: 'gesture.scroll', args: ['-99999'] });
  check('the strip does not scroll past its start',
    (offsets.get(ws) ?? 0) === 0);
  emit({ type: 'shell.command', command: 'gesture.settle', args: [] });
}

/* Pressing the switch for the workspace you are already on goes back to the
 * one before it, so the same key toggles between two. */
{
  const outs = globalThis.__shell.outputs;
  const output = outs.get(globalThis.__shell.activeOutput);
  const home = output.workspace;
  const away = home === 4 ? 6 : 4;

  emit({ type: 'shell.command', command: 'workspace.switch', args: [String(away)] });
  check('switching goes to the workspace asked for', output.workspace === away);

  emit({ type: 'shell.command', command: 'workspace.switch', args: [String(away)] });
  check('asking again goes back where you came from', output.workspace === home);

  emit({ type: 'shell.command', command: 'workspace.switch', args: [String(home)] });
  check('and again returns: the key is a toggle', output.workspace === away);

  /* The explicit command does the same without naming a workspace. */
  emit({ type: 'shell.command', command: 'workspace.back', args: [] });
  check('workspace.back goes to the previous one', output.workspace === home);

  /* Switching used to replace one set of windows with another between two
   * frames — the one moment here where nothing moves and everything changes.
   * The arrivals are faded in over IPC, because their contents are surfaces
   * the compositor draws and no stylesheet in the shell can touch them. */
  {
    /* `home` is where the windows are and `away` is empty, so the arrival is
       on the way back — leaving is the direction with nothing to fade. */
    const root = globalThis.__shell.workspaces.get(home);
    const onHome = new Set(root ? globalThis.__shell.dynamicOrderForTest(root) : []);

    emit({ type: 'shell.command', command: 'workspace.switch', args: [String(away)] });
    const mark = sent.length;
    emit({ type: 'shell.command', command: 'workspace.switch', args: [String(home)] });

    const faded = sent.slice(mark).filter((m) => m.type === 'view.opacity');
    check('switching workspace fades the arriving windows in', faded.length > 0);
    check('the fade starts from nothing', faded.some((m) => m.opacity === 0));
    check('and finishes fully opaque', faded.some((m) => m.opacity === 1));
    check('nothing off this workspace was faded',
      faded.every((m) => onHome.has(m.id)));
  }

  {
    /* Switching to a workspace with nothing on it fades nothing: there is
       nothing to arrive, and a stray opacity message would be sent to a window
       that is not on screen. */
    const empty = home === 7 ? 9 : 7;
    const mark = sent.length;
    emit({ type: 'shell.command', command: 'workspace.switch', args: [String(empty)] });
    check('an empty workspace fades nothing',
      !sent.slice(mark).some((m) => m.type === 'view.opacity'));
    emit({ type: 'shell.command', command: 'workspace.switch', args: [String(home)] });
  }

  /* A workspace with nothing before it does not move anywhere. */
  const fresh = home === 8 ? 7 : 8;
  emit({ type: 'shell.command', command: 'workspace.switch', args: [String(fresh)] });
  emit({ type: 'shell.command', command: 'workspace.switch', args: [String(fresh)] });
  check('a repeated switch never lands nowhere',
    output.workspace >= 1 && output.workspace <= 9);
}

/* The thumb-button gestures step one workspace on or back, wrapping at the
 * edges — browser back/forward, but for the workspace strip. */
{
  const outs = globalThis.__shell.outputs;
  const output = outs.get(globalThis.__shell.activeOutput);
  const base = output.workspace;
  const next = base === 9 ? 1 : base + 1;
  const prev = base === 1 ? 9 : base - 1;

  emit({ type: 'shell.command', command: 'workspace.next', args: [] });
  check('next steps one workspace on', output.workspace === next);

  emit({ type: 'shell.command', command: 'workspace.prev', args: [] });
  check('prev steps back where you came from', output.workspace === base);

  emit({ type: 'shell.command', command: 'workspace.switch', args: [String(base)] });
}

/* The overview draws every window shrunk rather than resizing it: a thumbnail
 * is smaller than many windows' minimum size, so resizing would be refused as
 * often as it was honoured. The compositor is told the real size plus a scale. */
{
  /* Park a window on a workspace no output is displaying. Showing every
     workspace at once is the point of the overview, so it must still be drawn
     — visibility normally follows whether a monitor is showing the workspace,
     and that rule has to be suspended here. */
  const views = globalThis.__shell.views;
  const parked = [...views.keys()][0];
  emit({ type: 'view.focused', id: parked });
  emit({ type: 'shell.command', command: 'workspace.move', args: ['7'] });

  const onScreen = [...globalThis.__shell.outputs.values()]
    .some((o) => o.workspace === 7);
  check('the test parked a window off screen', !onScreen);
  check('and it is hidden while parked', views.get(parked).el.hidden);

  const before = sent.length;
  emit({ type: 'shell.command', command: 'layout.overview', args: [] });

  check('the overview draws a window from an off-screen workspace',
    !views.get(parked).el.hidden);

  const announced = sent.slice(before)
    .find((m) => m.type === 'shell.overview');
  check('the compositor is told to route input to the shell',
    announced?.active === true);

  /* A thumbnail is a picture of a monitor, so it is shaped like one. The grid
     tracks are not: three workspaces across an ultrawide gives tracks nothing
     like the output, and a thumbnail stretched to its track is a monitor drawn
     in the wrong shape. */
  {
    const thumbs = [...globalThis.__shell.overviewThumbs.values()];
    check('the overview has thumbnails to check', thumbs.length > 0);
    const area = { width: 1920, height: 1050 };  // what the stub reports
    const wrong = thumbs.filter((cell) => {
      const width = parseFloat(cell.style.width);
      const height = parseFloat(cell.style.height);
      if (!Number.isFinite(width) || !Number.isFinite(height) || height === 0) {
        return true;
      }
      return Math.abs(width / height - area.width / area.height) > 0.01;
    });
    check('every thumbnail has the output\'s aspect ratio', wrong.length === 0);
  }

  const scaled = sent.slice(before)
    .filter((m) => m.type === 'view.layout' && m.scale !== undefined);
  check('windows are laid out with a scale', scaled.length > 0);
  check('the scale shrinks them',
    scaled.every((m) => m.scale > 0 && m.scale < 1));
  check('and their reported size is the real one, not the shrunken one',
    scaled.every((m) => m.width > 0 && m.height > 0));

  /* Dragging a window from one thumbnail to another moves it to that
     workspace, and leaves the overview open so more can be arranged. */
  {
    const views = globalThis.__shell.views;
    const dragged = [...views.keys()].find((vid) => !views.get(vid).el.hidden);
    if (dragged !== undefined) {
      const el = views.get(dragged).el;
      const listeners = el.listeners.mousedown ?? [];
      const before = globalThis.__shell.workspaceOfForTest(dragged);

      /* Press on the window, release over a different thumbnail. */
      const thumbs = globalThis.__shell.overviewThumbs;
      const [[otherWs, otherCell]] = [...thumbs]
        .filter(([n]) => n !== before);
      otherCell.__rect = { left: 5000, top: 900, width: 100, height: 100 };

      for (const fn of listeners) {
        fn({ preventDefault() {}, stopPropagation() {} });
      }
      for (const fn of windowListeners.mouseup ?? []) {
        fn({ clientX: 5050, clientY: 950 });
      }

      check('dragging a window between thumbnails moves it',
        globalThis.__shell.workspaceOfForTest(dragged) === otherWs);
      check('and the overview stays open for more',
        sent.filter((m) => m.type === 'shell.overview').pop()?.active === true);
    }
  }

  /* A window closing while the overview is up.
   *
   * The overview keeps per-window state — the scale each window is drawn at
   * and the thumbnail bounding it — alongside the window record rather than
   * inside it. Nothing enforces that the two agree, so this closes a window
   * from under the overview and then makes the shell use that state again:
   * every remaining window must still report a shrunken size and a clip, and
   * the closed one must be gone from every structure rather than lingering in
   * one of them. */
  {
    const views = globalThis.__shell.views;
    const visible = [...views.keys()].filter((vid) => !views.get(vid).el.hidden);
    const doomed = visible[visible.length - 1];
    if (doomed !== undefined) {
      const at = sent.length;
      emit({ type: 'view.removed', id: doomed });

      check('closing a window in the overview drops it from the view list',
        !views.has(doomed));
      check('and stops it being laid out',
        !sent.slice(at).some((m) => m.type === 'view.layout' &&
          m.id === doomed));

      /* Structure, not pixels. The survivors are not re-measured here — the
         stub's getBoundingClientRect never changes, so reportGeometry finds
         nothing moved and sends nothing — which is a property of this harness
         and not of the shell. What is worth asserting is that they are still
         overview windows and that nothing of the closed one is left. */
      check('and leaves no overview state behind for it',
        globalThis.__shell.overviewStateForTest(doomed).scale === undefined &&
        globalThis.__shell.overviewStateForTest(doomed).cell === undefined);
      check('the windows left behind are still drawn in the overview',
        visible.filter((vid) => vid !== doomed)
          .every((vid) => !views.get(vid).el.hidden));
      check('and keep the scale the overview gave them',
        visible.filter((vid) => vid !== doomed).every((vid) => {
          const scale = globalThis.__shell.overviewStateForTest(vid).scale;
          return scale > 0 && scale < 1;
        }));
    }
  }

  const exitAt = sent.length;
  emit({ type: 'shell.command', command: 'layout.overview', args: [] });
  check('leaving the overview hands input back',
    sent.slice(exitAt).find((m) => m.type === 'shell.overview')
      ?.active === false);
  check('and drops the scale',
    !sent.slice(exitAt).some((m) => m.type === 'view.layout' &&
      m.scale !== undefined));
}

/* Fullscreen is per workspace. Two monitors each showing something fullscreen
 * must not cancel each other — a single global made the second one silently
 * un-fullscreen the first. */
{
  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
      usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
    { name: 'DP-3', x: 1920, y: 0, width: 1920, height: 1080,
      usable_x: 1920, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });

  const outs = globalThis.__shell.outputs;
  const [left, right] = [...outs.values()];
  const [leftName, rightName] = [...outs.keys()];
  outs.get(leftName).el.__rect = { left: 0, top: 0, width: 1920, height: 1080 };
  outs.get(rightName).el.__rect =
    { left: 1920, top: 0, width: 1920, height: 1080 };

  /* One window on each monitor. addView places by the *active* output, not by
     the hint, so the active one has to be moved between the two — the same way
     it moves when the user looks at the other screen. */
  const onLeft = 90, onRight = 91;

  emit({ type: 'shell.command', command: 'output.focus', args: ['left'] });
  emit({ type: 'view.added', id: onLeft, title: 'a', app_id: 'test',
    output: leftName, min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });

  emit({ type: 'shell.command', command: 'output.focus', args: ['right'] });
  emit({ type: 'view.added', id: onRight, title: 'b', app_id: 'test',
    output: rightName, min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });

  check('the two windows landed on different workspaces',
    left.workspace !== right.workspace);

  {
    /* Focus an output by name as well as by direction.
     *
     * A direction is right for a key — "the screen to my left" is what someone
     * means by pressing one, and it follows the monitors being rearranged. It
     * is wrong for anything driving the shell over IPC: the layout rects are
     * here and not there, so a caller that wants a named monitor has to guess
     * a direction and hope the two are side by side. scripts/bench-vkcube.sh
     * is the caller that made this concrete — placing a client on a chosen
     * screen is the whole of what its two-monitor scenarios do, and guessing
     * `right` on a desk with stacked monitors measures the wrong one while
     * reporting success. */
    /* Each leg starts from the other monitor, by direction, so that neither
       check can pass because focus happened to already be where it was asked
       to go — which is exactly what the second one did when it was written
       the obvious way, and it passed against a shell that ignored names
       entirely. */
    emit({ type: 'shell.command', command: 'output.focus', args: ['right'] });
    emit({ type: 'shell.command', command: 'output.focus', args: [leftName] });
    check('an output can be focused by name',
      globalThis.__shell.activeOutput === leftName);

    emit({ type: 'shell.command', command: 'output.focus', args: ['left'] });
    emit({ type: 'shell.command', command: 'output.focus', args: [rightName] });
    check('and by the other name, rather than only in one direction',
      globalThis.__shell.activeOutput === rightName);

    /* A name that is not a monitor must not be taken as one. It falls through
       to the direction path, which finds nothing that way and leaves focus
       where it was — rather than throwing, which would take the whole
       command loop down. */
    emit({ type: 'shell.command', command: 'output.focus', args: ['DP-99'] });
    check('an unknown name leaves focus alone',
      globalThis.__shell.activeOutput === rightName);

    /* Focusing the output that is already active still says so.
     *
     * setActiveOutput returns early on a move to where focus already is,
     * which is right for the pointer crossing it was written for and wrong
     * for a caller over IPC: the compositor's record of the active output is
     * written by nothing but this message and starts empty, so a caller
     * asking for the output the shell happened to start on waited for a
     * confirmation that never came. That is not hypothetical — it aborted
     * every two-monitor benchmark run at the first placement, before a single
     * measurement was taken. */
    {
      const before = sent.length;
      emit({ type: 'shell.command', command: 'output.focus', args: [rightName] });
      const announced = sent.slice(before)
        .filter((m) => m.type === 'output.active' && m.name === rightName);
      check('focusing the output already active still announces it',
        announced.length > 0);
    }
  }

  emit({ type: 'view.focused', id: onLeft });
  emit({ type: 'shell.command', command: 'window.fullscreen', args: [] });
  emit({ type: 'view.focused', id: onRight });
  emit({ type: 'shell.command', command: 'window.fullscreen', args: [] });

  const unset = sent.filter((m) =>
    m.type === 'view.fullscreen' && m.id === onLeft && !m.fullscreen);
  check('fullscreen on one monitor leaves the other alone', unset.length === 0);

  const set = sent.filter((m) => m.type === 'view.fullscreen' && m.fullscreen);
  check('both monitors report fullscreen',
    set.some((m) => m.id === onLeft) && set.some((m) => m.id === onRight));

  /* Put the workspaces back: a fullscreen window hides everything else on its
     workspace, which would silently starve any later test of a laid-out
     window to measure. */
  for (const id of [onLeft, onRight]) {
    emit({ type: 'view.focused', id });
    emit({ type: 'shell.command', command: 'window.fullscreen', args: [] });
    emit({ type: 'view.removed', id });
  }
}

/* What a client outside the shell is told about the workspaces, and what it can
 * ask for.
 *
 * The workspaces are the shell's — nothing else knows they exist — so
 * ext-workspace-v1 publishes an empty world until `workspace.list` arrives. An
 * external bar with no workspace buttons on it is this message never being
 * sent, which is what it was doing before. Runs on both monitors from the block
 * above, because which screen a workspace is on is half of what is published. */
{
  const outs = globalThis.__shell.outputs;
  const [leftName, rightName] = [...outs.keys()];
  const published = () => sent.filter((m) => m.type === 'workspace.list').at(-1);

  emit({ type: 'shell.command', command: 'output.focus', args: ['left'] });

  const list = published();
  check('the shell publishes its workspaces at all', !!list);

  const here = list?.workspaces
    .find((w) => Number(w.id) === outs.get(leftName).workspace);
  check('the workspace on screen is active', here?.active === true);
  check('and is in the group of the monitor showing it',
    here?.output === leftName);
  check('every workspace carries a name to draw',
    list?.workspaces.every((w) => typeof w.name === 'string' && w.name !== ''));
  check('the other monitor\'s workspace is published too',
    list?.workspaces.some((w) => w.output === rightName && w.active === true));

  /* Nothing changed, so nothing goes out. relayoutAll() runs once per mousemove
     of a divider drag, and a workspace list per mousemove is an external bar
     redrawn sixty times a second to say what it already said. */
  const quiet = sent.length;
  emit({ type: 'output.layout', outputs: [
    { name: leftName, x: 0, y: 0, width: 1920, height: 1080,
      usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
    { name: rightName, x: 1920, y: 0, width: 1920, height: 1080,
      usable_x: 1920, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });
  check('an unchanged workspace set is not republished',
    !sent.slice(quiet).some((m) => m.type === 'workspace.list'));

  /* A bar clicking a workspace nobody is showing. */
  const free = [...Array(9).keys()].map((i) => i + 1)
    .find((n) => ![...outs.values()].some((o) => o.workspace === n));
  emit({ type: 'workspace.request', action: 'activate', id: String(free) });
  check('activating an unshown workspace brings it to the active monitor',
    outs.get(globalThis.__shell.activeOutput).workspace === free);
  check('and the list says so',
    published()?.workspaces.some((w) => Number(w.id) === free && w.active));

  /* Clicking one the other monitor is already showing goes to that monitor
     rather than making a second copy of it — and must not be read as the
     back-and-forth gesture, which would land somewhere else entirely. */
  const there = outs.get(rightName).workspace;
  emit({ type: 'workspace.request', action: 'activate', id: String(there) });
  check('activating a workspace shown elsewhere moves to that monitor',
    globalThis.__shell.activeOutput === rightName);
  check('and leaves it where it was',
    outs.get(rightName).workspace === there);

  /* Asked for again while it is the one on screen: still that workspace, not
     the one before it. */
  emit({ type: 'workspace.request', action: 'activate', id: String(there) });
  check('asking twice does not bounce off to the previous workspace',
    outs.get(rightName).workspace === there);

  /* A workspace nobody is showing still says which screen it belongs to, so a
     bar has somewhere to draw it. `free` was just left behind on the left
     monitor by the two activations above. */
  emit({ type: 'shell.command', command: 'output.focus', args: ['left'] });
  const home = published()?.workspaces.find((w) => Number(w.id) === free);
  check('a workspace that went off screen keeps the monitor it was on',
    home === undefined || home.output === leftName);

  /* Nothing the shell can honour: there are nine workspaces, always, and a
     monitor is always showing one of them. Declined by doing nothing. */
  const before = sent.length;
  emit({ type: 'workspace.request', action: 'remove', id: '1' });
  emit({ type: 'workspace.request', action: 'deactivate', id: '1' });
  check('a request this shell cannot honour changes nothing',
    !sent.slice(before).some((m) => m.type === 'workspace.list'));
}

/* Clipping: a window scrolled off the left of its output must be reported with
 * a clip rect covering only the part still on screen. Nothing stops the
 * compositor drawing the rest onto the monitor next door otherwise. */
{
  const views = globalThis.__shell.views;
  const target = [...views.keys()][0];
  views.get(target).viewport.__rect =
    { left: -400, top: 0, width: 800, height: 600 };
  views.get(target).box = null; // force a resend

  const before = sent.length;
  emit({ type: 'view.focused', id: target });

  const layout = sent.slice(before).reverse()
    .find((m) => m.type === 'view.layout' && m.id === target);
  check('off-screen window reports a clip', !!layout?.clip);
  check('clip starts at the output edge', layout?.clip?.x === 0);
  check('clip covers only what is on screen', layout?.clip?.width === 400);
  check('clip keeps the full height', layout?.clip?.height === 600);

  /* And a window fully inside its output is not clipped down. */
  views.get(target).viewport.__rect =
    { left: 100, top: 100, width: 800, height: 600 };
  views.get(target).box = null;
  const mark = sent.length;
  emit({ type: 'view.focused', id: target });
  const full = sent.slice(mark).reverse()
    .find((m) => m.type === 'view.layout' && m.id === target);
  check('on-screen window clips to its whole self',
    full?.clip?.width === 800 && full?.clip?.height === 600);

  /* The frame is bounded the same way, and for a sharper reason than the
   * clip.
   *
   * `.desktop` is `overflow: hidden`, so the page never paints a border past
   * the edge of the monitor it is on — but getBoundingClientRect measures the
   * element rather than what was painted of it. The compositor takes that
   * rectangle and draws the shell's own pixels there above the windows, and on
   * the monitor next door those pixels are *its* desktop: dragging a window
   * off the right of DP-1 laid a strip of DP-3's window borders over DP-3's
   * windows. */
  {
    const view = views.get(target);
    /* Floating, because only a lifted window reports a frame at all — a tiled
       border falls in the gap between two windows, where no surface hides it
       and none has to be drawn again. */
    emit({ type: 'view.focused', id: target });
    emit({ type: 'shell.command', command: 'layout.float.toggle', args: [] });

    const area = measureOf(
      globalThis.__shell.outputs.get('DP-1').windowsEl ??
      globalThis.__shell.outputs.get('DP-1').el);
    const edge = area.left + area.width;

    view.el.__rect = { left: edge - 120, top: 0, width: 400, height: 300 };
    view.viewport.__rect = { left: edge - 118, top: 2, width: 396, height: 296 };
    view.box = null;
    const at = sent.length;
    emit({ type: 'view.focused', id: target });
    const over = sent.slice(at).reverse()
      .find((m) => m.type === 'view.layout' && m.id === target);

    check('a window overhanging the edge reports a frame', !!over?.frame);
    check('and the frame stops at the edge of its own output',
      over.frame.x + over.frame.width <= edge);
    check('while keeping the part that is on this monitor',
      over.frame.x === edge - 120 && over.frame.width === 120);

    /* Wholly past the edge: the shell paints none of it, so there is no frame
       to draw rather than one of zero width. */
    view.el.__rect = { left: edge + 200, top: 0, width: 400, height: 300 };
    view.box = null;
    const off = sent.length;
    emit({ type: 'view.focused', id: target });
    const gone = sent.slice(off).reverse()
      .find((m) => m.type === 'view.layout' && m.id === target);
    check('and a frame entirely on the next monitor is not reported',
      gone !== undefined && gone.frame === undefined);

    /* Put it back, floating and all: everything after this reads the same
       windows. */
    view.el.__rect = undefined;
    view.viewport.__rect = { left: 100, top: 100, width: 800, height: 600 };
    view.box = null;
    emit({ type: 'shell.command', command: 'layout.float.toggle', args: [] });
    emit({ type: 'view.focused', id: target });
  }
}

/* --- the bar that hides ------------------------------------------------
 *
 * 'auto' exists for OLED panels: the bar is the one thing on screen that never
 * moves, so it is hidden until Mod4 is held. What matters here is that the
 * reveal is driven by the modifier message and that releasing puts it back —
 * a bar that reveals and then stays is the failure that defeats the point.
 */
{
  const desktop = () => outputsEl.children[0];
  const barHidden = () => desktop().classList.contains('bar-hidden');

  emit({ type: 'config', layout: mode, bar: 'auto' });
  check('auto hides the bar to begin with', barHidden());

  emit({ type: 'modifiers', logo: true });
  check('holding Mod4 reveals it', !barHidden());

  /* And it is drawn above the windows, since 'auto' reserves no room for it —
     and it takes the pointer, which it used to decline.
   *
   * Declining was for the windows underneath: Mod4 reveals the bar and Mod4 is
   * what a window is dragged and resized with, so a bar that took those left a
   * window moved up under it unable to be grabbed there. What it cost was
   * every click the bar exists to receive — a workspace pill, a window's title
   * — because under 'auto' the bar is on screen only while Mod4 is held, so a
   * click on it always arrives with that modifier down. The compositor
   * declines the gesture over anything the shell drew in front instead; see
   * `starts_gesture` in input.rs. */
  const floating = () => (sent.filter((m) => m.type === 'shell.overlay')
    .at(-1)?.rects ?? []).filter((r) => r.height > 0);
  check('the revealed bar is drawn over the windows', floating().length > 0);
  check('and takes the pointer, so its buttons can be clicked',
    floating().every((r) => r.passthrough !== true));

  emit({ type: 'modifiers', logo: false });
  check('letting go hides it again', barHidden());
  check('and stops it being drawn at all', floating().length === 0);

  /* A theme from the config file lands as custom properties. */
  emit({ type: 'config', layout: mode, bar: 'auto',
    theme: { bg: '#000000', 'not a name': '#fff', text: 'javascript:x' } });
  check('a colour from the config is applied',
    document.documentElement.style.getPropertyValue('--bg') === '#000000');
  check('a name that is not a custom property is refused',
    document.documentElement.style.getPropertyValue('--not a name') === '');
  check('a value that is not a colour is refused',
    document.documentElement.style.getPropertyValue('--text') === '');

  emit({ type: 'config', layout: mode, bar: 'visible' });
  check('switching back to visible shows the bar', !barHidden());

  /* A gaps block from the config file lands as the inner and outer gap custom
     properties, and remembers the smart flag. */
  emit({ type: 'config', layout: mode,
    gaps: { inner: 15, outer: 4, smart: true } });
  const gapsStyle = document.documentElement.style;
  check('gaps.inner lands on --gap',
    gapsStyle.getPropertyValue('--gap') === '15px');
  check('gaps.outer lands on --gap-outer',
    gapsStyle.getPropertyValue('--gap-outer') === '4px');
  /* An absent field leaves the prior/default value standing. */
  emit({ type: 'config', layout: mode, gaps: { inner: 20 } });
  check('an omitted outer keeps the previous value',
    gapsStyle.getPropertyValue('--gap-outer') === '4px');
  /* Reset for the checks that follow, which read --gap expecting its default. */
  emit({ type: 'config', layout: mode, gaps: { inner: 8, outer: 0, smart: false } });

  /* The border block from the config file. The compositor crops each client
     to this same corner, so a radius the page does not draw with is a client
     cut to a curve that is not there. */
  emit({ type: 'config', layout: mode, border: { radius: 12, width: 3 } });
  check('border.radius lands on --window-radius',
    gapsStyle.getPropertyValue('--window-radius') === '12px');
  check('border.width lands on --window-border',
    gapsStyle.getPropertyValue('--window-border') === '3px');
  emit({ type: 'config', layout: mode, border: { radius: 0, width: 0 } });
  check('a radius of zero is square and not absent',
    gapsStyle.getPropertyValue('--window-radius') === '0px');
  check('a width of zero is no border and not absent',
    gapsStyle.getPropertyValue('--window-border') === '0px');
  emit({ type: 'config', layout: mode });
  check('a config with no border block leaves the radius standing',
    gapsStyle.getPropertyValue('--window-radius') === '0px');
  emit({ type: 'config', layout: mode, border: { radius: 6 } });
  check('an omitted width keeps the previous value',
    gapsStyle.getPropertyValue('--window-border') === '0px');
  emit({ type: 'config', layout: mode, border: { width: 2 } });

  /* Extra bar widgets from the config file. Each builds an element in the
     bar's right side, filled from the status sample: a disk widget shows the
     free space on its mount, a volume widget the sink's volume and mute. The
     default modules are untouched — nothing here asserts about them. */
  const wout = globalThis.__shell.outputs.get('DP-1');
  emit({ type: 'config', layout: mode,
    bar_widgets: [
      { type: 'disk', path: '/home' },
      { type: 'volume' },
    ] });
  check('a widget element is built for each configured widget',
    wout.widgetsEls.length === 2);
  emit({ type: 'status.update', cpu: -1, memory: -1, load: 0,
    net_rx: 0, net_tx: 0, disk_free: 0, disk_total: 0,
    mounts: [{ path: '/home', free: 500000000, total: 1000000000 }],
    volume: 0.45, muted: false });
  check('a disk widget shows free space on its mount',
    wout.widgetsEls[0].textContent.includes('/home'));
  check('a volume widget shows the sink volume',
    wout.widgetsEls[1].textContent.includes('45') &&
    wout.widgetsEls[1].textContent.includes('%'));
  emit({ type: 'status.update', cpu: -1, memory: -1, load: 0,
    net_rx: 0, net_tx: 0, disk_free: 0, disk_total: 0,
    mounts: [], volume: 0.45, muted: true });
  check('a muted sink switches the volume widget to muted',
    wout.widgetsEls[1].textContent.includes('%'));
  /* A config that names no widgets takes them back off, leaving the default
     bar exactly as it shipped. */
  emit({ type: 'config', layout: mode });
  check('a config without widgets leaves the default bar',
    wout.widgetsEls.length === 0);

  /* A mic widget mirrors the volume widget but aims at the default audio
     source. It reads the mic half of the sample, and muting must switch its
     glyph without hiding it — a muted node keeps its percentage. */
  emit({ type: 'config', layout: mode, bar_widgets: [{ type: 'mic' }] });
  const micEl = wout.widgetsEls[0];
  check('a mic widget builds its own element',
    wout.widgetsEls.length === 1 && micEl.title === 'microphone');
  emit({ type: 'status.update', cpu: -1, memory: -1, load: 0,
    net_rx: 0, net_tx: 0, disk_free: 0, disk_total: 0,
    mounts: [], volume: -1, muted: false,
    mic_volume: 0.3, mic_muted: false });
  check('a mic widget shows the source volume',
    micEl.textContent.includes('30') && micEl.textContent.includes('%'));
  emit({ type: 'status.update', cpu: -1, memory: -1, load: 0,
    net_rx: 0, net_tx: 0, disk_free: 0, disk_total: 0,
    mounts: [], volume: -1, muted: false,
    mic_volume: 0.3, mic_muted: true });
  check('a muted mic keeps its percentage (does not hide)',
    micEl.textContent.includes('30') && micEl.textContent.includes('%'));
  check('a muted mic switches to the muted glyph', micEl.textContent !== '');
  /* And the glyph is a microphone. The widget shipped drawing U+F02DB and
     U+F02DC, which are md-hololens and md-home — a headset and a house. */
  check('a muted mic draws the crossed-out microphone',
    micEl.textContent.startsWith('\u{f036d}'));
  emit({ type: 'status.update', cpu: -1, memory: -1, load: 0,
    net_rx: 0, net_tx: 0, disk_free: 0, disk_total: 0,
    mounts: [], volume: -1, muted: false,
    mic_volume: 0.3, mic_muted: false });
  check('and an unmuted one draws the microphone',
    micEl.textContent.startsWith('\u{f036c}'));
  const micExec = () => sent.filter((m) => m.type === 'shell.exec');
  const micBefore = micExec().length;
  const micSentBefore = sent.length;
  micEl.listeners.wheel.forEach((fn) => fn({ preventDefault() {}, deltaY: 100 }));
  /* The microphone is the source rather than the sink, and goes the same way
     the speakers do: one `status.volume`, which changes it and re-samples in
     that order. */
  const micVolume = (from) => sent.slice(from)
    .filter((m) => m.type === 'status.volume');
  check('scrolling a mic widget down lowers the source volume by 5%',
    micVolume(micSentBefore).some((m) =>
      m.target === 'source' && m.delta === -5 && !m.mute));
  check('and runs no program to do it',
    micExec().length === micBefore);
  const micBeforeMute = micExec().length;
  const micSentBeforeMute = sent.length;
  micEl.listeners.contextmenu.forEach((fn) => fn({ preventDefault() {} }));
  check('right-clicking a mic widget toggles the source mute',
    micVolume(micSentBeforeMute).some((m) =>
      m.target === 'source' && m.mute === true));
  check('and the microphone is never confused with the speakers',
    !micVolume(micSentBefore).some((m) => m.target === 'sink'));
  emit({ type: 'config', layout: mode });

  /* The media widget. The compositor reads MPRIS — the page has no bus — and
     sends what is playing when it changes; the widget draws it, with only the
     buttons the player says it will honour. */
  {
    emit({ type: 'config', layout: mode, bar_widgets: [{ type: 'mpris' }] });
    const el = wout.widgetsEls[0];
    const parts = () => el.children;
    const press = (node) => (node.listeners.click ?? [])
      .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));

    check('nothing playing draws nothing at all',
      parts().length === 0 && el.textContent === '');

    emit({ type: 'mpris.update', player: { id: 'mpv', title: 'Rhubarb',
      artist: 'Aphex Twin', album: 'Selected Ambient Works', status: 'playing',
      art: '', can_go_next: true, can_go_previous: false, can_pause: true,
      can_play: true } });
    const label = parts().find((n) => n._classes.has('mpris-label'));
    check('a playing track is drawn with its artist',
      label?.textContent === 'Rhubarb — Aphex Twin');
    const buttons = parts().filter((n) => n._classes.has('mpris-button'));
    check('and a button for each thing the player says it can do',
      buttons.filter((b) => !b.hidden).length === 2);
    check('the one the player cannot do is hidden rather than dead',
      buttons[0].hidden === true);
    check('a playing track offers pause, which is what pressing it does',
      buttons[1].textContent === '\u{f03e4}');

    let before = sent.length;
    press(buttons[1]);
    check('pressing it drives the player over the bus, not through a program',
      sent.slice(before).some((m) => m.type === 'mpris.control' &&
        m.action === 'play-pause') &&
      !sent.slice(before).some((m) => m.type === 'shell.exec'));

    before = sent.length;
    el.listeners.wheel.forEach((fn) => fn({ preventDefault() {}, deltaY: 100 }));
    check('scrolling down skips to the next track',
      sent.slice(before).some((m) => m.type === 'mpris.control' &&
        m.action === 'next'));

    emit({ type: 'mpris.update', player: { id: 'mpv', title: 'Rhubarb',
      artist: 'Aphex Twin', album: '', status: 'paused', art: '',
      can_go_next: true, can_go_previous: true, can_pause: true,
      can_play: true } });
    check('a paused track offers play',
      parts().filter((n) => n._classes.has('mpris-button'))[1]
        .textContent === '\u{f040a}');

    emit({ type: 'mpris.update', player: null });
    check('and a player that exits takes the widget off the bar',
      parts().length === 0 && el.textContent === '');
    emit({ type: 'config', layout: mode });
  }

  /* Battery widget. The compositor reads UPower; the page draws the
     percentage and opens the profile picker. */
  {
    emit({ type: 'config', layout: mode, bar_widgets: [{ type: 'battery' }] });
    const el = wout.widgetsEls[0];
    check('no battery draws nothing at all',
      el.textContent === '');

    emit({ type: 'power.update', batteries: [{ percentage: 87, state: 'discharging' }],
      on_battery: true, lid_closed: false, profile: 'balanced',
      profiles: ['power-saver', 'balanced', 'performance'] });
    check('a discharging battery is drawn with its percentage',
      el.textContent.includes('87%'));

    emit({ type: 'power.update', batteries: [{ percentage: 40, state: 'charging' }],
      on_battery: false, lid_closed: false, profile: 'balanced',
      profiles: ['power-saver', 'balanced', 'performance'] });
    check('a charging battery keeps showing the percentage',
      el.textContent.includes('40%'));

    let before = sent.length;
    (el.listeners.click ?? []).forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));
    check('clicking it opens the profile picker',
      globalThis.__shell.powerEl.hidden === false);
    check('and names the overlay so it draws over the windows',
      sent.slice(before).some((m) => m.type === 'shell.overlay'));

    /* `powerEl` is only the docking box; the dialog centred inside it —
       `.power-dialog`, the same split renderPowerPicker's own comment
       explains for the reason — is where the rows actually live. */
    const rows = globalThis.__shell.powerEl.children[0]?.children[0]?.children ?? [];
    check('every profile the compositor listed is a row',
      rows.length === 3 && rows[1].textContent === 'balanced');

    before = sent.length;
    (rows[0].listeners.click ?? []).forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));
    check('choosing a row asks the compositor to switch profile',
      sent.slice(before).some((m) => m.type === 'power.profile' &&
        m.profile === 'power-saver'));
    check('and takes the picker down',
      globalThis.__shell.powerEl.hidden === true);

    /* Placement on a two-monitor desk — see the clipboard picker's own
       placement test for why 50%/50% is the wrong centre and what the
       harness can check about the fix instead. `powerEl` is the docking box
       renderPowerPicker sizes and positions from the active output's rect. */
    emit({ type: 'output.layout', outputs: [
      { name: 'DP-1', x: 0, y: 0, width: 2560, height: 1440,
        usable_x: 0, usable_y: 30, usable_width: 2560, usable_height: 1410,
        scale: 1, transform: 'normal', modes: [], enabled: true },
      { name: 'DP-2', x: 2560, y: 0, width: 2560, height: 1440,
        usable_x: 2560, usable_y: 30, usable_width: 2560, usable_height: 1410,
        scale: 1, transform: 'normal', modes: [], enabled: true },
    ] });
    emit({ type: 'shell.command', command: 'output.focus', args: ['DP-2'] });
    (el.listeners.click ?? []).forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));
    check('the docking box sits over the active output, not the middle of the page',
      globalThis.__shell.powerEl.style.left === '2560px' &&
      globalThis.__shell.powerEl.style.top === '0px' &&
      globalThis.__shell.powerEl.style.width === '2560px' &&
      globalThis.__shell.powerEl.style.height === '1440px');
    (el.listeners.click ?? []).forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));
    emit({ type: 'output.layout', outputs: [
      { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
        usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
        scale: 1, transform: 'normal', modes: [], enabled: true },
    ] });

    emit({ type: 'power.update', batteries: [], on_battery: false,
      lid_closed: false });
    check('and no battery takes the widget off the bar',
      el.textContent === '');
    emit({ type: 'config', layout: mode });
  }

  /* The power rows. The four that sit below any profiles — suspend, power
     off, reboot, quit — and the two of them this compositor is the session
     for. Drawn with no profiles on offer, because that is the desk that has
     nothing to choose but a verb: a lid set to off and no battery daemon is
     still a machine. */
  {
    emit({ type: 'config', layout: mode, bar_widgets: [{ type: 'battery' }] });
    emit({ type: 'power.update', batteries: [], on_battery: false,
      lid_closed: false, profiles: [] });

    /* From a shell command, the verb the battery module click sends — so a
       desk that has no touch screen can bring the picker up the same way. */
    emit({ type: 'shell.command', command: 'power', args: [] });
    check('a "power" shell command opens the picker',
      globalThis.__shell.powerEl.hidden === false);

    /* The profile rows (when any) live inside the `.power-list` the dialog
       holds; the action rows are the dialog's own `.power-row` children,
       drawn after the divider. */
    const dialog = globalThis.__shell.powerEl.children[0];
    const actions = dialog.children.filter((el) => el._classes.has('power-row'));
    check('below the profiles it always draws the five power rows',
      actions.length === 5 &&
        actions.map((r) => r.textContent).join(' ') ===
          'Lock Suspend Power Off Reboot Quit');
    check('the ones that end the machine wear their colour, and only those',
      actions.filter((r) => r._classes.has('danger'))
        .map((r) => r.textContent).join(' ') ===
        'Power Off Reboot Quit' &&
        !actions[0]._classes.has('danger') &&
        !actions[1]._classes.has('danger'));

    /* Lock is not a logind verb. What locking means is the compositor's
       answer — a locker named in the config file, or the shell's own lock
       screen — and this row says nothing about which. */
    let before = sent.length;
    (actions[0].listeners.click ?? []).forEach((fn) =>
      fn({ preventDefault() {}, stopPropagation() {} }));
    check('lock asks the compositor to lock, not logind to do something',
      sent.slice(before).some((m) => m.type === 'session.lock') &&
        !sent.slice(before).some((m) => m.type === 'power.action'));
    check('and it takes the picker down',
      globalThis.__shell.powerEl.hidden === true);

    emit({ type: 'shell.command', command: 'power', args: [] });

    before = sent.length;
    (actions[1].listeners.click ?? []).forEach((fn) =>
      fn({ preventDefault() {}, stopPropagation() {} }));
    check('suspend hands its verb to the compositor',
      sent.slice(before).some((m) => m.type === 'power.action' &&
        m.action === 'suspend'));
    check('and it takes the picker down too',
      globalThis.__shell.powerEl.hidden === true);

    before = sent.length;
    (actions[2].listeners.click ?? []).forEach((fn) =>
      fn({ preventDefault() {}, stopPropagation() {} }));
    check('power off is the same row, second word',
      sent.slice(before).some((m) => m.type === 'power.action' &&
        m.action === 'poweroff'));

    before = sent.length;
    (actions[3].listeners.click ?? []).forEach((fn) =>
      fn({ preventDefault() {}, stopPropagation() {} }));
    check('reboot goes out the same way',
      sent.slice(before).some((m) => m.type === 'power.action' &&
        m.action === 'reboot'));

    before = sent.length;
    (actions[4].listeners.click ?? []).forEach((fn) =>
      fn({ preventDefault() {}, stopPropagation() {} }));
    check('quit is a message of its own, not a fifth power verb',
      sent.slice(before).some((m) => m.type === 'quit') &&
        !sent.slice(before).some((m) => m.type === 'power.action'));
    emit({ type: 'config', layout: mode });
  }

/* The lock screen.
 *
 * The one surface here whose failure mode is not "the desktop looks wrong".
 * Three things are worth stating exactly, and the rest of this block is
 * arranging for them to be observable:
 *
 * The page never decides anything. It draws when the compositor says the
 * session is locked, it stops when the compositor says it is not, and a
 * password goes out as a question rather than an answer.
 *
 * It says when it has drawn. `session.lock.drawn` carries the generation it
 * was told, and the compositor draws none of this page on a locked screen
 * until it has arrived — see `lock_screen_is_drawing` on the Rust side. The
 * harness runs `requestAnimationFrame` synchronously, so the double frame the
 * real page waits out lands inside `emit` here.
 *
 * And it answers for the lock it is on and no other. A message naming a lock
 * that has ended is a message from before a shell restart, and acting on one
 * is how a stale error lands on a screen somebody is typing at.
 */
{
  const lockEl = globalThis.__shell.lockEl;
  const pane = () => lockEl.children[0];
  const field = () => pane()?.querySelector('lock-input');
  const messageEl = () => pane()?.querySelector('lock-message');

  let before = sent.length;
  emit({ type: 'session.lock', generation: 7, can_authenticate: true });
  check('a lock puts the lock screen up', lockEl.hidden === false);
  check('and it says so, naming the lock it was told about',
    sent.slice(before).some((m) => m.type === 'session.lock.drawn' &&
      m.generation === 7));
  check('with a clock and a password box on the screen being looked at',
    !!pane()?.querySelector('lock-time') && !!field());
  check('and a keyboard to type into it with, for a desk that has none',
    !!pane()?.querySelector('lock-keyboard'));

  /* Typing. The password goes out with the generation; nothing about whether
     it was right is decided here. */
  before = sent.length;
  field().value = 'hunter2';
  (field().listeners.keydown ?? []).forEach((fn) =>
    fn({ key: 'Enter', preventDefault() {}, stopPropagation() {} }));
  const attempt = sent.slice(before).find((m) => m.type === 'session.unlock');
  check('Enter asks the compositor, rather than deciding anything',
    !!attempt && attempt.generation === 7 && attempt.password === 'hunter2');

  /* A refusal. The words are the compositor's — PAM's, really — because
     "wrong password" and "your account has expired" are different problems. */
  emit({ type: 'session.lock.error', generation: 7, message: 'Sorry, try again.' });
  check('a refusal is shown in the words it came with',
    messageEl()?.textContent === 'Sorry, try again.');
  check('and the box is emptied rather than left half-typed over',
    field().value === '');
  check('and the screen is still up', lockEl.hidden === false);

  /* A refusal for a lock that is over. This is what a message from before a
     shell restart looks like, and acting on it would put somebody else's
     error on the screen in front of the person typing. */
  emit({ type: 'session.lock.error', generation: 6, message: 'stale' });
  check('a refusal naming a lock that is over is ignored',
    messageEl()?.textContent === 'Sorry, try again.');

  /* Every monitor gets a pane. A screen showing nothing while another shows a
     lock screen reads as a screen that has died — and, on the compositor's
     side, an output the page does not cover is drawn black rather than left
     showing the desktop, so a pane is the only thing that says what happened
     on it. */
  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 2560, height: 1440,
      usable_x: 0, usable_y: 30, usable_width: 2560, usable_height: 1410,
      scale: 1, transform: 'normal', modes: [], enabled: true },
    { name: 'DP-2', x: 2560, y: 0, width: 2560, height: 1440,
      usable_x: 2560, usable_y: 30, usable_width: 2560, usable_height: 1410,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });
  emit({ type: 'shell.command', command: 'output.focus', args: ['DP-1'] });
  emit({ type: 'session.lock', generation: 7, can_authenticate: true });
  check('every monitor gets a pane', lockEl.children.length === 2);
  check('and each one is docked to its own corner of the layout',
    lockEl.children[1].style.left === '2560px');
  const boxes = lockEl.children.filter((el) => !!el.querySelector('lock-input'));
  check('but only one password box, on the screen being looked at',
    boxes.length === 1);

  /* A machine whose PAM could not be loaded still locks. It says so rather
     than silently swallowing every password, because the alternative is
     somebody typing the right one twenty times. */
  emit({ type: 'session.lock', generation: 8, can_authenticate: false });
  check('a session that cannot check a password locks anyway',
    lockEl.hidden === false);
  check('and says so instead of swallowing every attempt',
    (pane()?.querySelector('lock-message')?.textContent ?? '')
      .includes('cannot check a password'));

  emit({ type: 'session.unlock' });
  check('the compositor ends it, and the screen goes', lockEl.hidden === true);
  check('leaving nothing of it behind', lockEl.children.length === 0);

  /* Back to one output for everything after this, which every later section
     assumes. */
  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
      usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });
}

  /* A full bar override, `bar_items`: modules and widgets listed together in
     whatever order the config wants them drawn. Unlike bar_widgets, this
     replaces the whole right side — the built-in modules that are not listed
     are not drawn, and a widget can sit in the middle of the modules. */
  emit({ type: 'config', layout: mode,
    bar_items: [
      'net',
      { type: 'disk', path: '/games' },
      'clock',
      { type: 'weather', location: 'Pickering, ON, Canada' },
    ] });
  check('a bar_items override draws one element per item',
    wout.barItemsEls.length === 4);
  check('the override replaces the shipped modules, none left behind',
    (() => {
      const right = wout.barEl.querySelector('.bar-right');
      return right.children.length === 4 &&
        right.children.length === wout.barItemsEls.length;
    })());
  check('a bare string becomes a module element',
    wout.barItemsEls[0].className === 'module net');
  check('a widget object becomes a widget element',
    wout.barItemsEls[1].className === 'module widget');
  check('modules and widgets stay in the config order',
    wout.barItemsEls[1].dataset.widget === 'disk:/games');
  check('the default modules are replaced, not added to',
    wout.modules.cpu === undefined && wout.modules.clock !== undefined);
  emit({ type: 'status.update', cpu: -1, memory: -1, load: 0,
    net_rx: 10, net_tx: 20, disk_free: 0, disk_total: 0,
    mounts: [{ path: '/games', free: 1000000, total: 2000000 }],
    volume: -1, muted: false });
  check('an override widget renders from the status sample',
    wout.barItemsEls[1].textContent.includes('/games'));

  /* The keys on an empty desktop, drawn from what the compositor says is
     really bound.
     From the compositor and not from a table here, because the keymap is not
     knowable from this side: a few chords exist only in one layout and a
     config file may add or shadow any of them, so a list written in the shell
     would be describing a keyboard nobody has — and would be wrong in exactly
     the case someone is most likely to be reading it. */
  {
    emit({ type: 'config', layout: mode, binds: [
      { chord: 'Mod4+Return', action: 'exec foot' },
      { chord: 'Mod4+Shift+q', action: 'close' },
      { chord: 'Mod4+bracketleft', action: 'shell canvas.pan left' },
      { chord: 'h', action: 'shell layout.resize left', mode: 'resize' },
    ] });
    check('the shell keeps the keymap it was sent',
      globalThis.__shell.keybindsForTest().length === 4);

    const keys = globalThis.__shell.outputs.get('DP-1')
      .emptyEl.querySelector('.keys');
    const rows = keys.children;
    check('and draws one row per chord in the ordinary keymap',
      rows.length === 3);
    check('with the chord and what it does, both as a config file spells them',
      rows[0].children[0].textContent === 'Mod4+Return'
      && rows[0].children[1].textContent === 'exec foot');
    check('a binding mode is left out: its keys are not live right now',
      [...rows].every((row) =>
        row.children[0].textContent !== 'h'));

    /* A compositor that sends no keymap leaves the two lines the markup ships
       with, rather than emptying the box: a shell that cannot say what the
       keys are should still say the two that start a terminal and a menu. */
    const before = keys.children.length;
    emit({ type: 'config', layout: mode });
    check('and a config with no keymap changes nothing',
      keys.children.length === before);
  }

  /* The engine's own right-click menu, turned off.
     The shell is drawn by a browser and a browser offers back, reload, view
     source and save image on a right-click. On a desktop background that menu
     is nonsense, and it appears over the windows because it is the engine's
     surface rather than anything the compositor placed. The right button here
     belongs to the compositor — Mod4 and it resize a window — or to whatever
     the pointer is over. */
  {
    const handlers = documentListeners.contextmenu ?? [];
    check('the shell listens for the engine\'s context menu',
      handlers.length > 0);
    let prevented = false;
    for (const fn of handlers) fn({ preventDefault: () => { prevented = true; } });
    check('and refuses it', prevented);
  }

  /* The weather widget's line: the condition first, then the temperature —
     the glyph labels the number rather than trailing it. */
  const sunny = globalThis.__shell.weatherLineForTest(0, 21.4);
  check('a weather line leads with the condition glyph',
    sunny.startsWith('☀') && sunny.endsWith('21°C'));
  check('a code with no glyph leaves no leading space',
    globalThis.__shell.weatherLineForTest(85, -3.2) === '-3°C');

  /* Widgets carry input, sent to the compositor so it can run the command
     the widget stands for. The volume widget scrolls in 5% steps and a right
     click toggles mute; the disk widget opens its mount; a module element
     (a bare string in the override) does none of this. */
  const execAfter = () => sent.filter((m) => m.type === 'shell.exec');

  emit({ type: 'config', layout: mode, bar_items: [
    { type: 'volume' }, { type: 'disk', path: '/games' }, 'clock',
  ] });
  const volEl = wout.barItemsEls[0];
  const diskEl = wout.barItemsEls[1];
  const clockEl = wout.barItemsEls[2];

  const before = execAfter().length;
  const sentBeforeScroll = sent.length;
  volEl.listeners.wheel.forEach((fn) =>
    fn({ preventDefault() {}, deltaY: -100 }));
  /* One message, not two. The pair this replaced — `shell.exec wpctl …`
     followed by `status.refresh` — is a race the refresh wins: the compositor
     spawns the command and samples the sink before it has run, so the bar
     redraws the volume that was already there. */
  const volumeAfter = (from) => sent.slice(from)
    .filter((m) => m.type === 'status.volume');
  check('scrolling a volume widget up raises volume by 5%',
    volumeAfter(sentBeforeScroll).some((m) =>
      m.target === 'sink' && m.delta === 5 && !m.mute));
  check('and asks for it in one message, so the sample cannot come first',
    execAfter().length === before
    && !sent.slice(sentBeforeScroll).some((m) => m.type === 'status.refresh'));
  const beforeMute = execAfter().length;
  const sentBeforeMute = sent.length;
  volEl.listeners.contextmenu.forEach((fn) => fn({ preventDefault() {} }));
  check('right-clicking a volume widget toggles mute',
    volumeAfter(sentBeforeMute).some((m) => m.target === 'sink' && m.mute === true));
  check('and does not run a program to do it',
    execAfter().length === beforeMute);
  const beforeDisk = execAfter().length;
  diskEl.listeners.click.forEach((fn) => fn());
  check('clicking a disk widget opens its mount',
    execAfter().slice(beforeDisk).some((m) =>
      m.command.includes('xdg-open') && m.command.includes('/games')));
  const beforeClock = execAfter().length;
  clockEl.listeners.click.forEach((fn) =>
    fn({ preventDefault() {}, stopPropagation() {} }));
  clockEl.listeners.wheel.forEach((fn) =>
    fn({ preventDefault() {}, deltaY: 100 }));
  check('a module (non-widget) element runs no program on click or scroll',
    execAfter().length === beforeClock);
  /* The clock is the module that does answer a click, in the override path as
     well as in the shipped markup: it opens the calendar under itself. Nothing
     is spawned to do it — the date is arithmetic this page can do — which is
     what the check above still says. */
  check('but the clock module opens the calendar under it',
    globalThis.__shell.calendarEl.hidden === false);
  documentListeners.click.forEach((fn) => fn({ preventDefault() {} }));

  emit({ type: 'config', layout: mode });

  /* The empty desktop's two parts, switched from the config file. */
  const root = document.documentElement.classList;
  emit({ type: 'config', layout: mode, logo: false, tutorial: false });
  check('logo: false hides the mark', root.contains('no-logo'));
  check('tutorial: false hides the note', root.contains('no-tutorial'));

  emit({ type: 'config', layout: mode, logo: true, tutorial: true });
  check('turning them back on shows them again',
    !root.contains('no-logo') && !root.contains('no-tutorial'));

  emit({ type: 'config', layout: mode });
  check('a config that says nothing leaves them on',
    !root.contains('no-logo') && !root.contains('no-tutorial'));

  /* A terminal drawn behind the page.
   *
   * The compositor draws it under the shell's own buffer, so it is visible
   * only while the page paints no background of its own. `behind` is the whole
   * of the shell's part in that, and getting it wrong is invisible in a
   * screenshot of the shell alone — the page looks exactly the same, and the
   * terminal underneath is simply never seen. */
  emit({ type: 'config', layout: mode, background_terminal: true });
  check('background_terminal makes room for what is behind the page',
    root.contains('behind'));

  emit({ type: 'config', layout: mode, background_terminal: false });
  check('turning it off paints the wallpaper again', !root.contains('behind'));

  /* Absent means off, and the compositor omits the key when it is: a desktop
   * with no terminal behind it must not go transparent onto the clear colour,
   * which is a black screen with a bar on it. */
  emit({ type: 'config', layout: mode });
  check('a config that omits the key keeps the wallpaper',
    !root.contains('behind'));
}

/* Moving a window to another workspace, for both kinds of window.
 *
 * There were two implementations of this — the Mod4+Shift+N binding and the
 * overview drag — and only the second one knew that floating windows exist.
 * The binding looked the window up in the tiling tree, did not find it, and
 * returned, so a floating window could be dragged between thumbnails but not
 * sent anywhere with the keyboard.
 *
 * Fullscreen is recorded per workspace rather than per window, so it is the
 * other thing a move has to carry: left behind, the workspace the window came
 * from goes on claiming a fullscreen window it no longer has.
 */
{
  const views = globalThis.__shell.views;
  const ws = globalThis.__shell.workspaceOfForTest;
  const open = (id, app, floating = false) => emit({ type: 'view.added', id,
    title: app, app_id: app, output: 'DP-1', min_width: 0, min_height: 0,
    floating, width: 800, height: 600 });

  /* Floated by the compositor's own signal rather than by a config rule: the
     rules declared at the top of this file are gone by now, replaced by the
     later `config` messages the bar tests send. */
  open(90, 'floaty-mover', true);
  check('the test opened a floating window',
    globalThis.__shell.floatingForTest(90) !== null);

  /* The compositor cannot draw a floating window's border above the windows
     under it without being told where that border is. */
  const framed = sent.filter((m) => m.type === 'view.layout' && m.id === 90).at(-1);
  check('a floating window reports its frame',
    framed !== undefined && framed.frame !== undefined &&
    framed.frame.width > 0 && framed.frame.height > 0);

  /* Stacking belongs to the compositor, and floating is the part of it only
     the shell knows. Without the flag a click on a tiled window raises it over
     the dialog that was sitting on top. */
  check('a floating window says so on its layout',
    framed !== undefined && framed.floating === true);

  /* A tiled border falls in the gap between two windows, where nothing covers
     it, so reporting one would be a texture drawn per window for nothing.
     Not in solar, where a window is either the middle one — which does sit
     over the others and does need both — or is in an orbit that nothing
     overlaps; there is no window in that layout that is tiled in this sense.
     Nor on the canvas, for the plainer version of the same reason: windows
     there are placed by hand and overlap by design, so the focused one is
     lifted over whatever it covers and its border falls inside that window's
     hole rather than in a gap. */
  if (mode !== 'solar' && mode !== 'canvas') {
    open(93, 'tiled-one');
    const tiled = sent.filter((m) => m.type === 'view.layout' && m.id === 93).at(-1);
    check('a tiled window reports none',
      tiled !== undefined && tiled.frame === undefined);
    /* Absent means tiled, so nothing is sent for the ordinary case. */
    check('and does not claim to float',
      tiled !== undefined && tiled.floating === undefined);
    emit({ type: 'view.removed', id: 93 });
  }

  /* Mod4 and the left button on a *tiled* window did nothing at all: the
     handler returned early unless the window already floated.

     Not on the canvas, where every window already has a rectangle of its own
     and a drag edits that: floating one there would write a rect the layout
     does not read, which is a window that does not move however far it is
     dragged. The canvas section above checks the drag it does do. */
  if (mode !== 'canvas') {
    open(94, 'tiled-drag');
    check('the test opened a tiled window',
      globalThis.__shell.floatingForTest(94) === null);
    emit({ type: 'shell.command', command: 'layout.move.delta',
      args: ['94', '30', '20'] });
    const dragged = globalThis.__shell.floatingForTest(94);
    check('dragging a tiled window floats it', dragged !== null);
    /* And it carries on from where it was rather than jumping to the middle of
       the screen on the first pixel of the drag. */
    check('and it moves by what was dragged',
      dragged !== null && dragged.x !== 0 && dragged.y !== 0);
    emit({ type: 'view.removed', id: 94 });

    /* Resize mode looks the focused window up in the tree, and a floating
       window is not in it — so every press was ignored. On the canvas there is
       no resize mode at all: nothing shares space with anything, so there is
       no neighbour to take it from and the gesture is a drag instead. */
    emit({ type: 'view.focused', id: 90 });
    const before = globalThis.__shell.floatingForTest(90).width;
    emit({ type: 'shell.command', command: 'layout.resize', args: ['right'] });
    const after = globalThis.__shell.floatingForTest(90).width;
    check('resize mode grows a floating window', after > before);
    emit({ type: 'shell.command', command: 'layout.resize', args: ['left'] });
    check('and shrinks it again',
      globalThis.__shell.floatingForTest(90).width === before);
  }

  emit({ type: 'view.focused', id: 90 });
  const floatFrom = ws(90);
  const floatTo = floatFrom === 4 ? 5 : 4;
  emit({ type: 'shell.command', command: 'workspace.move',
    args: [String(floatTo)] });
  check('the keybinding moves a floating window between workspaces',
    ws(90) === floatTo);

  /* And a tiled one, fullscreen, which must take that state with it. */
  open(91, 'tiled-mover');
  emit({ type: 'view.focused', id: 91 });
  const from = ws(91);
  emit({ type: 'shell.command', command: 'window.fullscreen.set',
    args: ['91', '1'] });
  check('the window is fullscreen where it started',
    globalThis.__shell.fullscreenOnForTest(from) === 91);

  const to = from === 8 ? 9 : 8;
  emit({ type: 'shell.command', command: 'workspace.move', args: [String(to)] });
  check('a tiled window moves too', ws(91) === to);
  check('fullscreen follows it',
    globalThis.__shell.fullscreenOnForTest(to) === 91);
  check('and is not left behind on the workspace it came from',
    globalThis.__shell.fullscreenOnForTest(from) === null);

  /* Not on the canvas, where a plane has no edge to reach: window.move slides
     the window across it and the view follows, so there is no fall-through to
     moveViewToOutput() and nothing to carry. A window gets to the other
     monitor there the way it does in any layout — by changing workspace. */
  if (mode !== 'canvas') {
    /* The same window carried to the next monitor, which is the other way a
       window changes workspace: at the edge of its own tree the move falls
       through to moveViewToOutput(), and that path used to move the leaf and
       leave the fullscreen record behind — bar hidden on a workspace whose
       fullscreen window is on the other screen. */
    emit({ type: 'output.layout', outputs: [
      { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
        usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
        scale: 1, transform: 'normal', modes: [], enabled: true },
      { name: 'DP-2', x: 1920, y: 0, width: 1920, height: 1080,
        usable_x: 1920, usable_y: 30, usable_width: 1920, usable_height: 1050,
        scale: 1, transform: 'normal', modes: [], enabled: true },
    ] });

    /* Which monitor is to the right of which is read off the elements, and
       the stub gives every element the same rect until it is told otherwise. */
    globalThis.__shell.outputs.get('DP-1').el.__rect =
      { left: 0, top: 0, width: 1920, height: 1080 };
    globalThis.__shell.outputs.get('DP-2').el.__rect =
      { left: 1920, top: 0, width: 1920, height: 1080 };

    const start = ws(91);
    emit({ type: 'view.focused', id: 91 });
    emit({ type: 'shell.command', command: 'window.move', args: ['right'] });
    const landed = ws(91);
    check('a window at the edge is carried to the next monitor',
      landed !== start);
    check('fullscreen goes with it across monitors',
      globalThis.__shell.fullscreenOnForTest(landed) === 91);
    check('and the workspace it left is no longer fullscreen',
      globalThis.__shell.fullscreenOnForTest(start) === null);

    emit({ type: 'output.layout', outputs: [
      { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
        usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
        scale: 1, transform: 'normal', modes: [], enabled: true },
    ] });
  }

  emit({ type: 'view.removed', id: 90 });
  emit({ type: 'view.removed', id: 91 });
}

/* --- the settings panel ------------------------------------------------
 *
 * The panel is drawn entirely out of the last `config` event and the last
 * `output.layout`, and every control it draws sends a message. So what there
 * is to check is exactly those two halves: that a value the compositor stated
 * is the value on the switch, and that pressing the switch sends the message
 * the compositor would act on.
 *
 * The display half is worth more than the rest put together, because it is the
 * one that can leave somebody looking at a screen they cannot read: a change
 * has to raise the confirm bar, and the bar's two buttons have to send the two
 * messages that end the compositor's countdown.
 * --------------------------------------------------------------------- */

{
  /* Everything under `root` with this class, which is what a test needs and
     the stub's querySelector — one element, first match — does not give. */
  const all = (root, cls) => {
    const found = [];
    const stack = [...root.children];
    while (stack.length) {
      const el = stack.shift();
      if (el._classes.has(cls)) found.push(el);
      stack.push(...el.children);
    }
    return found;
  };
  const click = (el) => (el.listeners.click ?? []).forEach((fn) =>
    fn({ preventDefault() {}, stopPropagation() {} }));
  const dialog = () => globalThis.__shell.settingsEl.children[0];
  /* The row whose label reads this, as the controls in it. */
  const controls = (label) => all(dialog(), 'settings-row')
    .find((row) => row.children[0]?.textContent === label)
    ?.children[1]?.children ?? [];

  /* A desktop with an opinion about every one of these, so the panel is being
     read rather than guessed at: an absent field would leave the shell's own
     default on the switch and the assertion would pass against nothing. */
  emit({ type: 'config',
    layout: mode,
    rules: HARNESS_RULES,
    gaps: { inner: 12, outer: 3, smart: true },
    border: { radius: 9, width: 4, smart: false },
    wallpaper: 'file:///pic/w.png',
    wallpaper_mode: 'tile',
    dark_mode: false });
  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', make: '', model: 'Screen', serial: '',
      x: 0, y: 0, width: 1920, height: 1080,
      usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', enabled: true,
      modes: [
        { width: 1920, height: 1080, refresh: 60000,
          preferred: true, current: true },
        { width: 1920, height: 1080, refresh: 143998,
          preferred: false, current: false },
      ] },
  ] });

  let before = sent.length;
  emit({ type: 'shell.command', command: 'settings', args: [] });
  check('a "settings" shell command opens the panel',
    globalThis.__shell.settingsEl.hidden === false);
  check('and takes the keyboard, because its fields are real text boxes',
    sent.slice(before).some((m) => m.type === 'shell.focus'));
  check('and asks what the monitors are now rather than trusting an old layout',
    sent.slice(before).some((m) => m.type === 'output.query'));

  /* Read, not guessed: every one of these is a number the compositor stated
     and none of them is the shell's own default. */
  check('the gaps are drawn from what the compositor last said',
    controls('Between windows')[0]?.value === '12' &&
      controls('Around the edge')[0]?.value === '3');
  check('and so is the border',
    controls('Corner radius')[0]?.value === '9' &&
      controls('Thickness')[0]?.value === '4');
  check('a boolean is drawn as what it is, not as what it would become',
    controls('Drop for a lone window')[0]?.textContent === 'On' &&
      controls('Square a lone window')[0]?.textContent === 'Off');
  check('the wallpaper is the URL the compositor resolved',
    controls('Picture')[0]?.value === 'file:///pic/w.png');
  check('and the fitting it is really using is the one marked',
    all(dialog(), 'settings-option')
      .filter((o) => o._classes.has('active'))
      .some((o) => o.textContent === 'tile'));
  check('dark mode is off, because the compositor said so',
    controls('Dark applications')[0]?.textContent === 'Off');

  before = sent.length;
  click(controls('Dark applications')[0]);
  check('the switch sends the state it wants rather than a toggle',
    sent.slice(before).some((m) => m.type === 'config.dark_mode' &&
      m.enabled === true));

  /* Committed on change rather than on every keystroke: typing 20 over 12
     passes through 2, and a two-pixel gap applied for as long as it takes to
     type the second digit is a desktop that jumps while being configured. */
  before = sent.length;
  const inner = controls('Between windows')[0];
  inner.value = '20';
  (inner.listeners.change ?? []).forEach((fn) => fn({}));
  check('a committed number goes out as the setter it belongs to',
    sent.slice(before).some((m) => m.type === 'config.gaps' && m.inner === 20));

  before = sent.length;
  const negative = controls('Corner radius')[0];
  negative.value = '-4';
  (negative.listeners.change ?? []).forEach((fn) => fn({}));
  check('a negative one is refused in the box rather than by the compositor',
    !sent.slice(before).some((m) => m.type === 'config.border') &&
      negative.value === '9');

  before = sent.length;
  click(all(dialog(), 'settings-option').find((o) => o.textContent === 'fit'));
  check('a fitting is sent without naming the picture again',
    sent.slice(before).some((m) => m.type === 'config.wallpaper' &&
      m.mode === 'fit' && m.path === undefined));

  /* The displays. A mode list the compositor sent, drawn as a list of modes
     rather than a list of pixel sizes: 60 Hz and 143.998 Hz at the same size
     are two different modes and a panel that shows one row for them is a
     panel that cannot select the other. */
  const select = all(dialog(), 'settings-select')[0];
  check('every mode the display offers is on the list',
    select?.children.length === 2 &&
      select.children[0].textContent === '1920×1080 @ 60.0 Hz (preferred)' &&
      select.children[1].textContent === '1920×1080 @ 144.0 Hz');
  check('and the one it is in is the one selected',
    select.value === '1920x1080@60000');

  before = sent.length;
  select.value = '1920x1080@143998';
  (select.listeners.change ?? []).forEach((fn) => fn({ stopPropagation() {} }));
  check('choosing one sends the three numbers, not an index into the list',
    sent.slice(before).some((m) => m.type === 'output.configure' &&
      m.name === 'DP-1' && m.mode?.width === 1920 &&
      m.mode?.refresh === 143998));
  check('and the panel asks whether the screen came back',
    all(dialog(), 'settings-confirm').length === 1);

  before = sent.length;
  click(all(dialog(), 'settings-button').find((b) => b.textContent === 'Keep'));
  check('Keep is what ends the compositor\'s countdown',
    sent.slice(before).some((m) => m.type === 'output.confirm'));
  check('and the question goes away with it',
    all(dialog(), 'settings-confirm').length === 0);

  before = sent.length;
  click(all(dialog(), 'settings-option').find((o) => o.textContent === '90°'));
  check('a rotation is the same provisional change',
    sent.slice(before).some((m) => m.type === 'output.configure' &&
      m.transform === '90') &&
      all(dialog(), 'settings-confirm').length === 1);

  before = sent.length;
  click(all(dialog(), 'settings-button')
    .find((b) => b.textContent === 'Revert'));
  check('and Revert takes it back without waiting out the deadline',
    sent.slice(before).some((m) => m.type === 'output.revert'));

  /* Saving. The panel says where, because the overlay file is a thing
     somebody may want to go and look at — or delete, which is how the config
     file is put back in charge. */
  before = sent.length;
  click(dialog().querySelector('.settings-save'));
  check('Save asks the compositor to write the settings down',
    sent.slice(before).some((m) => m.type === 'config.save'));
  emit({ type: 'config.saved', path: '/home/me/.config/viewport/settings.json' });
  check('and says where they went',
    dialog().querySelector('.settings-hint').textContent ===
      'Saved to /home/me/.config/viewport/settings.json');

  /* And a config message from anywhere — an editor saving the config file,
     another client on the socket — redraws the switches under the hand. */
  emit({ type: 'config', layout: mode, rules: HARNESS_RULES,
    gaps: { inner: 40, outer: 3, smart: true }, dark_mode: true });
  check('a change from outside the panel reaches the panel',
    controls('Between windows')[0]?.value === '40' &&
      controls('Dark applications')[0]?.textContent === 'On');

  /* Escape closes it, from wherever the caret ended up. The only global key
     handler in the shell, and the panel is the only surface it is for. */
  (documentListeners.keydown ?? []).forEach((fn) =>
    fn({ key: 'Escape', preventDefault() {} }));
  check('Escape closes the panel',
    globalThis.__shell.settingsEl.hidden === true);

  (documentListeners.keydown ?? []).forEach((fn) =>
    fn({ key: 'Escape', preventDefault() {} }));
  check('and pressing it again with nothing open does nothing at all',
    globalThis.__shell.settingsEl.hidden === true);

  /* Put the desktop back the way the sections after this one expect it.
     This block replaced the window rules and the monitor's mode list, and a
     config message replaces the rules wholesale.

     The four custom properties are removed by hand rather than sent as a
     config message, because there is no message that removes one: a config
     that omits `gaps` leaves whatever the last one set — deliberately, so a
     reload cannot silently reset a value — and the stylesheet section further
     down checks what `shell.css` itself declares, which is only visible with
     nothing overriding it. */
  for (const property of ['--gap', '--gap-outer', '--window-radius',
    '--window-border']) {
    document.documentElement.style.removeProperty(property);
  }
  emit({ type: 'config', layout: mode, rules: HARNESS_RULES,
    gaps: { smart: false }, border: { smart: false } });
  emit({ type: 'output.layout', outputs: [
    { name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
      usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
      scale: 1, transform: 'normal', modes: [], enabled: true },
  ] });
}

/* --- the screen-share chooser ------------------------------------------
 *
 * Drawn here and decided in the compositor: the highlight arrives in the
 * message rather than being moved by a key, because the shell receives no
 * input of its own. So what these check is that the shell draws exactly what
 * it was told, and takes it down when it is told to.
 * --------------------------------------------------------------------- */

{
  const rows = () => {
    const dialog = screencastEl.children[0];
    if (!dialog) return [];
    const list = dialog.children.find((c) => c._classes.has('screencast-list'));
    return list ? list.children : [];
  };
  const highlighted = () => rows().findIndex((r) => r._classes.has('selected'));
  const label = (row) => row.children[0].textContent;

  emit({ type: 'screencast.pick', id: 7, selected: 0, sources: [
    { kind: 'window', label: 'a terminal', detail: 'foot' },
    { kind: 'window', label: '', detail: 'firefox' },
    { kind: 'output', label: 'DP-1', detail: 'Dell U2720Q' },
  ] });

  check('the chooser is up', screencastEl.hidden === false);
  check('one row per source', rows().length === 3);
  check('the first is highlighted', highlighted() === 0);
  check('a window is named by its title', label(rows()[0]) === 'a terminal');
  /* A row with no text in it reads as a bug rather than as a choice. */
  check('an untitled window still says something',
    label(rows()[1]) === 'an untitled window');
  check('a monitor is marked as one', rows()[2].dataset.kind === 'output');

  /* The three sources that name nothing in particular. The compositor names
     them — there is no client title to fall back from — so the shell's job is
     to draw them as rows like any other and to keep the kind on the element,
     which is the only thing the stylesheet has to tell them apart by. */
  emit({ type: 'screencast.pick', id: 7, selected: 0, sources: [
    { kind: 'window', label: 'a terminal', detail: 'foot' },
    { kind: 'follow-window', label: 'The focused window',
      detail: 'follows as you switch windows' },
    { kind: 'all-outputs', label: 'All monitors', detail: '2 screens, side by side' },
    { kind: 'follow-output', label: 'The active monitor',
      detail: 'follows as you move between screens' },
    /* A kind from a newer compositor than this shell. Drawing nothing would
       hide a source the user could otherwise have picked. */
    { kind: 'something-new', label: 'A tablet', detail: '' },
  ] });
  check('every kind gets a row', rows().length === 5);
  check('a following window says so', label(rows()[1]) === 'The focused window');
  check('and is marked as following', rows()[1].dataset.kind === 'follow-window');
  check('the whole desk is offered', label(rows()[2]) === 'All monitors');
  check('a following monitor says so', label(rows()[3]) === 'The active monitor');
  check('and what it will follow is under it',
    rows()[3].children[1].textContent === 'follows as you move between screens');
  check('an unknown kind is still drawn', label(rows()[4]) === 'A tablet');
  /* Only the two kinds that borrow a client's text can arrive unnamed. */
  emit({ type: 'screencast.pick', id: 7, selected: 0, sources: [
    { kind: 'something-new', label: '', detail: '' },
  ] });
  check('an unknown kind with no name still says something',
    label(rows()[0]) === 'something to share');

  /* The compositor moved the highlight and re-sent the list, which is the
     whole of the interaction: there is no state here to move. */
  emit({ type: 'screencast.pick', id: 7, selected: 2, sources: [
    { kind: 'window', label: 'a terminal', detail: 'foot' },
    { kind: 'window', label: '', detail: 'firefox' },
    { kind: 'output', label: 'DP-1', detail: 'Dell U2720Q' },
  ] });
  check('the highlight moved', highlighted() === 2);
  check('and only one row has it',
    rows().filter((r) => r._classes.has('selected')).length === 1);

  /* An answer for a request that is already dealt with must not take down the
     chooser that replaced it. */
  emit({ type: 'screencast.pick.done', id: 6 });
  check('a stale answer leaves it alone', screencastEl.hidden === false);

  /* The compositor cannot draw the chooser above the windows without being
     told which part of the shell it is. */
  /* `shell.overlay` carries every rectangle that floats above the windows —
     the chooser here, a notification elsewhere — so the check is that one of
     them is the dialog rather than that the message exists. */
  const overlay = sent.filter((m) => m.type === 'shell.overlay').at(-1);
  check('the shell says where the dialog is',
    overlay !== undefined
      && overlay.rects.some((r) => r.width > 0 && r.height > 0));

  emit({ type: 'screencast.pick.done', id: 7 });
  check('the chooser goes away when it is answered',
    screencastEl.hidden === true);
  check('and takes its rows with it', screencastEl.children.length === 0);
  /* Otherwise the compositor goes on drawing that piece of the shell over
     whatever is there now. */
  const gone = sent.filter((m) => m.type === 'shell.overlay').at(-1);
  check('and tells the compositor there is nothing on top now',
    gone !== undefined && gone.rects.length === 0);

  /* The third question through the same dialog: keys an application wants to
     hear while something else has focus. There is nothing to choose between,
     so there is no highlight and the list is sent once. */
  emit({ type: 'shortcuts.pick', id: 9, app: 'Discord', shortcuts: [
    { id: 'talk', description: 'push to talk', trigger: 'Mod4+grave' },
    { id: 'mute', description: '', trigger: 'Mod4+Shift+m' },
  ] });
  check('the shortcut dialog is up', screencastEl.hidden === false);
  check('one row per shortcut', rows().length === 2);
  /* The chord is what the person compares against their own keyboard, so it
     is the row's headline rather than the description. */
  check('a row is headed by the chord', label(rows()[0]) === 'Mod4+grave');
  check('with what it is for under it',
    rows()[0].children[1].textContent === 'push to talk');
  /* An application that will not say what a key is for is worth seeing as
     exactly that. */
  check('an unexplained shortcut says so',
    rows()[1].children[1].textContent === 'no reason given');
  /* Nothing is being chosen between: a highlight would say otherwise. */
  check('nothing is highlighted', highlighted() === -1);
  const shortcutOverlay = sent.filter((m) => m.type === 'shell.overlay').at(-1);
  check('the shell says where the shortcut dialog is',
    shortcutOverlay !== undefined
      && shortcutOverlay.rects.some((r) => r.width > 0 && r.height > 0));

  emit({ type: 'shortcuts.pick.done', id: 8 });
  check('a stale answer leaves the shortcut dialog alone',
    screencastEl.hidden === false);
  emit({ type: 'shortcuts.pick.done', id: 9 });
  check('and it goes away when it is answered', screencastEl.hidden === true);

  /* An application the frontend could not name is still an application asking
     for a key, and a sentence with a hole in it would read as though nothing
     were asking. */
  emit({ type: 'shortcuts.pick', id: 10, app: '', shortcuts: [
    { id: 'talk', description: 'push to talk', trigger: 'Mod4+grave' },
  ] });
  const help = screencastEl.children[0].children
    .find((c) => c._classes.has('screencast-help'));
  check('an unnamed application is named as one',
    help.textContent.startsWith('An unidentified application'));
  emit({ type: 'shortcuts.pick.done', id: 10 });
}

/* --- the keyboard, on every surface that can be opened -----------------
 *
 * The launcher and the passphrase box grew their own `keydown` because each
 * has a text field. Everything else here — the tray menu, the clipboard
 * history, the notification centre, the power menu, the two radio pickers —
 * was opened by a chord and then had to be finished with the pointer, which
 * is the same gap as a power menu a touch screen could not open.
 *
 * What is checked is the whole of what "has a keyboard" means for one of
 * these: that opening it asks the compositor for the keys at all (nothing
 * else can make an arrow key arrive), that the arrows move a highlight the
 * page can see, that Enter travels the row's own click handler rather than a
 * second path of its own, that Escape takes the surface down, and that going
 * down hands the keyboard back to the window that had it. See keys.js.
 * --------------------------------------------------------------------- */
{
  const key = (el, k) => (el.listeners.keydown ?? [])
    .forEach((fn) => fn({ key: k, preventDefault() {}, stopPropagation() {} }));
  const fire = (type) => (documentListeners[type] ?? [])
    .forEach((fn) => fn({ preventDefault() {}, stopPropagation() {} }));
  const here = (list) => list.findIndex((el) => el._classes.has('kbd-here'));
  /* Every stop the arrows make, wherever the surface happened to hang it. The
     power menu puts its verbs beside the list rather than in it and the radio
     pickers put the switch in a header, so a walk of the dialog is the only
     reading of "what can the keyboard reach" that is not a restatement of the
     markup it is checking. */
  const stops = (root) => {
    const out = [];
    const walk = (el) => {
      for (const c of el.children) {
        if (c.hidden) continue;
        if (c._classes.has('kbd-row') && !c._classes.has('disabled')) out.push(c);
        walk(c);
      }
    };
    if (root) walk(root);
    return out;
  };

  /* Something for the surfaces to hand the keyboard back to. */
  emit({ type: 'view.added', id: 90, title: 'Work', app_id: 'work',
    output: 'DP-1', min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });
  emit({ type: 'view.focused', id: 90 });

  /* --- the clipboard history --- */
  {
    const dialog = () => globalThis.__shell.clipboardEl.children[0];
    const rows = () => stops(dialog());

    emit({ type: 'clipboard.history', entries: [
      { id: 3, text: 'newest' }, { id: 2, text: 'middle' },
      { id: 1, text: 'oldest' },
    ] });
    let before = sent.length;
    emit({ type: 'shell.command', command: 'clipboard', args: [] });
    /* The one thing that has to happen first: the shell is a Wayland client
       and the window under the picker has the keyboard until this is sent. */
    check('opening the clipboard picker asks for the keyboard',
      sent.slice(before).some((m) => m.type === 'shell.focus'));
    /* Three entries and "forget everything", which is a stop too — a footer a
       keyboard could not reach is a button that only exists for a pointer. */
    check('every row and the footer are stops', rows().length === 4);
    check('and it opens on the first row', here(rows()) === 0);

    key(dialog(), 'ArrowDown');
    key(dialog(), 'ArrowDown');
    check('the arrows move the highlight', here(rows()) === 2);
    key(dialog(), 'ArrowUp');
    check('and back', here(rows()) === 1);

    before = sent.length;
    key(dialog(), 'Delete');
    /* The row under the keyboard, not the whole history: a Delete that
       cleared the clipboard would be a keystroke away from losing what
       somebody opened the picker to find. */
    check('Delete forgets the row the keyboard is on and nothing else',
      sent.slice(before).some((m) => m.type === 'clipboard.forget' &&
        m.id === 2));

    before = sent.length;
    key(dialog(), 'Enter');
    check('Enter pastes it, by the same click a pointer would have made',
      sent.slice(before).some((m) => m.type === 'clipboard.paste' && m.id === 2));
    check('and the picker goes',
      globalThis.__shell.clipboardEl.hidden === true);
    check('handing the keyboard back to the window that had it',
      sent.slice(before).some((m) => m.type === 'view.focus' && m.id === 90));

    emit({ type: 'shell.command', command: 'clipboard', args: [] });
    check('and it opens on the first row again rather than where it was left',
      here(rows()) === 0);
    before = sent.length;
    key(dialog(), 'Escape');
    check('Escape takes it down without pasting anything',
      globalThis.__shell.clipboardEl.hidden === true &&
      !sent.slice(before).some((m) => m.type === 'clipboard.paste'));
    check('and hands the keyboard back the same way',
      sent.slice(before).some((m) => m.type === 'view.focus' && m.id === 90));
  }

  /* --- the power menu --- */
  {
    const dialog = () => globalThis.__shell.powerEl.children[0];
    const rows = () => stops(dialog());

    emit({ type: 'power.update', profiles: ['power-saver', 'balanced',
      'performance'], profile: 'balanced', percentage: 80 });
    let before = sent.length;
    emit({ type: 'shell.command', command: 'power', args: [] });
    check('opening the power menu asks for the keyboard',
      sent.slice(before).some((m) => m.type === 'shell.focus'));
    /* Three profiles and five verbs, in the order they are drawn: a keyboard
       that could reach the profiles and not Suspend would be a power menu a
       keyboard could not use for the thing it is mostly used for. Lock is the
       fifth, and the one this matters most for — the desk that cannot use a
       pointer is the desk that cannot use an external locker either. */
    check('every profile and every verb is a stop', rows().length === 8);
    check('and it opens on the first', here(rows()) === 0);

    key(dialog(), 'ArrowDown');
    before = sent.length;
    key(dialog(), 'Enter');
    check('Enter on a profile row asks for that profile',
      sent.slice(before).some((m) => m.type === 'power.profile' &&
        m.profile === 'balanced'));
    check('and the menu goes', globalThis.__shell.powerEl.hidden === true);

    emit({ type: 'shell.command', command: 'power', args: [] });
    key(dialog(), 'End');
    before = sent.length;
    key(dialog(), 'Enter');
    /* The last row is Quit, which goes out as its own message rather than as
       a fourth power verb — see power.js. Reaching it from the keyboard is
       the point: it is the row furthest from where the highlight starts. */
    check('End reaches the last row and Enter takes it',
      sent.slice(before).some((m) => m.type === 'quit'));

    emit({ type: 'shell.command', command: 'power', args: [] });
    before = sent.length;
    key(dialog(), 'Escape');
    check('Escape takes the power menu down without doing anything',
      globalThis.__shell.powerEl.hidden === true &&
      !sent.slice(before).some((m) => m.type === 'power.action'));
  }

  /* --- the notification centre --- */
  {
    const dialog = () => globalThis.__shell.notificationCentreEl.children[0];
    const rows = () => (dialog()?.children[0]?.children ?? [])
      .filter((el) => el._classes.has('notification-centre-row'));
    const now = Math.floor(Date.now() / 1000);

    emit({ type: 'notification.history', entries: [
      { id: 30, app_name: 'chat', summary: 'newest', body: 'a message',
        urgency: 1, timeout: -1, actions: [], at: now - 30 },
      { id: 20, app_name: 'mail', summary: 'middle', body: '', urgency: 1,
        timeout: -1, actions: [{ key: 'default', label: 'Open' }], at: now - 60 },
    ] });
    let before = sent.length;
    emit({ type: 'shell.command', command: 'notifications', args: [] });
    check('opening the notification centre asks for the keyboard',
      sent.slice(before).some((m) => m.type === 'shell.focus'));
    check('and it opens on the newest', here(rows()) === 0);
    /* Read out as one sentence starting with the part that says whether the
       rest is worth hearing. Four fragments in visual order would make a
       reader say "chat, just now, newest, a message". */
    check('a row is named for a reader, summary first',
      rows()[0].getAttribute('aria-label').startsWith('newest, a message'));

    before = sent.length;
    key(dialog(), 'Delete');
    check('Delete forgets the row the keyboard is on',
      sent.slice(before).some((m) => m.type === 'notification.forget' &&
        m.id === 30));
    /* And not the whole list, which is what an id-less forget means. */
    check('and not the whole record',
      !sent.slice(before).some((m) => m.type === 'notification.forget' &&
        m.id === undefined));

    key(dialog(), 'ArrowDown');
    before = sent.length;
    key(dialog(), 'Enter');
    check('Enter invokes the row default action where there is one',
      sent.slice(before).some((m) => m.type === 'notification.action' &&
        m.id === 20 && m.action === 'default'));

    before = sent.length;
    key(dialog(), 'Escape');
    check('Escape closes the centre and gives the keyboard back',
      globalThis.__shell.notificationCentreEl.hidden === true &&
      sent.slice(before).some((m) => m.type === 'view.focus' && m.id === 90));
  }

  /* --- a tray item's menu --- */
  {
    const menu = () => globalThis.__shell.trayMenuEl;
    const rows = () => stops(menu());

    let before = sent.length;
    emit({ type: 'tray.menu', id: ':1.9/StatusNotifierItem', x: 100, y: 30,
      items: [
        { id: 1, label: 'Open', kind: 'standard', enabled: true },
        { id: 2, label: '', kind: 'separator', enabled: true },
        { id: 3, label: 'Sync now', kind: 'standard', enabled: false },
        { id: 4, label: 'Recent', kind: 'standard', enabled: true, children: [
          { id: 5, label: 'notes.md', kind: 'standard', enabled: true },
        ] },
      ] });
    check('opening a tray menu asks for the keyboard',
      sent.slice(before).some((m) => m.type === 'shell.focus'));
    /* Open and Recent. The separator is not a row at all, the disabled one is
       a dead end the keyboard would have to press past, and the submenu's own
       row is hidden until Recent is opened. */
    check('the arrows stop only at rows that can be chosen',
      rows().length === 2);

    key(menu(), 'ArrowDown');
    before = sent.length;
    key(menu(), 'Enter');
    check('Enter on a row with children opens it rather than choosing it',
      !sent.slice(before).some((m) => m.type === 'tray.menu.click'));
    check('and its rows join the ones the arrows stop at',
      rows().length === 3);
    /* The highlight stays on the row that was opened rather than following
       the rows that appeared under it: opening a submenu is not choosing
       something in it. */
    check('with the keyboard still on the row that was opened',
      here(rows()) === 1);

    key(menu(), 'ArrowDown');
    before = sent.length;
    key(menu(), 'Enter');
    check('and Enter in the submenu names the row to the application',
      sent.slice(before).some((m) => m.type === 'tray.menu.click' &&
        m.item === 5 && m.id === ':1.9/StatusNotifierItem'));
    check('and the menu goes', menu().hidden === true);

    emit({ type: 'tray.menu', id: ':1.9/StatusNotifierItem', x: 100, y: 30,
      items: [{ id: 1, label: 'Open', kind: 'standard', enabled: true }] });
    before = sent.length;
    key(menu(), 'Escape');
    check('Escape takes the menu down and tells the application it went',
      menu().hidden === true &&
      sent.slice(before).some((m) => m.type === 'tray.menu.closed'));
  }

  /* --- the wireless picker --- */
  {
    const dialog = () => globalThis.__shell.networkEl.children[0];
    const rows = () => stops(dialog());

    emit({ type: 'network.update', available: true, wireless: true,
      enabled: true, state: 'connected', ssid: 'kitchen', access_points: [
        { ssid: 'kitchen', strength: 88, security: 'wpa2', known: true, active: true },
        { ssid: 'office', strength: 70, security: 'wpa2', known: true, active: false },
      ] });
    let before = sent.length;
    emit({ type: 'shell.command', command: 'network', args: [] });
    /* The list used to be readable and not steerable: only the passphrase box
       asked for the keyboard, and the box is under a row somebody had to
       click to get. */
    check('opening the wireless picker asks for the keyboard',
      sent.slice(before).some((m) => m.type === 'shell.focus'));
    /* The radio switch and two networks. The switch is a stop because it is
       the only control on a picker whose radio is off — a keyboard that could
       not reach it could not turn the radio back on. */
    check('the switch is a stop as well as the networks', rows().length === 3);
    check('and it opens on the switch', here(rows()) === 0);

    key(dialog(), 'ArrowDown');
    key(dialog(), 'ArrowDown');
    before = sent.length;
    key(dialog(), 'Enter');
    check('Enter on a saved network joins it',
      sent.slice(before).some((m) => m.type === 'network.connect' &&
        m.ssid === 'office'));

    /* Named for a reader rather than left to the row's own text: the strength
       is four bars in a private-use codepoint and the lock is another one. */
    check('a network row is named in words',
      rows()[1].getAttribute('aria-label') ===
        'kitchen, connected, secured, signal 3 of 4');

    before = sent.length;
    key(dialog(), 'Escape');
    check('Escape closes the picker and gives the keyboard back',
      globalThis.__shell.networkEl.hidden === true &&
      sent.slice(before).some((m) => m.type === 'view.focus' && m.id === 90));
  }

  /* A surface that never took the keyboard must not hand one back: the
     screen-share chooser is steered from the compositor and receives no input
     of its own, and a view.focus from it would move focus on a desktop nobody
     touched. */
  {
    const before = sent.length;
    emit({ type: 'screencast.pick', id: 40, app: 'obs', outputs: [
      { name: 'DP-1', description: 'a monitor' },
    ], windows: [] });
    emit({ type: 'screencast.pick.done', id: 40 });
    check('the screen-share chooser neither takes the keyboard nor returns it',
      !sent.slice(before).some((m) => m.type === 'shell.focus' ||
        m.type === 'view.focus'));
  }

  fire('click');
  emit({ type: 'view.removed', id: 90 });
  check('teardown clean', process.exitCode !== 1);
}

/* --- the stylesheet ----------------------------------------------------
 *
 * A window is a border and a hole, and both of them are CSS. Nothing above
 * this point can tell whether that CSS reaches the elements the shell builds:
 * geometry.js toggles `focused` on and the assertion that it did so is an
 * assertion about geometry.js. Rename the class in shell.css alone and the
 * frame stops changing colour with focus, with every test still passing.
 *
 * So shell.css is run through the cascade in css.js against the elements the
 * shell has just built — the real class list, the real ancestors, the real
 * inline styles — and the question asked is the one a browser answers: which
 * declaration wins here? That covers selector matching, specificity, source
 * order, `!important` and var(). It covers no pixels whatever: nothing here
 * knows what `flex: 0 0 8px` looks like, only that it is what is in force.
 * --------------------------------------------------------------------- */
{
  const sheet = css.parse(fs.readFileSync(`${shellDir}/shell.css`, 'utf8'),
    { root: documentElement });

  /* The resolver is itself something that can be wrong, and a broken one
     answers every question with the empty string — which would quietly satisfy
     any assertion phrased as "is not the focus colour". So it is given a
     stylesheet whose answers are known before it is trusted with the real
     one. */
  {
    const fixture = css.parse(
      '.a { color: red; border: 1px solid red }'
      + '.a.b { color: green }'
      + '.c { color: blue }'
      + '.d { color: black !important }');
    const el = new El('div');

    el.className = 'a c';
    check('a later rule of equal specificity wins',
      fixture.value(el, 'color') === 'blue');

    el.className = 'a b c';
    check('and a more specific one wins wherever it sits in the file',
      fixture.value(el, 'color') === 'green');

    el.className = 'e';
    check('a rule that matches nothing contributes nothing',
      fixture.value(el, 'color') === '');

    el.className = 'a b c';
    el.style.color = 'purple';
    check('an inline style beats an ordinary rule',
      fixture.value(el, 'color') === 'purple');

    el.className = 'a b c d';
    check('and !important beats the inline style',
      fixture.value(el, 'color') === 'black');

    el.className = 'a';
    check('a shorthand is seen as the longhands it sets',
      fixture.value(el, 'border-top-style') === 'solid');
  }

  /* Every `display: grid` has a fallback, which is a portability assertion
     rather than a matter of taste.
   *
   * The shell is drawn by whichever engine the backend names, and one of them
   * has no grid: Servo drops the declaration outright — "Unsupported property
   * declaration: 'display: grid'" — and the box falls back to `block`. That
   * turned the overview into a single column of thumbnails running off the
   * bottom of the screen and the empty state into a mark in the top-left
   * corner.
   *
   * The overview keeps its grid and gains an `@supports not (display: grid)`
   * fallback, because measuring said the flex version costs the engines that
   * *do* have grid about twice the compositor CPU. The empty state is flex
   * outright: it is laid out once and has no per-frame cost to protect.
   *
   * So the sweep is not "no grid anywhere" — it is that every rule declaring
   * one is answered by a fallback somewhere in the file. */
  {
    const text = fs.readFileSync(`${shellDir}/shell.css`, 'utf8')
      /* Comments stripped, or this finds the paragraphs explaining the
         fallback and reports them as declarations. */
      .replace(/\/\*[\s\S]*?\*\//g, '');

    const empty = new El('div');
    empty.className = 'empty';
    check('the empty state lays out with flexbox',
      sheet.value(empty, 'display') === 'flex');

    /* The keymap on a full config is fifty chords or more, which is taller
       than the box it is drawn in. That box has been bounded since it was
       written, and until now nothing could scroll it:

       `.empty` is inert so a click on an empty desktop reaches the desktop,
       and `pointer-events` is inherited -- so the wheel went through the list
       to the page behind it. And a `columns: 2` box with a bounded height does
       not scroll at all: it lays out a third column to the right, which with
       `overflow-y` computing the other axis to `auto` put the rest of the
       chords behind a horizontal scrollbar instead.

       A wrapping flex row overflows downwards, which is what a bounded box
       wants. Flex and not grid for the reason the sweep below exists: Servo
       has no grid, and this is drawn by Servo. */
    const keys = new El('pre');
    keys.className = 'keys';
    empty.append(keys);
    check('the keymap wraps into rows rather than into columns',
      sheet.value(keys, 'display') === 'flex'
      && sheet.value(keys, 'flex-wrap') === 'wrap'
      && sheet.value(keys, 'columns') === '');
    check('and a keymap taller than its box scrolls',
      sheet.value(keys, 'overflow-y') === 'auto'
      && sheet.value(keys, 'max-height') !== '');
    check('and takes the wheel, which the empty state around it does not',
      sheet.value(keys, 'pointer-events') === 'auto'
      && sheet.value(empty, 'pointer-events') === 'none');

    /* The focus ring.
     *
     * `kbd-here` is where the keyboard is on whichever surface has it, and it
     * has to be visible on top of every background the pickers already paint
     * for hover and for state. Being last in the file is what settles that
     * without an `!important`, and being an outline is what stops a
     * background painted by a row's own rule from covering it — so both are
     * asserted here rather than trusted to stay that way. A rename in
     * shell.css alone would leave every keyboard assertion above still
     * passing against a highlight nobody could see.
     *
     * The clipboard row is the harder of the two: `.clipboard-row` sets no
     * background of its own, and `.launcher-row` did until the rule that did
     * it was folded into this one. */
    for (const [what, cls] of [['a clipboard row', 'clipboard-row'],
      ['a launcher row', 'launcher-row'], ['a power row', 'power-row'],
      ['a tray menu row', 'tray-menu-row'], ['a network row', 'radio-row']]) {
      const row = new El('button');
      row.className = cls;
      const plain = sheet.value(row, 'outline-style');
      row.className = `${cls} kbd-here`;
      check(`${what} the keyboard is on draws a ring, and does not otherwise`,
        plain !== 'solid' && sheet.value(row, 'outline-style') === 'solid'
        && sheet.value(row, 'outline-color') === '#7aa2f7');
      /* Inside the row's own box: the lists clip, and a ring drawn outside
         the first row would be cut in half by the list it is in. */
      check(`and the ring on ${what} is drawn inside it`,
        sheet.value(row, 'outline-offset') === '-2px');
    }

    /* Nothing about the ring is animated. Everything else in this file that
       changes on a state change fades; a ring that arrives over 120ms is not
       there yet when a held arrow key has already moved on, and somebody
       steering by keyboard is reading their position from it. */
    {
      const row = new El('button');
      row.className = 'launcher-row kbd-here';
      check('and it appears at once rather than fading in',
        !/outline/.test(sheet.value(row, 'transition-property')));
    }

    const grids = text.split('\n')
      /* The `@supports` condition names the property it is testing for and is
         not a declaration of it. */
      .filter((line) => !/@supports/.test(line))
      .filter((line) => /display\s*:\s*(inline-)?grid/.test(line));
    check('the overview is the only rule left asking for a grid',
      grids.length === 1);
    check('and an engine without one is given flexbox instead',
      /@supports\s+not\s*\(\s*display\s*:\s*grid\s*\)\s*\{[^}]*\.overview\s*\{[^}]*display\s*:\s*flex/
        .test(text));

    /* The script's half of the same fallback: the flex row needs to be told
       how wide one row is, or it wraps wherever the output runs out. */
    const js = fs.readFileSync(`${shellDir}/scrolling.js`, 'utf8');
    check('and the script asks the engine which of the two it is drawing',
      js.includes("CSS.supports('display', 'grid')")
      && js.includes('maxWidth'));
  }

  /* The wallpaper: a picture the compositor resolved, painted by the page.
   *
   * Both halves are checked here because neither is worth much alone — the
   * config handler setting a class nothing styles, or a rule keyed on a class
   * nothing sets, both look exactly like a wallpaper that does not appear.
   * `body` is built for this rather than taken from the harness, which has no
   * document to speak of: what the cascade needs is the tag and the parent. */
  {
    const body = new El('body');
    body.parentElement = documentElement;
    const wallpaperOf = () => sheet.value(body, 'background-image');
    /* The shell's own background is a layered shorthand, which the resolver
       keeps whole under `background` — so "the gradient is what is showing" is
       asked as "there is no background-image over it". */
    check('the shell paints a gradient of its own',
      sheet.value(body, 'background').includes('gradient'));

    emit({ type: 'config', layout: mode,
      wallpaper: 'file:///pic/wall%20paper.png', wallpaper_mode: 'fit' });
    check('a wallpaper is drawn as the desktop background',
      wallpaperOf().includes('url("file:///pic/wall%20paper.png")'));
    check('and the fitting it was given is the one in force',
      sheet.value(body, 'background-size') === 'contain');

    /* The mode changes without the picture being named again, which is what
       `config.wallpaper --mode` alone sends. */
    emit({ type: 'config', layout: mode,
      wallpaper: 'file:///pic/wall%20paper.png', wallpaper_mode: 'tile' });
    check('a tiled wallpaper repeats at its own size',
      sheet.value(body, 'background-repeat') === 'repeat'
      && sheet.value(body, 'background-size') === 'auto');

    /* A colour is a wallpaper too, and lands on the colour rather than the
       image: `background-image: #1a1b26` is nothing at all, and a desktop that
       came up black would look exactly like the setting being ignored. */
    emit({ type: 'config', layout: mode, wallpaper: '#1a1b26' });
    check('a colour paints the desktop rather than an image',
      sheet.value(body, 'background-color') === '#1a1b26'
      && wallpaperOf() === 'none');
    check('and a colour has no fitting to leave behind',
      !documentElement.classList.contains('wallpaper-tile'));

    /* A gradient is an image, because that is the property it belongs to. */
    emit({ type: 'config', layout: mode,
      wallpaper: 'linear-gradient(#1a1b26, #24283b)' });
    check('a gradient is drawn as the image, not the colour',
      wallpaperOf() === 'linear-gradient(#1a1b26, #24283b)');

    /* Absent is the gradient back, and not a black screen: this is what the
       empty `wallpaper` in a config file and `--path ''` over the socket both
       arrive as, so it is the only way back. */
    emit({ type: 'config', layout: mode });
    check('no wallpaper leaves the shell its own background',
      wallpaperOf() === '' && sheet.value(body, 'background').includes('gradient'));

    /* A terminal behind the page wins over a picture in it. Both cannot be
       seen — the page is what would cover the terminal — and the terminal is
       the one that was asked for by running a program. */
    emit({ type: 'config', layout: mode, background_terminal: true,
      wallpaper: 'file:///pic/wall.png' });
    check('a background terminal is not painted over by a wallpaper',
      !wallpaperOf().includes('url('));

    /* And back to where this section found the desktop: a config event is the
       whole configuration, so leaving one of these in force would hand the
       rest of the file a different shell. */
    emit({ type: 'config', layout: mode });
  }

  /* A shorthand expands to one longhand per side, so this is how a test asks
     what the frame looks like all the way round rather than on one edge. */
  const sides = (el, part) => {
    const values = ['top', 'right', 'bottom', 'left']
      .map((side) => sheet.value(el, `border-${side}-${part}`));
    return values.every((v) => v === values[0]) ? values[0] : 'mixed';
  };
  const snapshot = (el) => [...sheet.declarations(el)]
    .map(([prop, value]) => `${prop}:${value}`).sort().join(';');

  /* Classes seen on a real window element during this section, so the sweep at
     the end is about what the shell does rather than about a list kept here. */
  const seen = new Set();
  const record = () => {
    for (const [, view] of globalThis.__shell.views) {
      for (const c of String(view.el.className).split(/\s+/)) {
        if (c !== '' && c !== 'window') seen.add(c);
      }
    }
  };

  const open = (id, app, floating = false) => emit({ type: 'view.added', id,
    title: app, app_id: app, output: 'DP-1', min_width: 0, min_height: 0,
    floating, width: 800, height: 600 });

  const views = globalThis.__shell.views;
  const outs = globalThis.__shell.outputs;

  /* Everything above has left windows, tabbed containers and fullscreen state
     scattered over the workspaces, and a container someone tabbed earlier
     draws titles instead of the divider this section is about. So it starts on
     one nobody has touched, and parks a window on a second that no monitor is
     showing — which is what makes a hidden window hidden. */
  const workspaceOf = globalThis.__shell.workspaceOfForTest;
  const busy = new Set([...views.keys()].map((id) => workspaceOf(id)));
  const shown = new Set([...outs.values()].map((o) => o.workspace));
  const free = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    .filter((n) => !busy.has(n) && !shown.has(n));
  check('the test found two workspaces of its own', free.length >= 2);
  const [home, park] = free;

  emit({ type: 'shell.command', command: 'workspace.switch',
    args: [String(home)] });
  const output = outs.get(globalThis.__shell.activeOutput);

  open(80, 'framed');
  open(81, 'framed');
  emit({ type: 'view.focused', id: 80 });
  record();

  const framed = views.get(80);
  const other = views.get(81);

  check('a window is drawn as a frame around its hole',
    sheet.value(framed.el, 'display') === 'flex' &&
    sheet.value(framed.el, 'overflow') === 'hidden');
  check('two pixels of border on every side',
    sides(framed.el, 'width') === '2px');

  /* A hole is whole pixels and layout is not.
   *
   * Rounding a fractional rect to the nearest pixel moves the hole outwards
   * half the time, and the compositor then draws the client over the pixel the
   * page painted the border into: invisible at 2px, where the other pixel
   * survives, and the entire border at 1px. The hole has to be the pixels
   * *inside* the rect, so the frame is never overdrawn. */
  {
    const at = sent.length;
    const measured = framed.viewport.getBoundingClientRect;
    framed.viewport.getBoundingClientRect = () => ({
      left: 100.4, top: 50.6, width: 800.9, height: 600.2,
      x: 100.4, y: 50.6,
    });
    globalThis.__shell.reportGeometryForTest(80);
    framed.viewport.getBoundingClientRect = measured;
    const laid = sent.slice(at).find((m) => m.type === 'view.layout' && m.id === 80);
    /* 100.4 .. 901.3 encloses whole pixels 101 .. 901, so x=101 and width=800.
       Nearest-rounding would have said x=100, width=801 — a hole reaching a
       pixel past the frame at both ends. */
    check('a fractional rect becomes the whole pixels inside it',
      laid !== undefined && laid.x === 101 && laid.width === 800);
    /* 50.6 .. 650.8 likewise encloses 51 .. 650, which is 599 rows and not the
       600 the measured height rounds to. */
    check('and the same on the other axis',
      laid !== undefined && laid.y === 51 && laid.height === 599);
    check('with a clip that does not reach past the hole',
      laid === undefined || laid.clip === undefined ||
      (laid.clip.x >= laid.x &&
       laid.clip.x + laid.clip.width <= laid.x + laid.width));
  }

  /* Against the custom property rather than against #7aa2f7: a theme from the
     config file arrives as an override of exactly these, so a literal here
     would be asserting the default theme rather than the rule. */
  const radius = sheet.custom(documentElement, '--radius');
  const focusColour = sheet.custom(documentElement, '--border-focus');
  const restColour = sheet.custom(documentElement, '--border');
  check('rounded by the theme rather than by a literal',
    radius !== '' && sheet.value(framed.el, 'border-radius') === radius);
  check('and focused and unfocused are different colours',
    focusColour !== '' && restColour !== '' && focusColour !== restColour);

  /* Two windows differing in one class, so this is the class doing it.
     Solar and the matrix both draw the window beside the focused one in a
     colour of their own — a tier, or a slot in the focus history, is not
     merely "not focused" — so the second half of the pair is asked for there
     rather than the resting grey. */
  const besideColour = (mode === 'solar' || mode === 'matrix')
    ? sheet.custom(documentElement, '--border-hover') : restColour;
  check('the focused window wears the focus colour',
    sides(framed.el, 'color') === focusColour);
  check('and the one beside it does not',
    besideColour !== focusColour && sides(other.el, 'color') === besideColour);

  emit({ type: 'view.focused', id: 81 });
  check('moving focus moves the colour with it',
    sides(other.el, 'color') === focusColour &&
    sides(framed.el, 'color') === besideColour);
  emit({ type: 'view.focused', id: 80 });

  /* The rule the whole file is written around: the compositor paints the
     client into whatever rect this element occupies, on a layer above the web
     view, so anything painted here is composited and then covered. */
  check('the hole is painted with nothing at all',
    sheet.value(framed.viewport, 'background-color') === 'transparent' &&
    sheet.value(framed.viewport, 'background') === '');

  /* The gap between two windows is a real element — that is what makes edge
     dragging need no compositor support — so it has a width of its own and
     must never take any of a window's. */
  const gap = sheet.custom(documentElement, '--gap');
  /* Solar has no dividers, because it has no shared edges: windows overlap on
     purpose there and the space between two of them is not a thing anyone
     could drag. The matrix has none either, for the nearer reason that its
     rectangles are arithmetic: the space between two slots is a number in
     matrix.js, not an element. The area's own inset below still applies to
     both. The canvas has none for both reasons at once: its windows overlap
     and its rectangles are arithmetic. */
  const divider = output.windowsEl.querySelector('.divider');
  if (mode !== 'solar' && mode !== 'matrix' && mode !== 'canvas') {
    check('the shell drew a real element in the gap', divider !== null);
    check('as wide as the gap, and it never grows',
      sheet.value(divider, 'flex-basis') === gap &&
      sheet.value(divider, 'flex-grow') === '0' &&
      sheet.value(divider, 'flex-shrink') === '0');
    check('and its container leaves no CSS gap for it to sit on top of',
      sheet.value(divider.parentElement, 'gap') === '');
  }
  /* The edge padding is inner + outer. With the defaults (outer 0) the two
     resolve to a calc whose value is the inner gap, so the inset is still the
     gap — but an outer gap should widen it, and it is the calc that says so. */
  const edgePadding = sheet.value(output.windowsEl, 'padding-top');
  const gapOuter = sheet.custom(documentElement, '--gap-outer');
  check('the tiling area is inset by that same gap',
    ['top', 'right', 'bottom', 'left'].every((side) =>
      sheet.value(output.windowsEl, `padding-${side}`) === edgePadding));
  check('and the edge padding is the inner gap plus the outer gap',
    edgePadding === `calc(${gap} + ${gapOuter})`);
  /* scrolling.js reads --gap out of the stylesheet at runtime and falls back to
     8 where there are no computed styles, which is every run of this harness.
     The two have to agree or the column arithmetic checked above is not the
     arithmetic that ships. */
  check('which is the 8px this harness and scrolling.js fall back to',
    gap === '8px');

  /* Floating: positioned rather than laid out, and sized as the client asked
     rather than as the frame around it. */
  open(82, 'floaty', true);
  const floaty = views.get(82);
  record();

  check('a floating window carries its rect as an inline style',
    floaty.el.style.left !== '' && floaty.el.style.width !== '');
  /* content-box has to beat the `*` rule that makes everything else
     border-box, or the rect describes the frame and the hole comes out two
     pixels smaller on every side than the client asked for. */
  check('and its rect describes the hole rather than the frame',
    sheet.value(floaty.el, 'box-sizing') === 'content-box');
  const floatingLayer = Number(sheet.value(floaty.el, 'z-index'));
  check('it is lifted above the tiled windows',
    sheet.value(floaty.el, 'position') === 'absolute' && floatingLayer > 0);
  check('and its hole is still a hole',
    sheet.value(floaty.viewport, 'background-color') === 'transparent');

  /* Fullscreen over a floating window is the case the !important in shell.css
     exists for: the inline rect is left where it was, and the rule has to win
     over it without the JS stripping and restoring four properties. */
  emit({ type: 'shell.command', command: 'window.fullscreen.set',
    args: ['82', '1'] });
  record();

  check('the desktop knows something on it is fullscreen',
    output.el.classList.contains('has-fullscreen'));
  check('fullscreen takes the frame off entirely',
    sides(floaty.el, 'width') === '0' &&
    sheet.value(floaty.el, 'border-radius') === '0');
  check('and covers the output over the rect the window still carries',
    floaty.el.style.left !== '' &&
    sheet.value(floaty.el, 'left') === '0' &&
    sheet.value(floaty.el, 'top') === '0' &&
    sheet.value(floaty.el, 'width') === 'auto');
  /* Above the floating layer, and this is the whole of what decides it:
     `.window.fullscreen` asks for 10 and `.window.floating` for 5, the two
     selectors are equally specific, and nothing but source order separates
     them. Written the other way round — as they were, fullscreen first — the 5
     won and the 10 had never once applied to anything.

     Which showed nowhere, because a fullscreen window painted nothing at all:
     its border is gone, its hole is transparent, and client surfaces are
     stacked by the compositor rather than by anything the shell says. It
     paints something now — the rule switches off the shadow a floating window
     brings with it — and a window declaring a layer it does not get is worth
     failing on whether or not this year's stylesheet can show it. */
  check('and above a floating window rather than merely level with it',
    Number(sheet.value(floaty.el, 'z-index')) > floatingLayer);
  check('the gap goes with it: fullscreen means the whole output',
    sheet.value(output.windowsEl, 'padding-top') === '0' &&
    sheet.value(output.windowsEl, 'top') === '0');

  if (mode === 'scrolling') {
    /* A transform makes the strip the containing block for anything positioned
       inside it, which would anchor the fullscreen window to the scrolled strip
       rather than to the output. The rule that stops that has to beat the
       strip's own inline transform, which is what !important is for here. */
    const strip = output.windowsEl.querySelector('.strip');
    check('the strip is translated by an inline style',
      strip !== null && strip.style.transform !== '');
    check('and fullscreen stops the scrolling outright',
      strip !== null && sheet.value(strip, 'transform') === 'none');
  }

  emit({ type: 'shell.command', command: 'window.fullscreen.set',
    args: ['82', '0'] });
  check('and the frame comes back when it is over',
    sides(floaty.el, 'width') === '2px');
  emit({ type: 'view.removed', id: 82 });

  if (mode === 'tiling') {
    /* Tabs are the only place the shell draws a title at all — without one
       there is nothing to tell two collapsed windows apart — so this is the
       whole of its titlebar styling. */
    emit({ type: 'shell.command', command: 'layout.tabbed', args: [] });
    const strip = output.windowsEl.querySelector('.tabs');
    const tabs = (strip?.children ?? [])
      .filter((el) => el.classList.contains('tab'));
    check('a tabbed container draws a title strip', tabs.length >= 2);

    const active = tabs.filter((el) => el.classList.contains('active'));
    check('with exactly one title marked as the one on show',
      active.length === 1);
    check('the tab on show is lit',
      sides(active[0], 'color') === focusColour);
    check('and the ones behind it are not',
      tabs.filter((el) => !el.classList.contains('active'))
        .every((el) => sides(el, 'color') === restColour));
    check('each title is one --tab tall',
      sheet.value(tabs[0], 'height') ===
        sheet.custom(documentElement, '--tab'));

    emit({ type: 'shell.command', command: 'layout.toggle', args: [] });
  }

  /* Hidden on another workspace: the element stays in the DOM so the client
     stays alive, so something has to stop it being drawn. */
  emit({ type: 'view.focused', id: 81 });
  emit({ type: 'shell.command', command: 'workspace.move',
    args: [String(park)] });
  check('the test parked a window off screen', other.el.hidden);
  check('and a parked window is not drawn',
    sheet.value(other.el, 'display') === 'none');
  emit({ type: 'view.focused', id: 80 });

  /* In the overview a window is a miniature of itself, and the rule that says
     so reaches it through the thumbnail it was rendered into — a descendant
     selector, so this is a check that the tree the shell built has the shape
     the stylesheet expects. */
  emit({ type: 'shell.command', command: 'layout.overview', args: [] });
  record();
  check('a window in a thumbnail is drawn with a thinner frame',
    sides(framed.el, 'width') === '1px');
  check('and without the shadow it would otherwise cast',
    sheet.value(framed.el, 'box-shadow') === 'none');
  /* Kept for the sweep below, which needs somewhere a window can be dragged
     between workspaces. The overview closing detaches this from the desktop
     but not from the thumbnail above it, which is the part that matters. */
  const insideThumb = framed.el.parentElement;

  emit({ type: 'shell.command', command: 'layout.overview', args: [] });
  check('and it is the full frame again once the overview is gone',
    sides(framed.el, 'width') === '2px');

  /* Both directions of the same drift.
   *
   * A class renamed in shell.css alone leaves a rule that can never match; one
   * renamed in the shell alone leaves a state nothing draws. Neither shows up
   * as anything but a window that stops changing appearance, which no other
   * test in this file can see. */
  const states = new Set();
  for (const rule of sheet.rules) {
    const subject = rule.parts[rule.parts.length - 1].compound;
    if (!subject.classes.includes('window')) continue;
    for (const c of subject.classes) if (c !== 'window') states.add(c);
  }
  check('the stylesheet draws several window states', states.size >= 5);
  check('every one of them is a class the shell actually sets',
    [...states].every((c) => src.includes(`'${c}'`)));
  check('and every class the shell put on a window is one it draws',
    seen.size > 0 && [...seen].every((c) => states.has(c)));

  /* And that each of those rules still reaches a window, rather than existing
     and being overridden into having no effect at all.

     The probe goes inside a thumbnail because one of the states — being
     carried to another workspace — is only ever drawn there, and a bare
     .window would report that rule as dead when it is merely somewhere
     else. */
  const probe = document.createElement('section');
  insideThumb.append(probe);
  probe.className = 'window';
  const plain = snapshot(probe);
  check('each state changes how the window is drawn', [...states].every((c) => {
    probe.className = `window ${c}`;
    return snapshot(probe) !== plain;
  }));
  /* And that none of those layers is above the bar.
   *
   * The shell is one buffer under the clients, so what the compositor draws
   * over the windows is a crop of this same page — the bar under `auto` is
   * exactly that. A window layer that outranks the bar here is therefore a
   * window border drawn across the clock on screen, in every backend, and the
   * only place the question can be asked is the stylesheet: `.windows` makes
   * no stacking context of its own, so a window's z-index competes with the
   * bar's directly.
   *
   * It went wrong with two of these already at 4 and 5 against a bar at 3.
   * Written as a sweep over the states rather than as a list, so a layer added
   * later is held to the same rule without anyone remembering this. */
  const layer = new El('div');
  layer.className = 'bar';
  const barZ = Number(sheet.value(layer, 'z-index'));
  check('the bar is drawn on a layer of its own', Number.isFinite(barZ));

  const above = [...states].filter((c) => {
    probe.className = `window ${c}`;
    const z = Number(sheet.value(probe, 'z-index'));
    /* Fullscreen is the one exception, and not really one: it does cover the
       bar, by taking it off the desktop with `display: none` rather than by
       being drawn over it. */
    return c !== 'fullscreen' && Number.isFinite(z) && z >= barZ;
  });
  check('and every window layer is under it', above.length === 0);
  check('including the ones a lifted window carries',
    ['floating', 'front', 'sun'].every((c) => {
      probe.className = `window ${c}`;
      return Number(sheet.value(probe, 'z-index')) < barZ;
    }));

  probe.remove();

  /* The calendar hangs off the bar, so it has to be drawn over it — a dropdown
     under the thing it drops from is a dropdown nobody can read. The other
     half is the one the marked day depends on: `today` has to change how the
     cell is drawn, or the one state in the grid is invisible and the whole
     panel is a table of numbers. */
  {
    const dropdown = new El('div');
    dropdown.id = 'calendar';
    const layer = new El('div');
    layer.className = 'bar';
    check('the calendar is drawn over the bar it hangs from',
      Number(sheet.value(dropdown, 'z-index'))
        > Number(sheet.value(layer, 'z-index')));

    const day = new El('span');
    day.className = 'calendar-day';
    const plain = snapshot(day);
    day.className = 'calendar-day today';
    check('and today is drawn differently from every other day',
      snapshot(day) !== plain);
    day.className = 'calendar-day adjacent';
    check('as is a day belonging to the month next door',
      snapshot(day) !== plain);
  }

  emit({ type: 'view.removed', id: 80 });
  emit({ type: 'view.removed', id: 81 });
}

/* Resizing a floating window by a corner, pacing a gesture, and the one-layout
 * measure pass. Three things the shell grew after v0.1.0 and none of them
 * reachable from the layout tests above: what they change is which edge moves,
 * how often the desk is rebuilt, and how many times the browser is asked where
 * something is. */
{
  /* A pull on the top or left edge pins the opposite one.
   *
   * The window has to move as well as change size for that to hold, and the
   * move is worked out from the size the clamp actually allowed — so a drag
   * that has run into the minimum stops, rather than sliding the window across
   * the desktop from an edge that can no longer move.
   *
   * Not on the canvas, where a floating window is on the plane like every
   * other one and canvasResizeBy answers first. */
  if (mode !== 'canvas') {
    emit({ type: 'view.added', id: 95, title: 'dialog', app_id: 'corner-dialog',
      output: 'DP-1', min_width: 0, min_height: 0, floating: true,
      width: 400, height: 300 });
    const rect = () => globalThis.__shell.floatingForTest(95);
    const start = { ...rect() };
    check('a floating window has a rect to resize', start.width > 200);
    const right = start.x + start.width;
    const bottom = start.y + start.height;

    emit({ type: 'shell.command', command: 'layout.resize.delta',
      args: ['95', '-60', '-40', 'top-left'] });
    check('dragging the top left corner out grows the window',
      rect().width === start.width + 60 && rect().height === start.height + 40);
    check('and the corner the hand is not on stays where it was',
      rect().x + rect().width === right && rect().y + rect().height === bottom);
    check('so the window moved as well as grew',
      rect().x === start.x - 60 && rect().y === start.y - 40);

    /* The other corner of the same axis: west without north moves the left
       edge and leaves the top alone. */
    const before = { ...rect() };
    emit({ type: 'shell.command', command: 'layout.resize.delta',
      args: ['95', '-30', '30', 'bottom-left'] });
    check('a pull on the bottom left moves the left edge only',
      rect().x === before.x - 30 && rect().y === before.y);
    check('and grows both axes all the same',
      rect().width === before.width + 30 && rect().height === before.height + 30);

    /* Into the clamp. 80x60 is what resizeByDelta falls back to for a client
       that named no minimum of its own. */
    const grown = { ...rect() };
    emit({ type: 'shell.command', command: 'layout.resize.delta',
      args: ['95', '4000', '4000', 'top-left'] });
    check('a drag that shrinks it to nothing stops at the minimum',
      rect().width === 80 && rect().height === 60);
    check('and the far corner is still pinned there',
      rect().x + rect().width === right
      && rect().y + rect().height === grown.y + grown.height);

    const stuck = { ...rect() };
    emit({ type: 'shell.command', command: 'layout.resize.delta',
      args: ['95', '4000', '4000', 'top-left'] });
    check('and dragging further does not slide the window away from it',
      rect().x === stuck.x && rect().y === stuck.y
      && rect().width === 80 && rect().height === 60);

    emit({ type: 'view.removed', id: 95 });
    emit({ type: 'view.focused', id: 4 });
  }

  /* One relayout per frame while a hand is down, and the two ways that ends.
   *
   * Driven through gestureRelayout directly rather than through a drag: which
   * command reaches it differs by layout, and what is being checked here is
   * the pacing rather than any one gesture. */
  {
    const out = globalThis.__shell.outputs.get(globalThis.__shell.activeOutput);
    /* From rest, whatever an earlier delta test left running. Before the
       stubs below, so its relayout runs on the real ones. */
    endGesture();

    let relayouts = 0;
    const realReplace = out.windowsEl.replaceChildren.bind(out.windowsEl);
    out.windowsEl.replaceChildren = (...nodes) => {
      relayouts++;
      return realReplace(...nodes);
    };

    /* Frames and timers are inline in this harness, which is what makes
       coalescing invisible: held here so the gap between asking for a frame
       and getting one is a thing the test can stand in. */
    const frames = [];
    const timers = [];
    const realFrame = global.requestAnimationFrame;
    const realTimeout = global.setTimeout;
    global.requestAnimationFrame = (fn) => { frames.push(fn); };
    global.setTimeout = (fn, ms) => { timers.push({ fn, ms }); return 0; };

    gestureRelayout();
    gestureRelayout();
    gestureRelayout();
    check('a delta starts a gesture', isGesturing());
    check('three deltas ask for one frame, not three', frames.length === 1);
    check('and nothing is laid out until that frame comes', relayouts === 0);

    frames.splice(0).forEach((fn) => fn(fakeClock));
    check('the frame lays the desk out once', relayouts === 1);
    check('and draws it without its animations while the hand is down',
      out.windowsEl.classList.contains('gesture'));

    /* The fallback for a gesture that ends without a release: a VT switch
       takes the pointer away and no button is ever reported up. */
    /* The last of them: each delta re-arms the timer, and the stub above has
       no clearTimeout to take the ones it replaced back out again. */
    const idle = timers.filter((t) => t.ms === 120);
    check('a gesture arms a timer to end itself', idle.length > 0);
    relayouts = 0;
    idle[idle.length - 1].fn();
    check('and the timer firing ends it', !isGesturing());
    check('with one more relayout, to put the animations back',
      relayouts === 1 && !out.windowsEl.classList.contains('gesture'));

    /* And the ordinary ending: the compositor reports the button up. */
    gestureRelayout();
    frames.splice(0).forEach((fn) => fn(fakeClock));
    check('a second gesture suppresses them again',
      isGesturing() && out.windowsEl.classList.contains('gesture'));

    relayouts = 0;
    endGesture();
    check('the release ends the gesture at once',
      !isGesturing() && relayouts === 1
      && !out.windowsEl.classList.contains('gesture'));

    relayouts = 0;
    endGesture();
    check('and a release with no gesture under way does nothing',
      relayouts === 0);

    global.requestAnimationFrame = realFrame;
    global.setTimeout = realTimeout;
    delete out.windowsEl.replaceChildren;
  }

  /* One layout query per element per frame.
   *
   * The saving is invisible from outside — the same rectangles are reported
   * either way — so nothing else here would notice the cache going stale. A
   * DOM write between beginMeasurePass and endMeasurePass is what would do it,
   * and this is the assertion that says so. */
  {
    let queries = 0;
    const el = new El('div');
    el.__rect = { left: 0, top: 0, width: 100, height: 50 };
    const measure = el.getBoundingClientRect.bind(el);
    el.getBoundingClientRect = () => { queries++; return measure(); };

    measureOf(el);
    measureOf(el);
    check('outside a pass every measurement asks the browser again',
      queries === 2);

    beginMeasurePass();
    const first = measureOf(el);
    const second = measureOf(el);
    check('inside one, an element is measured once however often it is asked',
      queries === 3);
    check('and every caller is handed that one rectangle', first === second);

    el.__rect = { left: 400, top: 0, width: 100, height: 50 };
    check('so a write during a pass is not seen by it: nothing may write',
      measureOf(el).left === 0 && queries === 3);

    endMeasurePass();
    check('and the pass ending goes back to measuring live',
      measureOf(el).left === 400 && queries === 4);
  }
}

emit({ type: 'view.removed', id: 1 });
emit({ type: 'view.removed', id: 2 });
emit({ type: 'view.removed', id: 3 });
emit({ type: 'view.removed', id: 4 });
check('teardown clean', process.exitCode !== 1);
/* Exit rather than fall off the end. The shell installs a one-second interval
 * to redraw its bar, which keeps the event loop alive for ever — so without
 * this the tests pass and then hang, and a runner reports the timeout instead
 * of the result. The session path above already does the same. */
process.exit(process.exitCode ?? 0);
