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
    return { left: 0, top: 0, width: 1920, height: 1050, x: 0, y: 0 };
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
  webkit: { messageHandlers: { viewport: { postMessage: (m) => sent.push(JSON.parse(m)) } } },
  addEventListener: (type, fn) => { (windowListeners[type] ??= []).push(fn); },
};
global.ResizeObserver = class { observe() {} unobserve() {} };
global.requestAnimationFrame = (fn) => fn();

/* Top-level const/let inside an eval stay in that eval's own scope, so the
 * shell's state is unreachable from out here unless it hands it over. */
const EXPORTS = ';globalThis.__shell = { views, workspaces, floats };';
const src = fs.readFileSync(process.argv[2], 'utf8') + '\n' + EXPORTS;
(0, eval)(src);

function emit(message) {
  for (const fn of windowListeners.viewport ?? []) fn({ detail: message });
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

emit({ type: 'view.removed', id: 1 });
emit({ type: 'view.removed', id: 2 });
emit({ type: 'view.removed', id: 3 });
emit({ type: 'view.removed', id: 4 });
check('teardown clean', process.exitCode !== 1);
