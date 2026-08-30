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

const MAX_WORKSPACES = 512;
const MAX_WORKSPACE_ID = 0xffffffff;

/* Numbered workspace identity is separate from its tree: an empty workspace
 * created by an external bar still exists, has a name, and survives a session.
 * The traditional nine are present from the start; larger numbers are made on
 * demand by commands, rules, config and ext-workspace-v1. */
const workspaceCatalog = new Map(); // number -> display name

function validWorkspaceId(value) {
  return Number.isInteger(value) && value >= 1 && value <= MAX_WORKSPACE_ID;
}

function ensureWorkspace(value, name = null) {
  const n = Number(value);
  if (!validWorkspaceId(n)) return null;
  if (!workspaceCatalog.has(n)) {
    if (workspaceCatalog.size >= MAX_WORKSPACES) return null;
    workspaceCatalog.set(n, String(n));
  }
  if (typeof name === 'string' && name.trim() !== '') {
    workspaceCatalog.set(n, name.trim());
  }
  return n;
}

function sortedWorkspaceIds() {
  return [...workspaceCatalog.keys()].sort((a, b) => a - b);
}

function nextWorkspaceId() {
  if (workspaceCatalog.size >= MAX_WORKSPACES) return null;
  let n = 1;
  while (workspaceCatalog.has(n)) n++;
  return validWorkspaceId(n) ? n : null;
}

for (let n = 1; n <= 9; n++) ensureWorkspace(n);

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
const physicalOutputs = new Map(); // name -> physical head info, including mirrors/off

/* How the bar behaves, from the config file: 'visible' always, 'hidden' never,
   or 'auto', which reveals it only while Mod4 is held. 'auto' exists for OLED
   panels, where a bar in the same pixels for hours is the thing that burns in. */
let barMode = 'visible';
let logoHeld = false;
/* Every open window. This is the only structure keyed by view id: anything
 * else a window needs is a field on its record, so dropping the record drops
 * the window entirely and there is nothing to keep in step by hand. */
const views = new Map();
// id -> { el, viewport, title, app_id, tag, box, naturalWidth, naturalHeight,
//         floating, special, specialOutput, specialHidden, overview }
const workspaces = new Map(); // number -> tiling tree root

/* Which monitor each workspace was last shown on. Nothing in the shell needs
 * it — a workspace goes wherever it is asked for — but `ext-workspace-v1` puts
 * every workspace in the group of an output, and a workspace nobody is
 * currently showing still has to say which screen it belongs to or an outside
 * bar has nowhere to draw it. Remembering where it was last is the only answer
 * that does not move it about while nobody is looking. */
const workspaceHomes = new Map(); // number -> output name
/* Validated config policy and changes made by layout commands. Runtime entries
 * are separate so a config reload changes defaults without erasing choices
 * made in this session; both are resolved by workspace at use sites. */
const workspaceRules = new Map(); // number -> { output, layout, tiling_mode, gaps }
const workspaceRuntime = new Map(); // number -> { layout, tiling_mode }
let renderingWorkspace = null;

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
    if (view.floating && !view.special) yield [id, view.floating, view];
  }
}

