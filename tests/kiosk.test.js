/* SPDX-License-Identifier: MIT
 *
 * The kiosk shell, without a browser.
 *
 * Same technique as tests/shell.test.js: stub the DOM far enough to run the
 * real file, then drive it with the messages the compositor would send and
 * check what it sends back. Nothing here renders, and the pixel numbers are
 * whatever the stub was told to return.
 *
 * Worth testing despite being an example. An example that has never been run is
 * a worked demonstration of something that may not work, and this one is the
 * documentation for how a minimal shell answers the protocol — if it is wrong,
 * it is wrong in every shell anyone writes by copying it. The specific things
 * that would be invisible otherwise: that the application is told to fill the
 * output rather than some default rect, that a dialog does not steal the screen
 * from the application underneath it, and that a second monitor does not end up
 * with the same window on it.
 *
 *   node tests/kiosk.test.js examples/kiosk
 */
const fs = require('fs');

let failures = 0;

function check(what, ok) {
  console.log(`${ok ? 'ok   ' : 'not ok'} ${what}`);
  if (!ok) failures++;
}

/* ------------------------------------------------------------------------
 * DOM stub
 * --------------------------------------------------------------------- */

class El {
  constructor(tag) {
    this.tagName = tag;
    this.children = [];
    this.parentElement = null;
    this._classes = new Set();
    this.hidden = false;
    this.textContent = '';
    this.style = new Proxy({}, {
      set: (t, k, v) => { t[k] = v; return true; },
      get: (t, k) => t[k] ?? '',
    });
  }
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
      if (n.parentElement) n.parentElement.remove_child(n);
      n.parentElement = this;
      this.children.push(n);
    }
  }
  remove_child(n) { this.children = this.children.filter((c) => c !== n); }
  remove() { if (this.parentElement) this.parentElement.remove_child(this); }
  querySelector(sel) {
    const want = sel.replace(/^\./, '');
    const stack = [...this.children];
    while (stack.length) {
      const el = stack.shift();
      if (el._classes.has(want)) return el;
      stack.push(...el.children);
    }
    return null;
  }
  /* The shell positions a screen with style.left/top/width/height and then
   * measures it back. Reporting what was set is what makes "measured, never
   * assumed" testable at all — a stub returning a constant would pass no
   * matter what the shell wrote. */
  getBoundingClientRect() {
    const n = (v) => parseInt(String(v).replace('px', ''), 10) || 0;
    if (this._classes.has('viewport') && this.parentElement) {
      return this.parentElement.getBoundingClientRect();
    }
    return {
      left: n(this.style.left), top: n(this.style.top),
      width: n(this.style.width), height: n(this.style.height),
    };
  }
}

function buildScreen() {
  const root = new El('div');
  const section = new El('section');
  section._classes.add('screen');
  const hole = new El('div');
  hole._classes.add('viewport');
  const message = new El('p');
  message._classes.add('message');
  section.append(hole, message);
  root.append(section);
  return root;
}

function buildDialog() {
  const root = new El('div');
  const section = new El('section');
  section._classes.add('dialog');
  const hole = new El('div');
  hole._classes.add('viewport');
  section.append(hole);
  root.append(section);
  return root;
}

const screensEl = new El('div');
const sent = [];

global.document = {
  getElementById: (id) => ({
    screens: screensEl,
    'screen-template': { content: { cloneNode: () => buildScreen() } },
    'dialog-template': { content: { cloneNode: () => buildDialog() } },
  }[id]),
};

const windowListeners = {};
global.window = {
  webkit: { messageHandlers: { viewport: {
    postMessage: (m) => sent.push(JSON.parse(m)),
  } } },
  addEventListener: (type, fn) => { (windowListeners[type] ??= []).push(fn); },
};
global.ResizeObserver = class { observe() {} unobserve() {} };

const dir = process.argv[2];
(0, eval)(fs.readFileSync(`${dir}/kiosk.js`, 'utf8'));

function emit(message) {
  for (const fn of windowListeners.viewport ?? []) fn({ detail: message });
}

const lastOf = (type, id) => [...sent].reverse()
  .find((m) => m.type === type && (id === undefined || m.id === id));

/* ------------------------------------------------------------------------
 * The messages a compositor actually sends, in the order it sends them
 * --------------------------------------------------------------------- */

check('it asks for the outputs and the window list on load',
  sent.some((m) => m.type === 'output.query') &&
  sent.some((m) => m.type === 'view.query'));

