/* SPDX-License-Identifier: MIT
 *
 * The layout, measured instead of declared.
 *
 * tests/shell.test.js stubs the DOM, so it can say which CSS rule wins and
 * nothing whatever about what that rule does: getBoundingClientRect there
 * returns a fixed number, and every rect the shell reports under it is
 * fiction. This runs the same shell in the browser that ships it — WPE WebKit,
 * inside a real compositor — and reads back the rectangles it measured.
 *
 * Those rectangles are the entire contract. The shell draws a frame with a
 * hole in it, measures where the hole landed, and sends `view.layout`; the
 * compositor puts the client there and nowhere else. A rect that is right here
 * is a window that is right on screen, which is a thing no amount of cascade
 * checking can establish — `--gap: 8px` winning the cascade says nothing about
 * whether eight pixels end up between two windows.
 *
 *   node tests/layout.test.js result/bin/viewport
 *   node tests/layout.test.js target/debug/viewport data/shell
 *
 * What it needs, and why it is not in the `shell` CI job:
 *
 *   - a compositor to nest in, because the shell only starts on the winit and
 *     udev backends. `--headless` runs no WebKit at all: `start_shell` has two
 *     callers, in winit.rs and udev.rs, and headless.rs is neither.
 *   - a DRM device, because WPE is handed a primary node and a render node and
 *     asserts on a null one.
 *   - foot, which supplies the windows. Nothing in the compositor can conjure
 *     one: `view.added` is an event rather than a request, so the control
 *     socket cannot fake a window and a real client has to map a real surface.
 *
 * A hosted runner has neither of the first two, so this is a test for a machine
 * with a screen. Run it before touching shell.css.
 *
 * Nothing here is a fixed number. Nested, the output is the host compositor's
 * window: it arrives at whatever size the host chose and can be resized while
 * the test is running, so every expectation is derived from what the compositor
 * reports at the moment of measuring — and `--gap`, `--bar` and the frame's
 * border width are read out of shell.css through css.js. A theme that moves any
 * of them moves what is expected here, which is the point: what is being
 * checked is that the browser produced what the stylesheet asked for, not that
 * either of them says 8.
 *
 * Exits non-zero on failure. About five seconds, most of it waiting for
 * geometry to stop moving.
 */
'use strict';

const fs = require('fs');
const net = require('net');
const path = require('path');
const { spawn } = require('child_process');
const css = require('./css.js');

const binary = process.argv[2];
const shellDir = path.resolve(process.argv[3] ?? 'data/shell');

if (!binary || !fs.existsSync(binary)) {
  console.error(
    'usage: node tests/layout.test.js PATH-TO-VIEWPORT [SHELL-DIR]');
  process.exit(2);
}

/* Checked here rather than left to the compositor, because each of these comes
 * back from further down as something that names the wrong thing: a session
 * that is not there is "winit backend: Failed to initialize an event loop", and
 * a stale WAYLAND_DISPLAY looks exactly like a compositor that will not
 * start. */
for (const [ok, complaint] of [
  [process.env.WAYLAND_DISPLAY &&
    fs.existsSync(path.join(process.env.XDG_RUNTIME_DIR ?? '/run/user/1000',
      process.env.WAYLAND_DISPLAY)),
  `WAYLAND_DISPLAY=${process.env.WAYLAND_DISPLAY ?? '(unset)'} names no socket:`
  + ' this nests inside a running session, and there has to be one'],
  [fs.existsSync('/dev/dri'),
    'no /dev/dri: WPE is handed a primary node and a render node, and asserts'
    + ' on a null one'],
]) {
  if (!ok) {
    console.error(`unable to run: ${complaint}`);
    process.exit(2);
  }
}

function check(label, condition) {
  console.log(`${condition ? 'ok  ' : 'FAIL'} ${label}`);
  if (!condition) process.exitCode = 1;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/* Long enough for a slow machine to start WebKit and lay a page out, short
 * enough that a shell which is never going to answer fails the run rather than
 * hanging it. */
const DEADLINE = 30_000;
/* The shell resamples geometry every frame while anything is still moving —
 * see pumpGeometry — so the first rect a window gets is not its last. This is
 * how long the numbers have to stop changing before they count as final. */
const SETTLE = 500;

/* --- the compositor ---------------------------------------------------- */

const children = [];

function reap() {
  for (const child of children.splice(0)) {
    /* By pid, through the handle we were given. Nothing here ever matches on a
     * process name: this machine may well be running the compositor as the
     * session these tests are nested inside. */
    try { child.kill('SIGTERM'); } catch { /* already gone */ }
  }
}
process.on('exit', reap);
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => { reap(); process.exit(1); });
}

