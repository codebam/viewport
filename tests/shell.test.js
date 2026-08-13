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
 *   node tests/shell.test.js data/shell tiling session
 *
 * Exits non-zero on failure. CI runs all four combinations in the `shell`
 * job; run one by hand with the lines above when a case fails.
 */
const fs = require('fs');
const css = require('./css.js');

let idSeq = 0;

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
    ['bar-right', ['mode', 'clock', 'cpu', 'memory', 'load', 'disk', 'net']]]) {
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
  }
  to.onComplete?.();
}

global.gsap = {
  /* Accepted and ignored: what it sets — when the engine's frame callback goes
     back to sleep, whether it promotes a layer — is about a browser, and there
     is not one here. */
  config: () => {},
  defaults: () => {},
  to: (targets, vars) => runTween(targets, null, vars),
  from: (targets, vars) => runTween(targets, vars, { ...vars, duration: vars.duration }),
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
      from: (targets, vars) => { runTween(targets, vars, vars); finish(); return self; },
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
  for (const fn of windowListeners.viewport ?? []) fn({ detail: message });

  /* Bounded, because a shell bug that focuses in a loop should fail the test
     rather than hang it. */
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

emit({ type: 'config', layout: mode, rules: [
  { app_id: 'pinned', workspace: 6 },
  { app_id: 'dialogy', floating: true, x: 10, y: 20, width: 300, height: 200 },
] });
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

  const before = sent.length;
  emit({ type: 'notification.close', id: 7 });
  check('an application withdrawing one sends nothing back',
    !sent.slice(before).some((m) => String(m.type).startsWith('notification')));

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
    check('by just enough to fit it, and the configured gap',
      near(moved.x,
        away.x + away.width + canvas.followMargin() - AREA.width));
    check('and following never changes the zoom',
      moved.zoom === viewport.zoom);

    /* An oversized window shows its start rather than its end: both branches
       fire and the top-left one wins. */
    const huge = canvas.follow({ x: 0, y: 0, width: 9000, height: 9000 },
      at(500, 500), AREA);
    check('an oversized window is followed to its top left',
      near(huge.x, -canvas.followMargin())
      && near(huge.y, -canvas.followMargin()));
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

    emit({ type: 'view.removed', id: 30 });
    emit({ type: 'view.removed', id: 31 });
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
    const screen = canvas.area(globalThis.__shell.outputs.get(S_NAME));
    emit({ type: 'shell.command', command: 'canvas.fill', args: [] });
    const filled = canvas.places.get(3);
    check('canvas.fill sizes the focused window to the screen',
      near(filled.width, screen.width / filling.zoom)
      && near(filled.height, screen.height / filling.zoom));
    check('and puts it where the view starts, so it covers it exactly',
      near(filled.x, filling.x) && near(filled.y, filling.y));
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
    /* Smart radius follows smart gaps unless it is set on its own: the same
       window the gaps pushed against the screen edge is the one whose rounded
       corner would show wallpaper through it. */
    check('and squares its corners with them',
      smart.radius() === true
      && out.windowsEl.classList.contains('smart-square'));
    check('which the compositor is told about per window',
      sent.some((m) => m.type === 'view.layout' && m.id === 320
        && m.square === true));

    /* Set apart: gaps still smart, corners explicitly not. */
    emit({ type: 'config', layout: mode, border: { smart: false } });
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
     but not in front of the pointer. The bar is revealed by Mod4, and Mod4 is
     the modifier a window is dragged, resized and focused with: a bar that
     took the pointer took every one of those in the strip it floats over, so
     a window moved up under it could not be touched again and the click
     panned the canvas instead. */
  const floating = () => (sent.filter((m) => m.type === 'shell.overlay')
    .at(-1)?.rects ?? []).filter((r) => r.height > 0);
  check('the revealed bar is drawn over the windows', floating().length > 0);
  check('and lets the pointer through to them',
    floating().every((r) => r.passthrough === true));

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
  check('scrolling a mic widget down lowers the source volume by 5%',
    micExec().slice(micBefore).some((m) =>
      m.command === 'wpctl set-volume @DEFAULT_AUDIO_SOURCE@ 5%-'));
  check('scrolling a mic widget refreshes the bar at once',
    sent.slice(micSentBefore).some((m) => m.type === 'status.refresh'));
  const micBeforeMute = micExec().length;
  const micSentBeforeMute = sent.length;
  micEl.listeners.contextmenu.forEach((fn) => fn({ preventDefault() {} }));
  check('right-clicking a mic widget toggles the source mute',
    micExec().slice(micBeforeMute).some((m) =>
      m.command === 'wpctl set-mute @DEFAULT_AUDIO_SOURCE@ toggle'));
  check('muting a mic widget refreshes the bar at once',
    sent.slice(micSentBeforeMute).some((m) => m.type === 'status.refresh'));
  emit({ type: 'config', layout: mode });

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
  check('scrolling a volume widget up raises volume by 5%',
    execAfter().slice(before).some((m) =>
      m.command === 'wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+'));
  check('scrolling asks the compositor to refresh the bar at once',
    sent.slice(sentBeforeScroll).some((m) => m.type === 'status.refresh'));
  const beforeMute = execAfter().length;
  const sentBeforeMute = sent.length;
  volEl.listeners.contextmenu.forEach((fn) => fn({ preventDefault() {} }));
  check('right-clicking a volume widget toggles mute',
    execAfter().slice(beforeMute).some((m) =>
      m.command === 'wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle'));
  check('muting asks the compositor to refresh the bar at once',
    sent.slice(sentBeforeMute).some((m) => m.type === 'status.refresh'));
  const beforeDisk = execAfter().length;
  diskEl.listeners.click.forEach((fn) => fn());
  check('clicking a disk widget opens its mount',
    execAfter().slice(beforeDisk).some((m) =>
      m.command.includes('xdg-open') && m.command.includes('/games')));
  const beforeClock = execAfter().length;
  clockEl.listeners.click.forEach((fn) => fn());
  clockEl.listeners.wheel.forEach((fn) =>
    fn({ preventDefault() {}, deltaY: 100 }));
  check('a module (non-widget) element sends nothing on click or scroll',
    execAfter().length === beforeClock);

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
  probe.remove();

  emit({ type: 'view.removed', id: 80 });
  emit({ type: 'view.removed', id: 81 });
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
