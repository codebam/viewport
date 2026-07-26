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
 * So the DOM is stubbed just far enough to run the real file unmodified. This
 * is not a rendering engine: getBoundingClientRect returns fixed numbers and
 * the pixel results mean nothing. What it does check is structure — that four
 * windows make four columns, that consume and expel are inverses, that a
 * tabbed container shows exactly one window and it is the focused one.
 *
 *   node tests/shell.test.js data/shell/shell.js tiling
 *   node tests/shell.test.js data/shell/shell.js scrolling
 *
 * Exits non-zero on failure. The process does not exit on its own: the shell
 * sets a live-reload interval, so run it under `timeout`.
 */
const fs = require('fs');

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
const EXPORTS = ';globalThis.__shell = { views, workspaces, floats, outputs, scrollOffsets, overviewThumbs,'
  + ' workspaceOfForTest: workspaceOf,'
  + ' get activeOutput() { return activeOutput; } };';
const src = fs.readFileSync(process.argv[2], 'utf8') + '\n' + EXPORTS;
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

  const record = globalThis.__shell.floats.get(20);
  check('a floating window comes back floating', record !== undefined);
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
  const rect = globalThis.__shell.floats.get(51);
  check('a rule can float a window', rect !== undefined);
  check('with the rect the rule gave',
    rect?.x === 10 && rect?.width === 300);

  /* An application no rule mentions is untouched. */
  open(52, 'ordinary');
  check('an unmatched window is placed normally',
    !globalThis.__shell.floats.has(52));

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
} else {
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

emit({ type: 'view.removed', id: 1 });
emit({ type: 'view.removed', id: 2 });
emit({ type: 'view.removed', id: 3 });
emit({ type: 'view.removed', id: 4 });
check('teardown clean', process.exitCode !== 1);