class Compositor {
  constructor() {
    /* /tmp with the pid rather than a temporary directory: the path has to fit
     * in sockaddr_un.sun_path, and the compositor refuses one that does not. */
    this.socket = `/tmp/viewport-layout-${process.pid}.sock`;
    this.logPath = `/tmp/viewport-layout-${process.pid}.log`;
    this.configHome = `/tmp/viewport-layout-${process.pid}.config`;
  }

  /* `rules` goes into a config file because that is the only way in: the
   * control socket speaks the shell's half of the protocol, and `config` is
   * the compositor's half — there is no request that says "float this". */
  start(rules = []) {
    fs.rmSync(this.socket, { force: true });
    fs.rmSync(this.logPath, { force: true });
    fs.mkdirSync(`${this.configHome}/viewport`, { recursive: true });
    fs.writeFileSync(`${this.configHome}/viewport/config.json`,
      JSON.stringify({ layout: 'tiling', rules }));

    const log = fs.openSync(this.logPath, 'w');
    const child = spawn(binary, ['--socket', this.socket], {
      env: {
        ...process.env,
        /* The tree's own copy, not whatever is installed. */
        VIEWPORT_SHELL_URL: `file://${shellDir}/index.html`,
        /* `from shell:` is logged at debug on this target, which is where the
         * measured rects come from. Everything else stays at info, or a frame
         * of trace per vblank buries them. Turning debug on also turns the web
         * view's console on, so a page that throws says so here. */
        VIEWPORT_LOG: 'info,viewport::ipc=debug',
        XDG_CONFIG_HOME: this.configHome,
      },
      /* Both streams into one file. The compositor traces to stderr and
       * WebKit writes the page's console to stdout, and a shell that threw on
       * load says so only on the second — which is the difference between "the
       * layout is wrong" and "the layout never ran". */
      stdio: ['ignore', log, log],
    });
    children.push(child);
    this.child = child;
    return this;
  }

  log() {
    try { return fs.readFileSync(this.logPath, 'utf8'); } catch { return ''; }
  }

  async waitForLog(needle, what) {
    const deadline = Date.now() + DEADLINE;
    while (Date.now() < deadline) {
      if (this.log().includes(needle)) return;
      if (this.child.exitCode !== null) {
        throw new Error(`the compositor exited before ${what}:\n${this.log()}`);
      }
      await sleep(50);
    }
    throw new Error(`never saw ${what}:\n${this.log().slice(-4000)}`);
  }

  /* The shell is up when the compositor has acted on something it sent.
   *
   * Separated from waitForLog because the interesting case is not a timeout
   * but a diagnosis: a page that loaded and painted and still says nothing has
   * either thrown on load — in which case the console is in this same log,
   * which is why stdout is captured — or it has posted into a mailbox that
   * nothing on this backend drains. The second one is silent from every side,
   * and cost an afternoon to find once. */
  async waitForShell() {
    try {
      await this.waitForLog('shell is talking to us', 'the shell say anything');
    } catch (error) {
      const log = this.log();
      const loaded = log.includes('starting the shell at');
      const painted = log.includes('first shell frame imported');
      if (loaded && painted) {
        throw new Error(
          'the shell loaded and painted and the compositor received nothing '
          + 'from it. Nothing on this backend drains what the page posts: the '
          + 'calloop source that calls drain_shell is inserted in udev.rs and '
          + 'nowhere else, so on winit the messages never arrive. Run this on '
          + 'the DRM backend, or wire the ping into winit.rs.');
      }
      if (loaded) {
        throw new Error(
          `the shell loaded and never painted:\n${log.slice(-2000)}`);
      }
      throw error;
    }
  }

