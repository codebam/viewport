/* SPDX-License-Identifier: MIT
 *
 * Shell state, and the bridge to the compositor.
 *
 * Everything below is read and written from every other file: this is a set of
 * ordered classic scripts sharing one global scope, not modules, so a binding
 * declared here is the same binding there. Loaded first because the rest is
 * declarations and these are the values they act on.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */
const bridge = window.webkit?.messageHandlers?.viewport;

function send(message) {
  if (!bridge) {
    console.warn('not running under viewport:', message);
    return;
  }
  bridge.postMessage(JSON.stringify(message));
}

/* ------------------------------------------------------------------------
 * Configuration
 * --------------------------------------------------------------------- */

const WORKSPACES = 9;

/* ------------------------------------------------------------------------
 * Session
 *
 * Restarting the compositor kills every client with it, so nothing here
 * preserves processes — it preserves *places*. The tree is written down with
 * each window replaced by the application that was in it, and as those
 * applications come back they are put into the slot they left rather than
 * appended wherever there is room.
 *
 * A slot is an ordinary leaf whose id is negative: no real view ever has one,
 * so everything that walks the tree skips it (views.get returns undefined and
 * the renderers already drop leaves with no view). That means the structure,
 * the column widths and the weights all survive without a parallel
 * representation to keep in step.
 * --------------------------------------------------------------------- */

/* How long an unclaimed slot is held before the layout gives up on it. Long
 * enough for a slow application — a browser restoring its own session — and
 * short enough that a workspace is not permanently shaped around something
 * that is never coming back. */
const SLOT_TIMEOUT_MS = 45_000;

let nextSlotId = -1;
let slotsPending = 0;
/* Places kept for floating windows. Not slots in the tree — a floating window
 * has no position in it — so they are held separately and matched the same
 * way, by application. */
let floatSlots = [];

/* Which workspace a monitor starts on, by output name. Anything unlisted gets
 * the lowest workspace not already on screen. */
const OUTPUT_WORKSPACE = {
  'DP-1': 1,  // left
  'DP-3': 9,  // right
};

/* ------------------------------------------------------------------------
 * State
 * --------------------------------------------------------------------- */

const outputs = new Map(); // name -> desktop elements + workspace + barHidden

/* How the bar behaves, from the config file: 'visible' always, 'hidden' never,
   or 'auto', which reveals it only while Mod4 is held. 'auto' exists for OLED
   panels, where a bar in the same pixels for hours is the thing that burns in. */
let barMode = 'visible';
let logoHeld = false;
/* Every open window. This is the only structure keyed by view id: anything
 * else a window needs is a field on its record, so dropping the record drops
 * the window entirely and there is nothing to keep in step by hand. */
const views = new Map();
// id -> { el, viewport, title, app_id, box, naturalWidth, naturalHeight,
//         floating, overview }
const workspaces = new Map(); // number -> tiling tree root

/* Floating windows sit outside the tiling tree entirely: they keep their own
 * position and size and overlap whatever is tiled underneath. Dialogs land here
 * automatically because tiling them squeezes the window they belong to, and the
 * dialog itself usually cannot be resized to fill the slot it was given.
 *
 * That rect lives on the window's own record as `view.floating`, null when the
 * window is tiled. It was a second Map keyed by view id, which meant every
 * place a window went away had to remember to delete from both. */

function floatingOf(id) {
  return views.get(id)?.floating ?? null;
}

function isFloating(id) {
  return floatingOf(id) !== null;
}

/* Every floating window, as [id, rect, view]. Yields nothing for a window that
 * has no record, so callers do not have to check for one. */
function* floatingEntries() {
  for (const [id, view] of views) {
    if (view.floating) yield [id, view.floating, view];
  }
}

let focusedId = null;
let selectedContainer = null;
const selectedIds = new Set();

function clearSelection() {
  selectedContainer = null;
  selectedIds.clear();
}

let activeOutput = null;
/* Direction the next new window splits in, like sway's splith/splitv. */
let pendingSplit = 'horizontal';
/* Fullscreen is per workspace, not per session: a workspace lives on one
 * monitor at a time, so two monitors can each have something fullscreen and
 * neither cancels the other. A single global here meant fullscreening on the
 * second monitor silently un-fullscreened the first. */
