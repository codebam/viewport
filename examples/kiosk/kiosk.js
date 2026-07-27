/* SPDX-License-Identifier: MIT
 *
 * A kiosk shell: one application, fullscreen, and nothing else.
 *
 * The reference shell in data/shell/ is a desktop — workspaces, tiling, a
 * launcher, a bar. This is the opposite end of the same protocol, and it exists
 * to show how little of it a shell is obliged to implement. There is no tiling
 * tree here and no layout policy: one window owns the screen, anything else it
 * opens is centred on top of it, and every command the compositor forwards is
 * ignored.
 *
 * What runs is not this file's decision. A shell is a web page and cannot spawn
 * a process — the compositor launches the application from its startup command
 * (`"startup"` in the config file, or `--startup`). All this does is make sure
 * whatever turns up owns the screen, and say something useful when nothing has.
 *
 * READ examples/kiosk/README.md BEFORE DEPLOYING THIS. A shell cannot lock a
 * machine down on its own, and two of the ways out are deliberate and cannot be
 * closed from here.
 */

/* ------------------------------------------------------------------------
 * Configuration
 * --------------------------------------------------------------------- */

const KIOSK = {
  /* app_id of the application that owns the screen. Null means "whatever maps
   * first", which is right when the compositor starts exactly one thing.
   *
   * Naming it is better if you can: with null, an application that opens a
   * splash screen before its real window can leave the splash owning the
   * screen for the rest of the session. `app_id` is what the client calls
   * itself — check the compositor log under --debug if you do not know it. */
  app: null,

  /* Shown while no application window exists. */
  waiting: 'Starting…',
  gone: 'The application closed. Waiting for it to come back…',

  /* What the other monitors show, if there are any. One application window
   * can only be on one screen. */
  idle: 'This screen is not in use.',
};

/* ------------------------------------------------------------------------
 * The compositor
 * --------------------------------------------------------------------- */

const bridge = window.webkit?.messageHandlers?.viewport;

function send(message) {
  if (!bridge) {
    console.warn('not running under viewport:', message);
    return;
  }
  bridge.postMessage(JSON.stringify(message));
}

/* ------------------------------------------------------------------------
 * State
 *
 * All of it. A kiosk has no workspaces, no focus history and no layout to
 * remember, so there is nothing to save and nothing to restore.
 * --------------------------------------------------------------------- */

/* name -> { el, holeEl, messageEl, box } */
const screens = new Map();

/* The window that owns the screen, or null. */
let appView = null;

/* Windows the application opened on top of itself — a print dialog, a file
 * chooser, an authentication prompt. A kiosk that cannot show these is a kiosk
 * where printing silently does nothing, so they are drawn rather than refused,
 * centred at whatever size they asked for. */
const dialogs = new Map(); // id -> { el, holeEl, width, height }

const screensEl = document.getElementById('screens');
const screenTemplate = document.getElementById('screen-template');
const dialogTemplate = document.getElementById('dialog-template');

/* ------------------------------------------------------------------------
 * Outputs
 * --------------------------------------------------------------------- */

/* The screen the application goes on: the first the compositor reported.
 *
 * "First" is meaningful rather than arbitrary — the compositor enumerates
 * outputs in the order they were connected and arranges them left to right in
 * the same order, so this is the leftmost screen. */
function primaryScreen() {
  return screens.values().next().value ?? null;
}

function syncOutputs(list) {
  const seen = new Set();

  for (const output of list ?? []) {
    if (!output.enabled) continue;
    seen.add(output.name);

    let screen = screens.get(output.name);
    if (!screen) {
      const el = screenTemplate.content.cloneNode(true).querySelector('.screen');
      screensEl.append(el);
      screen = {
        el,
        holeEl: el.querySelector('.viewport'),
        messageEl: el.querySelector('.message'),
      };
      screens.set(output.name, screen);
      observer.observe(screen.holeEl);
    }

    /* The web view is one canvas spanning the whole output layout, so a screen
     * is placed at the output's own coordinates within it. The usable area
     * rather than the full box: it is the same thing here, since a kiosk runs
     * no panels, but taking the full box would quietly break the moment
     * someone added one. */
    Object.assign(screen.el.style, {
      left: `${output.usable_x}px`,
      top: `${output.usable_y}px`,
      width: `${output.usable_width}px`,
      height: `${output.usable_height}px`,
    });
  }

  for (const [name, screen] of screens) {
    if (seen.has(name)) continue;
    observer.unobserve(screen.holeEl);
    screen.el.remove();
    screens.delete(name);
  }

  /* A monitor appearing or disappearing can move the application's screen. */
  render();
}

/* ------------------------------------------------------------------------
 * Placement
 *
 * Geometry is measured, never assumed — the same rule the reference shell
 * follows and for the same reason. A hole's rect changes for reasons no
 * message announces, and the compositor draws the client wherever it was last
 * told, so what gets reported is what the browser actually laid out.
 * --------------------------------------------------------------------- */

const observer = new ResizeObserver(() => report());

function reportHole(id, holeEl) {
  const rect = holeEl.getBoundingClientRect();
  const box = {
    x: Math.round(rect.left),
    y: Math.round(rect.top),
    width: Math.round(rect.width),
    height: Math.round(rect.height),
  };

  if (box.width <= 0 || box.height <= 0) {
    send({ type: 'view.visible', id, visible: false });
    return;
  }
  send({ type: 'view.layout', id, ...box });
}

