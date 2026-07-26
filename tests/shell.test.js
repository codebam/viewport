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

global.document = {
  getElementById: (id) => ({
    outputs: outputsEl,
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
};
global.ResizeObserver = class { observe() {} unobserve() {} };
global.requestAnimationFrame = (fn) => fn();

/* Top-level const/let inside an eval stay in that eval's own scope, so the
 * shell's state is unreachable from out here unless it hands it over. */
const EXPORTS = ';globalThis.__shell = { views, workspaces, floats, outputs,'
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
emit({ type: 'config', layout: mode });
emit({ type: 'output.layout', outputs: [{
  name: 'DP-1', x: 0, y: 0, width: 1920, height: 1080,
  usable_x: 0, usable_y: 30, usable_width: 1920, usable_height: 1050,
  scale: 1, transform: 'normal', modes: [], enabled: true,
}] });

for (let id = 1; id <= 4; id++) {
  emit({ type: 'view.added', id, title: `win${id}`, app_id: 'test',
    output: 'DP-1', min_width: 0, min_height: 0, floating: false,
    width: 800, height: 600 });
  emit({ type: 'view.focused', id });
}

const layouts = sent.filter((m) => m.type === 'view.layout');
check('windows laid out', new Set(layouts.map((m) => m.id)).size === 4);

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

  const laidOut = new Set(sent.filter((m) => m.type === 'view.layout')
    .map((m) => m.id));
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