  /* The Wayland display it created, from its own log — the same line and the
   * same reasoning as crates/viewport/tests/control_socket.rs, because a
   * client pointed at a display that does not exist yet fails for a reason
   * that has nothing to do with what is under test. */
  async waylandDisplay() {
    await this.waitForLog('WAYLAND_DISPLAY=', 'the wayland display');
    const name = this.log().split(/\s+/)
      .find((word) => word.startsWith('WAYLAND_DISPLAY='))
      .slice('WAYLAND_DISPLAY='.length);
    return name.replace(/[^\w-]+$/, '');
  }

  /* Every `view.layout` the shell has sent, latest per window.
   *
   * The control socket cannot supply these. It carries the compositor's half
   * of the protocol outward — `view.added`, `output.layout` — and `view.layout`
   * runs the other way, from the page into `handle_request`, where it is acted
   * on rather than echoed. The log is where it surfaces, so the log is what
   * this reads. */
  rects() {
    const out = new Map();
    for (const line of this.log().split('\n')) {
      const at = line.indexOf('from shell: ');
      if (at === -1) continue;
      let message;
      try {
        message = JSON.parse(line.slice(at + 'from shell: '.length));
      } catch { continue; }
      if (message.type === 'view.layout') out.set(message.id, message);
    }
    return out;
  }

  /* Wait until every one of `ids` has a rect and none of them has moved for
   * SETTLE. Anything else races the animation and reads a window in flight. */
  async settled(ids, what) {
    const deadline = Date.now() + DEADLINE;
    let last = null;
    let since = Date.now();
    while (Date.now() < deadline) {
      const rects = this.rects();
      const shot = JSON.stringify(ids.map((id) => rects.get(id) ?? null));
      if (shot !== last) {
        last = shot;
        since = Date.now();
      } else if (ids.every((id) => rects.has(id)) &&
          Date.now() - since >= SETTLE) {
        return rects;
      }
      await sleep(50);
    }
    throw new Error(`the layout never settled for ${what}: ${last}`);
  }
}

/* --- the control socket ------------------------------------------------ */

class Control {
  constructor(socketPath) {
    this.pending = [];
    this.seen = [];
    this.buffer = '';
    this.outputs = null;
    this.outputsShot = null;
    this.outputsChanged = 0;
    this.stream = net.createConnection({ path: socketPath });
    this.stream.setEncoding('utf8');
    this.stream.on('data', (chunk) => this.feed(chunk));
    this.stream.on('error', () => { /* the compositor going away is the test's
      problem to report, not an unhandled error event */ });
  }

  static async connect(compositor) {
    const deadline = Date.now() + DEADLINE;
    while (Date.now() < deadline) {
      if (fs.existsSync(compositor.socket)) {
        const control = new Control(compositor.socket);
        await new Promise((resolve, reject) => {
          control.stream.once('connect', resolve);
          control.stream.once('error', reject);
        }).catch(() => null);
        if (!control.stream.destroyed) return control;
      }
      await sleep(50);
    }
    throw new Error(`the compositor never created ${compositor.socket}`);
  }

  feed(chunk) {
    this.buffer += chunk;
    let newline;
    while ((newline = this.buffer.indexOf('\n')) !== -1) {
      const line = this.buffer.slice(0, newline).trim();
      this.buffer = this.buffer.slice(newline + 1);
      if (line === '') continue;
      let message;
      try { message = JSON.parse(line); } catch { continue; }
      /* Kept as it arrives rather than queried when wanted. The compositor
       * broadcasts this whenever the layout changes, and nested the layout is
       * the host's window — which the host may resize at any moment, including
       * between measuring a window and working out what its rect should have
       * been. Holding the newest and noting when it last actually changed is
       * what lets a measurement say whether it was taken against a stale
       * screen. */
      if (message.type === 'output.layout') {
        const shot = JSON.stringify(message.outputs);
        if (shot !== this.outputsShot) {
          this.outputsShot = shot;
          this.outputsChanged += 1;
        }
        this.outputs = message.outputs;
      }
      this.seen.push(message);
      for (const waiter of this.pending.splice(0)) waiter();
    }
  }

  /* The output layout as of now, freshly asked for rather than remembered. */
  async output() {
    this.send({ type: 'output.query' });
    const layout = await this.waitFor((m) => m.type === 'output.layout',
      'the output layout');
    return layout.outputs;
  }