function* specialEntries() {
  for (const [id, view] of views) {
    if (view.floating && view.special) yield [id, view.floating, view];
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
const maximized = new Map(); // workspace -> view id
let lastStatus = {};
let statusOsdTimer = null;
let statusOsdOutput = null;
/* Authenticated AI usage is fetched by the compositor. Keys are provider names;
 * bearer credentials never enter this page. */
let aiUsage = new Map();
let aiAuth = new Map();
/* The system tray, as the compositor last sent it: one entry per registered
 * StatusNotifierItem. A snapshot rather than a list this shell maintains —
 * every tray.update replaces it whole, which is why nothing here reconciles
 * adds against removes. */
let trayItems = [];
/* The tray item whose menu is open, or null. The menu itself is drawn from
 * what the compositor sent and lives in the DOM; this is only what a click has
 * to name to send an answer back. */
let trayMenuOpen = null;
/* What is playing, as the compositor last read it off MPRIS, or null when
 * nothing is. Only sent while a media widget is on the bar — a desktop without
 * one does not follow the session's players at all. */
let mprisPlayer = null;
/* Battery, lid and profiles, as the compositor last read them off UPower,
 * or null when nothing has arrived. Only sent while a battery widget is
 * on the bar — lid policy can still run without one. */
let powerState = null;
let powerOpen = false;
/* The wireless radio and the Bluetooth adapter, as the compositor last read
 * them off NetworkManager and BlueZ, and whether either picker is on screen.
 * Both are null until a picker has been opened once: neither daemon is talked
 * to at all until something asks, because a scan is a radio transmitting. */
let networkState = null;
let networkOpen = false;
/* The network the passphrase box is open for, or null when it is not. One at a
 * time — it is a box under one row, not a dialog — and the name is what the
 * answer is sent back with. */
let networkAsking = null;
let bluetoothState = null;
let bluetoothOpen = false;
/* The settings panel: whether it is on screen, which window had the keyboard
 * before it took it, whether a display change is waiting to be confirmed, and
 * where the last save went (null when there has not been one this opening).
 *
 * The panel keeps no copy of any setting. Everything it draws comes out of
 * `shellConfig` and the outputs map below, so a value it has just sent is not
 * shown until the compositor has echoed it back — see settings.js for why. */
let settingsOpen = false;
let settingsRestoreId = null;
let settingsConfirming = false;
let settingsSaved = null;
/* The last `config` event, whole.
 *
 * Every other consumer of that message pulls out the one field it cares about
 * and lands it somewhere — a CSS custom property, a class on the document, a
 * variable here — which is right for the things that are *applied*. The
 * settings panel needs the opposite: the values as the compositor last stated
 * them, so it can draw a switch in the position it is really in rather than in
 * the position the page happens to have rendered. Reading them back out of the
 * stylesheet would be a parser nobody needs, and would answer with the
 * shell's fallback where the compositor said nothing. */
let shellConfig = {};
/* The clipboard history, as the compositor last sent it, and whether the
 * picker is on screen. The entries are kept whether or not it is open, because
 * they arrive on every copy and the picker opens without waiting for one. */
let clipboardEntries = [];
let clipboardOpen = false;
/* What has been notified, as the compositor last sent it, and whether the
 * centre is on screen. Kept whether or not it is open, for the same reason the
 * clipboard's entries are: they arrive as things happen and the centre opens
 * without waiting for one. The list itself lives in the compositor — this is a
 * copy of what it last said, and a reload asks again. */
let notificationHistory = [];
let notificationCentreOpen = false;
/* Manual DND belongs to this shell session. Screencast activity is sent by the
 * compositor, which owns the streams; picker visibility is not activity. */
let notificationDndManual = false;
let screencastActive = false;
/* How the bar's clock is written, from the config file's `clock` block, and
 * the calendar hanging under it. See calendar.js, which owns both — the grid
 * and the module above it have to agree about the locale or the desk is
 * reading a German month under an American date.
 *
 * All three null is the default and the common case: the locale the engine
 * runs under, the hour that locale writes, and the shape the shell ships. A
 * locale here has already been checked against `Intl` — a tag nothing can
 * parse never reaches this. */
let clockConfig = { locale: null, hour12: null, format: null };
/* Whether the calendar is on screen, which month it is showing (null is
 * whichever month today is in, so an opened-and-closed calendar always comes
 * back on today rather than where it was left three months ago), the element
 * it hangs from, and the day it was drawn for — a calendar left open overnight
 * would otherwise go on marking yesterday. */
let calendarOpen = false;
let calendarMonth = null;
let calendarAnchor = null;
let calendarDrawnDay = '';
let currentMode = 'default';
/* The layout models the shell implements. Set by the compositor from the
 * config file, and switched at runtime with `shell layout.model`.
 *
 *   tiling     i3-style splits — tiling.js
 *   scrolling  niri's endless strip of columns — scrolling.js
 *   solar      one window in the middle, the rest in orbit — solar.js
 *   matrix     the focused window large, the focus history halving away
 *              beside it — matrix.js
 *   canvas     an unbounded plane per workspace, panned and zoomed —
 *              canvas.js
 *
 * Extensions append themselves through registerLayout(), after their explicit
 * scripts have loaded and before the compositor replays any windows. */
const BUILTIN_LAYOUT_MODES = ['tiling', 'scrolling', 'solar', 'matrix', 'canvas'];
const LAYOUT_MODES = [...BUILTIN_LAYOUT_MODES];
const layoutRegistry = new Map();
const layoutSources = new Map();

function validLayoutDescriptor(name, descriptor) {
  return typeof name === 'string' && /^[A-Za-z0-9_-]+$/.test(name)
    && descriptor && typeof descriptor.render === 'function'
    && typeof descriptor.clear === 'function';
}

/* Public extension API. One name has one owner for the life of the page; in
 * particular, a local script cannot silently replace shipped policy. */
function registerLayout(name, descriptor) {
  if (!validLayoutDescriptor(name, descriptor)) {
    throw new TypeError('registerLayout requires a valid name and render/clear functions');
  }
  if (BUILTIN_LAYOUT_MODES.includes(name)) {
    throw new Error(`layout ${name} is built in and cannot be replaced`);
  }
  const script = document.currentScript;
  if (script?.dataset?.layoutName) {
    if (script.dataset.layoutName !== name
        || Number(script.dataset.layoutGeneration) !== layoutLoadGeneration) {
      throw new Error(`stale or mismatched layout registration: ${name}`);
    }
  }
  if (layoutRegistry.has(name)) throw new Error(`layout ${name} is already registered`);
  layoutRegistry.set(name, Object.freeze({ ...descriptor, name }));
  LAYOUT_MODES.push(name);
}

function registerBuiltinLayout(name, descriptor) {
  if (!BUILTIN_LAYOUT_MODES.includes(name) || !validLayoutDescriptor(name, descriptor)
      || layoutRegistry.has(name)) {
    throw new Error(`invalid built-in layout registration: ${name}`);
  }
  layoutRegistry.set(name, Object.freeze({ ...descriptor, name, builtin: true }));
}
let layoutMode = 'tiling';
/* How the tiling tree arranges itself: 'manual' is the splits you make, and
 * 'master-stack', 'spiral', 'bsp' and 'grid' derive the shape from which
 * windows are open. Only meaningful while layoutMode is 'tiling' — the
 * scrolling strip is its own model. See dynamic.js. */
let tilingMode = 'manual';

function workspaceRule(n) {
  return workspaceRules.get(n) ?? {};
}

function layoutModeOf(n = renderingWorkspace ?? activeWorkspace()) {
  return workspaceRuntime.get(n)?.layout ?? workspaceRule(n).layout ?? layoutMode;
}

function tilingModeOf(n = renderingWorkspace ?? activeWorkspace()) {
  return workspaceRuntime.get(n)?.tiling_mode
    ?? workspaceRule(n).tiling_mode ?? tilingMode;
}

function setWorkspaceRuntime(n, key, value) {
  const state = workspaceRuntime.get(n) ?? {};
  state[key] = value;
  workspaceRuntime.set(n, state);
  saveSession();
}
/* How a wallpaper picture is fitted to the screen. `fill` is the default and
 * is the absence of all four classes, so it is not in the list — what this is
 * for is taking the last one off when the mode changes. The names are stylix's
 * `imageScalingMode`, and the compositor holds the same list in config.rs. */
const WALLPAPER_MODES = ['fit', 'stretch', 'center', 'tile'];
/* Rules from the config file, applied to a window when it opens. Matched on
 * app_id, or on title where an application gives everything the same app_id. */
let windowRules = [];
/* Whether running off the end of the strip carries focus onto the next
 * monitor. From the config file; true unless it says otherwise, which is what
 * it has always done. The compositor honours the same setting for the tiling
 * layout, where it does the directional focus itself. */
let focusCrossesOutputs = true;
/* Notifications on screen, newest last. The compositor owns the D-Bus side and
 * hands them here; what they look like and how long they stay is the shell's.
 * Each entry carries the output its element was appended to, so drop and
 * report know which corner it belongs in. */
const notifications = new Map(); // id -> { el, timer, output }
/* Horizontal scroll offset per workspace, in pixels, for the scrolling layout.
 * Only ever adjusted to bring the focused column into view. */
const scrollOffsets = new Map();
/* While a three-finger swipe is in progress the strip follows the fingers
 * rather than the focused column, and the transition is off so it tracks them
 * exactly. Cleared on settle, when focus is moved to whichever column the
 * gesture landed on and the ordinary follow logic takes over again. */
let gestureWorkspace = null;
/* The compositor must decide ownership before the first update. This is the
 * last whole declaration sent to it, and the sequence whose ownership was
 * frozen when its begin arrived. */
let gestureCaptureDeclaration = '';
let liveGesture = null;
/* The workspace whose column edge is being dragged with the mouse, for the
 * same reason: renderStrip builds a new strip element every render, so the
 * class that turns the columns' transitions off has to be re-applied from
 * state rather than left on the element the mousedown happened to be on. */
let columnDragWorkspace = null;
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

/* `passthrough` names a rectangle that is drawn in front but does not take the
   pointer. The default is the other way round, and has to be: a notification
   over a window is visible, so a click on it belongs to the notification and
   not to whatever it covers.

   The bar under 'auto' is the exception, and the reason this option exists. It
   is on screen only while Mod4 is held — and Mod4 is the modifier every window
   gesture is on, so a bar that took the pointer took every Mod4+click and
   Mod4+drag in the strip it floats over. A window dragged up under it could
   not be focused, moved or resized again, and the click panned the canvas
   instead, because the compositor read it as a click on the desktop. Nothing
   is lost by declining: while the bar is visible Mod4 is down, and the
   compositor keeps those clicks for the gesture rather than forwarding them
   here. */
function setOverlay(name, el, { passthrough = false } = {}) {
  const rect = el?.getBoundingClientRect();
  if (!rect || rect.width < 1 || rect.height < 1) {
    if (!overlays.delete(name)) return;
  } else {
    const next = {
      x: Math.round(rect.left),
      y: Math.round(rect.top),
      width: Math.round(rect.width),
      height: Math.round(rect.height),
      passthrough,
    };
    const previous = overlays.get(name);
    if (previous
      && previous.x === next.x && previous.y === next.y
      && previous.width === next.width && previous.height === next.height
      && previous.passthrough === next.passthrough) {
      return;
    }
    overlays.set(name, next);
  }
  send({ type: 'shell.overlay', rects: [...overlays.values()] });
}

/* Everything a departing monitor left in the map.
 *
 * The per-output rectangles are keyed `kind:name` — `notifications:DP-1`,
 * `bar:DP-1` — and every one of them is reported by walking the outputs that
 * exist. A monitor that goes takes its entry out of that walk, so whatever it
 * had floating at that moment is never revisited and never cleared: the
 * compositor keeps drawing that piece of shell over the windows for the rest
 * of the session, as a rectangle nothing on screen accounts for and nothing
 * can dismiss.
 *
 * This is not the rare case it sounds like. A DisplayPort monitor coming back
 * from DPMS drops and reconnects — see `scan_device` in udev.rs — so an output
 * disappears and reappears every time the screens wake, and a notification up
 * when they went to sleep is exactly the thing left behind. */
function dropOverlaysForOutput(name) {
  let changed = false;
  for (const key of [...overlays.keys()]) {
    if (key.endsWith(`:${name}`) && overlays.delete(key)) changed = true;
  }
  if (changed) send({ type: 'shell.overlay', rects: [...overlays.values()] });
}
const screencastEl = document.getElementById('screencast');
const trayMenuEl = document.getElementById('tray-menu');
const clipboardEl = document.getElementById('clipboard');
const launcherEl = document.getElementById('launcher');
const notificationCentreEl = document.getElementById('notification-centre');
const powerEl = document.getElementById('power-picker');
const networkEl = document.getElementById('network-picker');
const bluetoothEl = document.getElementById('bluetooth-picker');
const calendarEl = document.getElementById('calendar');
const settingsEl = document.getElementById('settings');
const oskEl = document.getElementById('osk');
const lockEl = document.getElementById('lock');
const desktopTemplate = document.getElementById('desktop-template');
const windowTemplate = document.getElementById('window-template');