const OUTPUT = {
  name: 'HEADLESS-1', enabled: true,
  x: 0, y: 0, width: 1920, height: 1080,
  usable_x: 0, usable_y: 0, usable_width: 1920, usable_height: 1080,
};

emit({ type: 'output.layout', outputs: [OUTPUT] });
check('and asks again once it knows where the screens are',
  sent.filter((m) => m.type === 'view.query').length >= 2);

/* Nothing is running yet. */
const screenEl = screensEl.children[0];
check('an empty kiosk says it is starting',
  screenEl.querySelector('message').textContent.includes('Starting'));

/* The application maps. */
sent.length = 0;
emit({ type: 'view.added', id: 1, app_id: 'the-app', title: 'App',
       output: 'HEADLESS-1', width: 800, height: 600 });

const layout = lastOf('view.layout', 1);
check('the application is given the whole output',
  layout !== undefined && layout.x === 0 && layout.y === 0 &&
  layout.width === 1920 && layout.height === 1080);
check('and is given the keyboard, which the shell held until now',
  lastOf('view.focus', 1) !== undefined);
check('and the waiting message goes away',
  screenEl.querySelector('message').hidden === true);

/* A dialog opens on top of it — a print dialog, a file chooser. */
sent.length = 0;
emit({ type: 'view.added', id: 2, app_id: 'the-app', title: 'Print',
       output: 'HEADLESS-1', width: 400, height: 300 });

const dialogLayout = lastOf('view.layout', 2);
check('a second window is drawn rather than refused',
  dialogLayout !== undefined);
check('at the size it asked for',
  dialogLayout.width === 400 && dialogLayout.height === 300);
check('centred on the screen',
  dialogLayout.x === (1920 - 400) / 2 && dialogLayout.y === (1080 - 300) / 2);
check('and it does not take the screen from the application',
  lastOf('view.layout', 1) === undefined ||
  lastOf('view.layout', 1).width === 1920);

/* The dialog closes. */
sent.length = 0;
emit({ type: 'view.removed', id: 2 });
check('closing a dialog hands the keyboard back to the application',
  lastOf('view.focus', 1) !== undefined);

/* A second monitor appears. */
sent.length = 0;
emit({ type: 'output.layout', outputs: [OUTPUT, {
  name: 'HEADLESS-2', enabled: true,
  x: 1920, y: 0, width: 1280, height: 720,
  usable_x: 1920, usable_y: 0, usable_width: 1280, usable_height: 720,
}] });

check('a second monitor gets a screen of its own',
  screensEl.children.filter((c) => c._classes.has('screen')).length === 2);
const second = screensEl.children.filter((c) => c._classes.has('screen'))[1];
check('which says it is not in use',
  second.querySelector('message').textContent.includes('not in use'));
check('and the application stays on the first',
  lastOf('view.layout', 1).x === 0 &&
  lastOf('view.layout', 1).width === 1920);

/* Commands are ignored: there is nothing here for them to do. */
sent.length = 0;
emit({ type: 'shell.command', command: 'workspace.switch', args: ['3'] });
emit({ type: 'shell.command', command: 'layout.overview', args: [] });
emit({ type: 'status.update', cpu: 50 });
emit({ type: 'session.restore', state: '{"bogus":true}' });
check('forwarded commands do nothing at all', sent.length === 0);

/* The application exits. */
emit({ type: 'view.removed', id: 1 });
check('losing the application says so, and distinguishes it from starting',
  screenEl.querySelector('message').textContent.includes('closed') &&
  screenEl.querySelector('message').hidden === false);

/* And comes back, as a supervisor would restart it. */
sent.length = 0;
emit({ type: 'view.added', id: 3, app_id: 'the-app', title: 'App',
       output: 'HEADLESS-1', width: 800, height: 600 });
check('a restarted application takes the screen again',
  lastOf('view.layout', 3) !== undefined &&
  lastOf('view.layout', 3).width === 1920);

/* A malformed or surprising message must not take the kiosk down: the whole
 * point of one is that it stays up unattended. */
emit({ type: 'view.removed', id: 999 });
emit({ type: 'output.layout', outputs: undefined });
emit({ type: 'view.added', id: 4 });
check('and nothing above threw', true);

console.log(`${failures === 0 ? 'ok   ' : 'not ok'} ${failures} failure(s)`);
process.exit(failures === 0 ? 0 : 1);