  send(message) {
    this.stream.write(`${JSON.stringify(message)}\n`);
  }

  /* Over everything received so far as well as everything still to come: the
   * answer to a query can arrive before the caller gets round to asking for
   * it, and a wait that only looked forward would hang on its own reply. */
  async waitFor(predicate, what) {
    const deadline = Date.now() + DEADLINE;
    let from = 0;
    while (Date.now() < deadline) {
      for (; from < this.seen.length; from++) {
        if (predicate(this.seen[from])) return this.seen[from];
      }
      await new Promise((resolve) => {
        this.pending.push(resolve);
        setTimeout(resolve, 100);
      });
    }
    throw new Error(`never saw ${what}`);
  }
}

/* --- what the stylesheet says the answers should be --------------------- */

/* The declared values, read through the same cascade the other test uses, so
 * the numbers asserted below are the stylesheet's rather than this file's. A
 * theme that moves --gap moves what is expected here, which is the point: the
 * check is that the browser produced what the stylesheet asked for. */
const root = {
  tagName: 'html', className: '', parentElement: null, style: {}, dataset: {},
};
const sheet = css.parse(fs.readFileSync(`${shellDir}/shell.css`, 'utf8'),
  { root });
const px = (value) => parseInt(value, 10);
const GAP = px(sheet.custom(root, '--gap'));
const BAR = px(sheet.custom(root, '--bar'));
/* The frame is the element the browser lays out; the hole inside it is what
 * gets measured and reported. They differ by the border, on every side, and
 * that difference is the whole design rather than a detail — so it is read out
 * of the stylesheet with the rest and converted between explicitly below. */
const BORDER = px(sheet.value(
  { tagName: 'section', className: 'window', parentElement: null, style: {},
    dataset: {} },
  'border-top-width'));

const same = (rect, expected) => rect !== undefined &&
  rect.x === expected.x && rect.y === expected.y &&
  rect.width === expected.width && rect.height === expected.height;

const show = (rect) => rect === undefined
  ? 'nothing'
  : `${rect.width}x${rect.height}+${rect.x}+${rect.y}`;

/* The frame that must have been laid out for this hole to land where it did.
 *
 * Everything the tiling tree computes — the flex widths, the divider between
 * two windows, the padding round the edge — is about `.window` elements, and
 * what comes back over IPC is the `.viewport` inside one. Comparing the two
 * directly is how the first draft of this file managed to be wrong by two
 * pixels in nine places at once. */
const frameOf = (hole) => hole === undefined ? undefined : {
  x: hole.x - BORDER,
  y: hole.y - BORDER,
  width: hole.width + BORDER * 2,
  height: hole.height + BORDER * 2,
};

/* Where the shell will have put things on this output, worked out the way the
 * stylesheet works it out.
 *
 * Nothing here is a constant. Nested, the output is the host's window and is
 * whatever size the host felt like — a tiled session hands over a column — so
 * every expected number is derived from what the compositor reports at the
 * moment of measuring. The two lengths that are not the compositor's come out
 * of shell.css, above.
 *
 *   .desktop  covers the output, positioned in layout coordinates
 *   .windows  inset by the bar and by whatever layer-shell clients reserved,
 *             which reaches the page as --rsv-*, then padded by --gap
 *
 * `pane` is that inset box and `area` is the space inside its padding, and the
 * difference matters: a floating window is absolutely positioned, so its
 * containing block is `.windows`' padding box — its rect is measured from
 * `pane`, while every tiled window sits inside `area`. */
function geometryOf(output) {
  const reserved = {
    top: output.usable_y - output.y,
    left: output.usable_x - output.x,
    right: (output.x + output.width) - (output.usable_x + output.usable_width),
    bottom: (output.y + output.height)
      - (output.usable_y + output.usable_height),
  };
  const pane = {
    x: output.x + reserved.left,
    y: output.y + BAR + reserved.top,
    width: output.width - reserved.left - reserved.right,
    height: output.height - BAR - reserved.top - reserved.bottom,
  };
  return {
    output,
    reserved,
    pane,
    area: {
      x: pane.x + GAP,
      y: pane.y + GAP,
      width: pane.width - GAP * 2,
      height: pane.height - GAP * 2,
    },
  };
}

