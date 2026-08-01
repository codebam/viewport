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
  for (const c of ['windows', 'empty', 'workspaces', 'taskbar', 'mode',
      'clock', 'cpu', 'memory', 'load', 'disk', 'net']) {
    const el = new El('div');
    el.className = `${c} module`;
    main.append(el);
  }
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

global.document = {
  documentElement,
  getElementById: (id) => ({
    outputs: outputsEl,
    notifications: new El('div'),
    screencast: screencastEl,
    'desktop-template': desktopTemplate,
    'window-template': windowTemplate,
  }[id]),
  createElement: (tag) => new El(tag),
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
  .map((m) => m[1]);

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

/* Notifications are the compositor's on D-Bus and the shell's on screen. */
{
  emit({ type: 'notification.add', id: 7, app_name: 'test',
    summary: 'hello', body: 'world', urgency: 1, timeout: 0,
    actions: [{ key: 'reply', label: 'Reply' }] });

  const before = sent.length;
  emit({ type: 'notification.close', id: 7 });
  check('an application withdrawing one sends nothing back',
    !sent.slice(before).some((m) => String(m.type).startsWith('notification')));

  /* A critical notification never expires on its own, so it must still be
     there after any timer would have run. */
  emit({ type: 'notification.add', id: 8, app_name: 'test',
    summary: 'critical', body: '', urgency: 2, timeout: -1, actions: [] });
  check('a critical notification is kept', true);
  emit({ type: 'notification.close', id: 8 });
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

  emit({ type: 'modifiers', logo: false });
  check('letting go hides it again', barHidden());

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
     overlaps; there is no window in that layout that is tiled in this sense. */
  if (mode !== 'solar') {
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
     handler returned early unless the window already floated. */
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

  /* Resize mode looks the focused window up in the tree, and a floating window
     is not in it — so every press was ignored. */
  emit({ type: 'view.focused', id: 90 });
  const before = globalThis.__shell.floatingForTest(90).width;
  emit({ type: 'shell.command', command: 'layout.resize', args: ['right'] });
  const after = globalThis.__shell.floatingForTest(90).width;
  check('resize mode grows a floating window', after > before);
  emit({ type: 'shell.command', command: 'layout.resize', args: ['left'] });
  check('and shrinks it again',
    globalThis.__shell.floatingForTest(90).width === before);

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
     Solar draws the window beside the focused one in its own colour — an orbit
     is a tier and not merely "not focused" — so the second half of the pair is
     asked for there rather than the resting grey. */
  const besideColour = mode === 'solar'
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
     could drag. The area's own inset below still applies to it. */
  const divider = output.windowsEl.querySelector('.divider');
  if (mode !== 'solar') {
    check('the shell drew a real element in the gap', divider !== null);
    check('as wide as the gap, and it never grows',
      sheet.value(divider, 'flex-basis') === gap &&
      sheet.value(divider, 'flex-grow') === '0' &&
      sheet.value(divider, 'flex-shrink') === '0');
    check('and its container leaves no CSS gap for it to sit on top of',
      sheet.value(divider.parentElement, 'gap') === '');
  }
  check('the tiling area is inset by that same gap',
    ['top', 'right', 'bottom', 'left'].every((side) =>
      sheet.value(output.windowsEl, `padding-${side}`) === gap));
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