function report() {
  const screen = primaryScreen();
  if (appView !== null && screen) {
    reportHole(appView, screen.holeEl);
  }
  for (const [id, dialog] of dialogs) {
    reportHole(id, dialog.holeEl);
  }
}

function render() {
  const screen = primaryScreen();

  for (const [, s] of screens) {
    const isPrimary = s === screen;
    s.el.classList.toggle('idle', !isPrimary);
    s.holeEl.hidden = !isPrimary || appView === null;
    s.messageEl.hidden = isPrimary && appView !== null;
    if (!isPrimary) {
      s.messageEl.textContent = KIOSK.idle;
    }
  }

  if (screen && appView === null) {
    screen.messageEl.textContent = everRan ? KIOSK.gone : KIOSK.waiting;
  }

  /* Dialogs are centred on the primary screen, at the size they asked for and
   * no larger than the screen itself. */
  if (screen) {
    const bounds = screen.el.getBoundingClientRect();
    for (const [, dialog] of dialogs) {
      const width = Math.min(dialog.width, bounds.width);
      const height = Math.min(dialog.height, bounds.height);
      Object.assign(dialog.el.style, {
        left: `${Math.round(bounds.left + (bounds.width - width) / 2)}px`,
        top: `${Math.round(bounds.top + (bounds.height - height) / 2)}px`,
        width: `${width}px`,
        height: `${height}px`,
      });
    }
  }

  report();
}

/* ------------------------------------------------------------------------
 * Windows
 * --------------------------------------------------------------------- */

/* Set once the application has been seen, so "starting" and "it closed" can be
 * told apart. They look the same on screen otherwise, and they are very
 * different problems. */
let everRan = false;

function isTheApplication(message) {
  if (appView !== null) return false;
  if (KIOSK.app === null) return true;
  return message.app_id === KIOSK.app;
}

function addView(message) {
  const { id } = message;

  if (isTheApplication(message)) {
    appView = id;
    everRan = true;
    render();
    /* Keyboard focus follows: the shell holds it until something takes it, and
     * a kiosk application that cannot be typed into is not much of one. */
    send({ type: 'view.focus', id });
    return;
  }

  const el = dialogTemplate.content.cloneNode(true).querySelector('.dialog');
  const holeEl = el.querySelector('.viewport');
  screensEl.append(el);
  dialogs.set(id, {
    el,
    holeEl,
    /* What the client asked for, with a floor so a client reporting nothing
     * does not end up at zero by zero and invisible. */
    width: Math.max(message.width || 0, message.min_width || 0, 320),
    height: Math.max(message.height || 0, message.min_height || 0, 200),
  });
  observer.observe(holeEl);
  render();
  send({ type: 'view.focus', id });
}

function removeView(id) {
  if (id === appView) {
    appView = null;
    /* Every dialog belonged to the window that just went away. */
    for (const [dialogId] of dialogs) {
      dropDialog(dialogId);
    }
    render();
    return;
  }
  if (dialogs.has(id)) {
    dropDialog(id);
    render();
    /* Hand the keyboard back to the application underneath. */
    if (appView !== null) send({ type: 'view.focus', id: appView });
  }
}

function dropDialog(id) {
  const dialog = dialogs.get(id);
  if (!dialog) return;
  observer.unobserve(dialog.holeEl);
  dialog.el.remove();
  dialogs.delete(id);
}

/* ------------------------------------------------------------------------
 * Inbound
 * --------------------------------------------------------------------- */

window.addEventListener('viewport', (event) => {
  const message = event.detail;

  switch (message.type) {
    case 'output.layout':
      syncOutputs(message.outputs);
      /* Windows that mapped before this shell finished loading are replayed in
       * answer to this, which is what makes a shell reload non-destructive. */
      send({ type: 'view.query' });
      break;

    case 'view.added':
      addView(message);
      break;

    case 'view.removed':
      removeView(message.id);
      break;

    /* Everything below is deliberately ignored, and listed rather than left to
     * the default so that "a kiosk does not do this" is written down.
     *
     *   shell.command   every keybinding the compositor forwards. A kiosk has
     *                   no workspaces to switch, nothing to tile and no bar to
     *                   toggle. Lock the keys down in the config file as well —
     *                   ignoring a command is not the same as it being
     *                   unreachable.
     *   session.restore there is no layout to restore. This shell never sends
     *                   session.save either, so the blob stays empty.
     *   status.update   nothing draws it.
     *   view.focused    the application is the only thing that ever holds
     *                   focus, and this shell put it there.
     *   config          layout, rules and theming are all desktop concepts.
     *   modifiers       there is no bar to reveal.
     */
    case 'view.props':
    case 'view.focused':
    case 'config':
    case 'modifiers':
    case 'status.update':
    case 'session.restore':
    case 'shell.command':
    case 'notification.add':
    case 'notification.close':
      break;

    case 'error':
      console.error(`viewport: ${message.context}: ${message.message}`);
      break;
  }
});

window.addEventListener('resize', render);

send({ type: 'output.query' });
send({ type: 'view.query' });