/* --- drive it ---------------------------------------------------------- */

async function main() {
  const compositor = new Compositor().start([
    /* One window that is not tiled, at a rect nothing can round: the hole it
     * gets back is what says whether the frame was measured border-box or
     * content-box. */
    { app_id: 'floaty', floating: true,
      x: 120, y: 140, width: 500, height: 360 },
  ]);

  await compositor.waitForShell();
  const display = await compositor.waylandDisplay();
  const control = await Control.connect(compositor);

  const outputs = await control.output();
  check('the compositor came up with one output', outputs.length === 1);

  /* Rects and the screen they were measured against, together.
   *
   * Nested, the output is the host compositor's window: it arrives at whatever
   * size the host chose — a tiled session gives it a column — and the host can
   * resize it at any point, including between a window settling and this
   * working out where that window should have been. A rect compared against a
   * screen that has since changed size is a failure that says nothing, so the
   * output is re-read after the rects have settled and the whole measurement is
   * taken again if it moved underneath us. */
  const measure = async (ids, what) => {
    for (let attempt = 0; attempt < 4; attempt++) {
      const before = control.outputsChanged;
      const rects = await compositor.settled(ids, what);
      const [output] = await control.output();
      if (control.outputsChanged === before) {
        return { rects, ...geometryOf(output) };
      }
      /* Settling again from here rather than trusting the second read: the
       * shell relays out on a resize, so the rects that were stable a moment
       * ago are now in flight. */
    }
    throw new Error(
      `the host kept resizing the window while measuring ${what}; nothing `
      + 'measured against it can be compared to anything');
  };

  const windows = [];
  const openWindow = async (appId, extra = []) => {
    const child = spawn('foot',
      ['--app-id', appId, ...extra, '-e', 'sleep', '300'],
      { env: { ...process.env, WAYLAND_DISPLAY: display }, stdio: 'ignore' });
    children.push(child);
    const added = await control.waitFor(
      (m) => m.type === 'view.added' && m.app_id === appId,
      `a window from ${appId}`);
    windows.push({ appId, id: added.id, child });
    return added.id;
  };

  /* One window: the whole tiling area, which is the simplest statement of what
   * the bar height and the padding come to in practice. */
  const first = await openWindow('one');
  let { rects, area, pane } = await measure([first], 'the first window');
  check(`one window fills the tiling area (${show(frameOf(rects.get(first)))}`
    + ` in ${show(area)})`,
  same(frameOf(rects.get(first)), area));
  /* And what came back is the hole rather than the frame, which is the
   * distinction every other assertion here depends on. */
  check('and what it reported is the hole inside that frame',
    rects.get(first).width === area.width - BORDER * 2 &&
    rects.get(first).x === area.x + BORDER);

  /* Two: the gap between them is a real element, and this is the number that
   * proves it. Nothing in the compositor knows about a gap — it puts each
   * client exactly where it is told — so eight pixels of daylight here are
   * eight pixels the browser laid out from `.divider`. */
  const second = await openWindow('two');
  ({ rects, area } = await measure([first, second], 'two windows'));
  const a = frameOf(rects.get(first));
  const b = frameOf(rects.get(second));

  check(`two windows are side by side (${show(a)} ${show(b)})`,
    a.y === b.y && a.height === b.height && a.x < b.x);
  check('with the stylesheet\'s gap between them',
    b.x - (a.x + a.width) === GAP);
  check('and they share the width evenly',
    Math.abs(a.width - b.width) <= 1);
  check('leaving no slack at either edge',
    a.x === area.x && b.x + b.width === area.x + area.width);

  /* Three, because two halves can come out right by accident — the divider
   * share is spread over the columns, and a rounding mistake in it shows up
   * only once there is more than one divider to spread.
   *
   * To the pixel, and no tolerance: the browser lays these out in fractions
   * and the shell rounds each edge on its way out, so three windows whose
   * fractional widths sum exactly can still round to a row a pixel short. That
   * pixel is a seam of wallpaper between two windows, and it is worth knowing
   * about rather than rounding away here. */
  const third = await openWindow('three');
  ({ rects, area } = await measure([first, second, third], 'three windows'));
  const row = [first, second, third].map((id) => frameOf(rects.get(id)));
  check(`three windows fill the row exactly (${row.map(show).join(' ')}`
    + ` in ${show(area)})`,
  row[0].width + row[1].width + row[2].width + GAP * 2 === area.width);
  check('each gap between them is the same one',
    row[1].x - (row[0].x + row[0].width) === GAP &&
    row[2].x - (row[1].x + row[1].width) === GAP);
  check('and all three are the height of the area',
    row.every((r) => r.height === area.height && r.y === area.y));

  /* Closing one gives its space back to the others rather than leaving a hole,
   * which is the relayout the FLIP animates over.
   *
   * Here, while the layout is still plain. Later would measure something else
   * entirely: a fullscreen window switches the tiling area to the whole output
   * through `.desktop.has-fullscreen .windows`, and — being absolutely
   * positioned — leaves its divider behind in the flex row, so the windows
   * underneath it share the output minus a gap they are not separated by.
   * Nothing shows, because a fullscreen window covers all of it. */
  const closing = windows.find((w) => w.appId === 'three');
  closing.child.kill('SIGTERM');
  await control.waitFor((m) => m.type === 'view.removed' && m.id === closing.id,
    'the window to close');
  ({ rects, area } = await measure([first, second], 'the window closing'));
  const survivor = frameOf(rects.get(second));
  check(`closing a window gives its space back (${show(survivor)}`
    + ` in ${show(area)})`,
  survivor.x + survivor.width === area.x + area.width);
  check('and the one beside it kept the left edge',
    frameOf(rects.get(first)).x === area.x);

  /* Floating: the rect in the rule describes the hole, not the frame around
   * it. `.window.floating` is content-box for exactly this reason, and a
   * border-box regression makes the window four pixels short in each
   * direction — the size a client that cannot resize simply ignores. */
  const floaty = await openWindow('floaty');
  ({ rects, area, pane } = await measure([floaty], 'the floating window'));
  const dialog = rects.get(floaty);
  check(`a floating window is the size the rule asked for (${show(dialog)})`,
    dialog.width === 500 && dialog.height === 360);
  /* From the tiling area's own corner rather than from the page, which is what
   * keeps a dialog on the second monitor from being offset by the width of the
   * first — and from `.windows` rather than from the space inside its padding,
   * because an absolutely positioned child is placed against its containing
   * block's padding box. */
  check(`at the offset it asked for, inside ${show(pane)}`,
    frameOf(dialog).x === pane.x + 120 && frameOf(dialog).y === pane.y + 140);
  /* And the frame around it is reported separately, because it is the one
   * border that lands inside another window's surface. */
  check('and its frame is reported, two pixels larger all round',
    dialog.frame !== undefined &&
    dialog.frame.width === dialog.width + 4 &&
    dialog.frame.height === dialog.height + 4);

  /* Fullscreen: the output entire, bar and gap included. A client asking for
   * it itself is the path a video player takes, and it arrives at the shell as
   * a command rather than as anything the socket could send. */
  const big = await openWindow('big', ['--fullscreen']);
  let screen;
  ({ rects, output: screen } = await measure([big], 'the fullscreen window'));
  const full = rects.get(big);
  check(`fullscreen covers the whole output (${show(full)})`,
    same(full, {
      x: screen.x, y: screen.y, width: screen.width, height: screen.height,
    }));
  /* Compared without the frame conversion every other check here needs, and
   * that is the point of it: `.window.fullscreen` sets `border: 0`, so this is
   * the one window whose hole reaches the edge of the output instead of
   * sitting two pixels inside a frame. */
  check('with no frame inset left around it',
    full.x === screen.x && full.width === screen.width);

  reap();
  /* The compositor holds the log until it exits, and the socket file with it.
   * Left behind, the next run connects to a socket nobody is listening on. */
  await sleep(200);
  fs.rmSync(compositor.socket, { force: true });
  fs.rmSync(compositor.configHome, { recursive: true, force: true });
  console.log(`\nthe compositor's log is at ${compositor.logPath}`);
}

main().then(() => {
  check('teardown clean', process.exitCode !== 1);
  process.exit(process.exitCode ?? 0);
}).catch((error) => {
  console.error(`FAIL ${error.message}`);
  reap();
  process.exit(1);
});