const fullscreens = new Map(); // workspace -> view id
let lastStatus = {};
let currentMode = 'default';
/* 'tiling' (i3-style splits) or 'scrolling' (niri's strip of columns). Set by
 * the compositor from the config file; the shell implements both. */
let layoutMode = 'tiling';
/* How the tiling tree arranges itself: 'manual' is the splits you make, and
 * 'master-stack', 'spiral' and 'bsp' derive the shape from which windows are
 * open. Only meaningful while layoutMode is 'tiling' — the scrolling strip is
 * its own model. See dynamic.js. */
let tilingMode = 'manual';
/* Rules from the config file, applied to a window when it opens. Matched on
 * app_id, or on title where an application gives everything the same app_id. */
let windowRules = [];
/* Whether running off the end of the strip carries focus onto the next
 * monitor. From the config file; true unless it says otherwise, which is what
 * it has always done. The compositor honours the same setting for the tiling
 * layout, where it does the directional focus itself. */
let focusCrossesOutputs = true;
/* Notifications on screen, newest last. The compositor owns the D-Bus side and
 * hands them here; what they look like and how long they stay is the shell's. */
const notifications = new Map(); // id -> { el, timer }
/* Horizontal scroll offset per workspace, in pixels, for the scrolling layout.
 * Only ever adjusted to bring the focused column into view. */
const scrollOffsets = new Map();
/* While a three-finger swipe is in progress the strip follows the fingers
 * rather than the focused column, and the transition is off so it tracks them
 * exactly. Cleared on settle, when focus is moved to whichever column the
 * gesture landed on and the ordinary follow logic takes over again. */
let gestureWorkspace = null;
/* The overview: every workspace at once, scaled down. Windows are drawn shrunk
 * by the compositor rather than resized, so no client is asked to relayout
 * itself into a thumbnail — which many would refuse anyway, having a minimum
 * size larger than one. */
let overviewActive = false;
/* What the overview does to a window — the scale it is drawn at, so
 * reportGeometry can undo it, and the thumbnail bounding it, so the clip comes
 * from that rather than from the whole output — lives on the window's own
 * record as `view.overview`, set by renderOverview and cleared by
 * clearOverviewState(). It was two Maps keyed by view id, which worked but had
 * to be kept in step with the view list by hand at three separate sites. */

/* Thumbnails by workspace, so a drag can find what it was released over. This
 * one stays a Map: it is keyed by workspace, not by window, so there is no view
 * record for it to live on. */
const overviewThumbs = new Map(); // workspace -> thumbnail element

/* Forget every window's overview state. Called when the overview closes and at
 * the top of each relayout that rebuilds it. */
function clearOverviewState() {
  for (const view of views.values()) view.overview = null;
  overviewThumbs.clear();
}

const outputsEl = document.getElementById('outputs');
const notificationsEl = document.getElementById('notifications');

/* What the shell has drawn that belongs above the windows.
 *
 * The shell is one buffer *under* the clients, so anything it draws over a
 * window is behind it — a notification arrived, was drawn, and was covered by
 * whatever happened to be open. Naming the rectangles lets the compositor draw
 * this same buffer again, cropped to each, in front.
 *
 * Keyed by what put it there, so a notification and the screen-share chooser
 * can both float without either forgetting the other. */
const overlays = new Map();

function setOverlay(name, el) {
  const rect = el?.getBoundingClientRect();
  if (!rect || rect.width < 1 || rect.height < 1) {
    if (!overlays.delete(name)) return;
  } else {
    const next = {
      x: Math.round(rect.left),
      y: Math.round(rect.top),
      width: Math.round(rect.width),
      height: Math.round(rect.height),
    };
    const previous = overlays.get(name);
    if (previous
      && previous.x === next.x && previous.y === next.y
      && previous.width === next.width && previous.height === next.height) {
      return;
    }
    overlays.set(name, next);
  }
  send({ type: 'shell.overlay', rects: [...overlays.values()] });
}
const screencastEl = document.getElementById('screencast');
const desktopTemplate = document.getElementById('desktop-template');
const windowTemplate = document.getElementById('window-template');

