/* SPDX-License-Identifier: MIT
 *
 * User shell extensions runtime & live hot-reload.
 *
 * Discovered extensions live in `$XDG_CONFIG_HOME/viewport/extensions/<id>/`
 * with an `extension.json` manifest. The compositor validates each manifest
 * and asset path, watches the directory for edits, and sends `extensions.list`
 * and `extensions.changed` events carrying cache-busted `file://` URLs.
 *
 * This runtime mounts each extension into its declared slot (`bar-left`,
 * `bar-center`, `bar-right`, `desktop`, or `overlay`), exposes a read-only
 * reactive state API (`window.viewportExtensions`), isolates all extension
 * callbacks in `try/catch` so a broken extension never crashes or blanks the
 * shell, and performs surgical live hot-reload (unmount -> swap stylesheet &
 * script -> remount in place) without restarting the compositor or reloading
 * the desktop page.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

const VALID_SHELL_EXTENSION_SLOTS = new Set([
  'bar-left',
  'bar-center',
  'bar-right',
  'desktop',
  'overlay',
]);

/* Active validated manifests reported by the compositor, keyed by id. */
const shellExtensionManifests = new Map();
/* Registered extension definitions `{ id, mount, unmount }`, keyed by id. */
const shellExtensionDefs = new Map();
/* Mounted instances: id -> Map<scopeKey, Instance>.
   `scopeKey` is the output name for per-output slots, or 'overlay'. */
const shellExtensionInstances = new Map();
/* Per-extension lifecycle status for inspection and diagnostics. */
const shellExtensionStatus = new Map();
/* Monotonic load generation per id so stale script loads are dropped. */
const shellExtensionLoadGen = new Map();
/* Active read-only state subscribers `{ fn, extId }`. Early-returns with zero
   work when empty so desktops without extensions pay nothing. */
const shellExtensionSubscribers = new Set();
let notifyingShellExtensions = false;

function validShellExtensionId(id) {
  return typeof id === 'string' && /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/.test(id);
}

function freezeClone(value) {
  if (value === null || typeof value !== 'object') return value;
  if (Array.isArray(value)) {
    return Object.freeze(value.map((item) => freezeClone(item)));
  }
  const out = {};
  for (const [k, v] of Object.entries(value)) {
    out[k] = freezeClone(v);
  }
  return Object.freeze(out);
}

/* Read-only snapshot of live desktop state handed to extension `getState()`
   and `subscribe()` listeners. Every returned object/array is frozen so a
   user extension cannot mutate shell internals by accident. */
function buildShellExtensionState() {
  const activeOut = typeof activeOutputName === 'function' ? activeOutputName() : activeOutput;
  const activeWs = activeOut ? (outputs.get(activeOut)?.workspace ?? null) : null;

  const outputList = [...outputs.values()].map((o) => Object.freeze({
    name: o.name,
    workspace: o.workspace,
    active: o.name === activeOut,
    width: o.rect?.width ?? 0,
    height: o.rect?.height ?? 0,
    hdr: o.hdr === true,
  }));

  const workspaceList = sortedWorkspaceIds().map((id) => Object.freeze({
    id,
    name: workspaceCatalog.get(id) ?? String(id),
    output: hostOfWorkspace(id),
    active: id === activeWs,
    windowCount: idsOf(id).length,
  }));

  const windowList = [...views.entries()].map(([id, view]) => Object.freeze({
    id,
    title: view.title ?? '',
    app_id: view.app_id ?? '',
    workspace: workspaceOf(id),
    focused: id === focusedId,
    floating: isFloating(id),
    minimized: typeof isMinimized === 'function' ? isMinimized(id) : false,
    fullscreen: typeof isFullscreen === 'function' ? isFullscreen(id) : false,
    maximized: typeof isMaximized === 'function' ? isMaximized(id) : false,
  }));

  const s = lastStatus ?? {};
  const statusSnap = Object.freeze({
    cpu: s.cpu ?? -1,
    memory: s.memory ?? -1,
    load: s.load ?? 0,
    net_rx: s.net_rx ?? 0,
    net_tx: s.net_tx ?? 0,
    disk_free: s.disk_free ?? 0,
    disk_total: s.disk_total ?? 0,
    volume: s.volume ?? -1,
    muted: s.muted === true,
    mic_volume: s.mic_volume ?? -1,
    mic_muted: s.mic_muted === true,
    brightness: s.brightness ?? -1,
  });

  const clockSnap = Object.freeze({
    text: typeof clockTextCached === 'function' ? clockTextCached() : '',
    locale: clockConfig?.locale ?? null,
    hour12: clockConfig?.hour12 ?? null,
    format: clockConfig?.format ?? null,
  });

  return Object.freeze({
    activeOutput: activeOut ?? null,
    activeWorkspace: activeWs,
    focusedWindowId: focusedId ?? null,
    mode: currentMode,
    gameMode: typeof gameMode !== 'undefined' ? gameMode === true : false,
    outputs: Object.freeze(outputList),
    workspaces: Object.freeze(workspaceList),
    windows: Object.freeze(windowList),
    clock: clockSnap,
    status: statusSnap,
    power: powerState ? freezeClone(powerState) : null,
    mpris: mprisPlayer ? freezeClone(mprisPlayer) : null,
  });
}

/* Notify all active extension subscribers with a fresh read-only state
   snapshot. Completely inert when no extension has subscribed, and every
   listener is isolated in its own try/catch. */
function notifyShellExtensionsState() {
  if (shellExtensionSubscribers.size === 0 || notifyingShellExtensions) return;
  notifyingShellExtensions = true;
  try {
    const snapshot = buildShellExtensionState();
    for (const sub of [...shellExtensionSubscribers]) {
      if (!shellExtensionSubscribers.has(sub)) continue;
      try {
        sub.fn(snapshot);
      } catch (error) {
        console.error(
          `shell extension ${sub.extId}: subscriber threw: ${error?.message ?? error}`,
        );
      }
    }
  } finally {
    notifyingShellExtensions = false;
  }
}

/* Locate the slot container element for a given slot and optional output. */
function extensionSlotContainer(slot, output) {
  if (slot === 'overlay') {
    return document.getElementById('extensions-overlay');
  }
  if (!output?.el) return null;
  return output.el.querySelector(`.ext-slot-${slot}`);
}

/* Keep the compositor's overlay rect list in sync with mounted `overlay` slot
   extensions, and always hide them while the lock screen is active. */
function syncExtensionOverlays() {
  const overlayRoot = document.getElementById('extensions-overlay');
  const locked = typeof lockIsUp === 'function' && lockIsUp();

  let anyVisible = false;
  for (const [id, scopes] of shellExtensionInstances) {
    const manifest = shellExtensionManifests.get(id);
    if (!manifest || manifest.slot !== 'overlay') continue;
    const instance = scopes.get('overlay');
    if (!instance || locked) {
      setOverlay(`ext-overlay:${id}`, null);
      continue;
    }
    anyVisible = true;
    setOverlay(`ext-overlay:${id}`, instance.container, instance.overlayOpts);
  }

  if (overlayRoot) {
    overlayRoot.hidden = locked || !anyVisible;
  }
}

/* Tear down a single mounted instance cleanly, isolating every cleanup step
   so a broken `unmount()` can never leak DOM nodes or block hot-reload. */
function teardownExtensionInstance(instance) {
  for (const sub of instance.subscriptions) {
    shellExtensionSubscribers.delete(sub);
  }
  instance.subscriptions.clear();

  for (const fn of instance.cleanups.splice(0)) {
    try {
      fn();
    } catch (error) {
      console.error(
        `shell extension ${instance.id}: cleanup threw: ${error?.message ?? error}`,
      );
    }
  }

  if (typeof instance.def?.unmount === 'function') {
    try {
      instance.def.unmount(instance.container, instance.ctx);
    } catch (error) {
      console.error(
        `shell extension ${instance.id}: unmount threw: ${error?.message ?? error}`,
      );
    }
  }

  instance.container.remove();
  if (instance.slot === 'overlay') {
    setOverlay(`ext-overlay:${instance.id}`, null);
  }
}

/* Unmount every instance of extension `id` across all outputs / overlay. */
function unmountShellExtension(id) {
  const scopes = shellExtensionInstances.get(id);
  if (!scopes) return;
  for (const instance of scopes.values()) {
    teardownExtensionInstance(instance);
  }
  scopes.clear();
  shellExtensionInstances.delete(id);
  syncExtensionOverlays();
}

/* Remove previously injected `<script>` and `<link>` head tags for `id`. */
function removeExtensionHeadAssets(id) {
  const head = document.head;
  if (!head) return;
  for (const child of [...(head.children ?? [])]) {
    if (child?.dataset?.shellExtId === id) {
      child.remove();
    }
  }
}

/* Mount extension `id` into one target scope (`output` or overlay). */
function mountShellExtensionScope(manifest, def, scopeKey, output) {
  const slotEl = extensionSlotContainer(manifest.slot, output);
  if (!slotEl) return;

  let scopes = shellExtensionInstances.get(manifest.id);
  if (!scopes) {
    scopes = new Map();
    shellExtensionInstances.set(manifest.id, scopes);
  }
  const existing = scopes.get(scopeKey);
  if (existing) {
    teardownExtensionInstance(existing);
    scopes.delete(scopeKey);
  }

  const container = document.createElement('div');
  container.className = `ext-mount ext-mount-${manifest.slot}`;
  container.dataset.extId = manifest.id;
  container.dataset.extSlot = manifest.slot;
  container.dataset.extRev = manifest.revision;
  if (output?.name) container.dataset.extOutput = output.name;

  const subscriptions = new Set();
  const cleanups = [];
  const overlayOpts = { passthrough: false, blur: false, keepFocus: false };
  const options = manifest.options && typeof manifest.options === 'object'
    ? freezeClone(manifest.options)
    : Object.freeze({});

  const instance = {
    id: manifest.id,
    slot: manifest.slot,
    scopeKey,
    def,
    container,
    subscriptions,
    cleanups,
    overlayOpts,
    ctx: null,
  };

  const ctx = Object.freeze({
    id: manifest.id,
    name: manifest.name,
    slot: manifest.slot,
    revision: manifest.revision,
    output: output?.name ?? null,
    options,
    getState: () => buildShellExtensionState(),
    subscribe: (listener) => {
      if (typeof listener !== 'function') {
        throw new TypeError('ctx.subscribe requires a function');
      }
      const entry = { fn: listener, extId: manifest.id };
      shellExtensionSubscribers.add(entry);
      subscriptions.add(entry);
      try {
        listener(buildShellExtensionState());
      } catch (error) {
        console.error(
          `shell extension ${manifest.id}: initial subscriber threw: ${error?.message ?? error}`,
        );
      }
      return () => {
        shellExtensionSubscribers.delete(entry);
        subscriptions.delete(entry);
      };
    },
    onCleanup: (fn) => {
      if (typeof fn === 'function') cleanups.push(fn);
    },
    setOverlayOptions: (opts) => {
      if (!opts || typeof opts !== 'object') return;
      if (typeof opts.passthrough === 'boolean') overlayOpts.passthrough = opts.passthrough;
      if (typeof opts.blur === 'boolean') overlayOpts.blur = opts.blur;
      if (typeof opts.keepFocus === 'boolean') overlayOpts.keepFocus = opts.keepFocus;
      if (manifest.slot === 'overlay') syncExtensionOverlays();
    },
    log: (...args) => console.log(`[ext:${manifest.id}]`, ...args),
    warn: (...args) => console.warn(`[ext:${manifest.id}]`, ...args),
    error: (...args) => console.error(`[ext:${manifest.id}]`, ...args),
  });
  instance.ctx = ctx;

  slotEl.append(container);
  try {
    const ret = def.mount(container, ctx);
    if (typeof ret === 'function') {
      cleanups.push(ret);
    }
    scopes.set(scopeKey, instance);
    shellExtensionStatus.set(manifest.id, {
      status: 'mounted',
      error: null,
      revision: manifest.revision,
    });
  } catch (error) {
    /* Isolate mount failure: clean up any partial subscriptions/DOM and keep
       the desktop shell and all other extensions untouched. */
    for (const sub of subscriptions) shellExtensionSubscribers.delete(sub);
    subscriptions.clear();
    for (const fn of cleanups.splice(0)) {
      try { fn(); } catch (_) { /* already reporting mount failure */ }
    }
    container.remove();
    const message = String(error?.message ?? error);
    shellExtensionStatus.set(manifest.id, {
      status: 'error',
      error: message,
      revision: manifest.revision,
    });
    console.error(`shell extension ${manifest.id}: mount failed: ${message}`);
  }
}

/* Mount extension `id` across all matching target slots. */
function mountShellExtensionAll(id) {
  const manifest = shellExtensionManifests.get(id);
  const def = shellExtensionDefs.get(id);
  if (!manifest || !def) return;

  if (manifest.slot === 'overlay') {
    mountShellExtensionScope(manifest, def, 'overlay', null);
    syncExtensionOverlays();
    return;
  }
  for (const [name, output] of outputs) {
    mountShellExtensionScope(manifest, def, name, output);
  }
}

/* Called by outputs.js when a new monitor appears so every active per-output
   extension mounts onto it automatically. */
function mountExtensionsForOutput(output) {
  if (!output?.name) return;
  for (const [id, manifest] of shellExtensionManifests) {
    if (manifest.slot === 'overlay') continue;
    const def = shellExtensionDefs.get(id);
    if (!def) continue;
    mountShellExtensionScope(manifest, def, output.name, output);
  }
}

/* Called by outputs.js when a monitor is unplugged so per-output extension
   instances run their unmount/cleanup hooks and leak no timers or listeners. */
function unmountExtensionsForOutput(output) {
  if (!output?.name) return;
  for (const scopes of shellExtensionInstances.values()) {
    const instance = scopes.get(output.name);
    if (!instance) continue;
    teardownExtensionInstance(instance);
    scopes.delete(output.name);
  }
}

/* Load stylesheet + script for one extension (with stale-generation guard) and
   mount it once evaluated. */
function loadAndMountShellExtension(manifest, generation) {
  const head = document.head;
  if (!head) {
    /* Environments without `document.head` still allow already-registered
       definitions to mount directly. */
    if (shellExtensionDefs.has(manifest.id)) {
      mountShellExtensionAll(manifest.id);
    }
    return;
  }

  if (typeof manifest.style === 'string' && manifest.style.length > 0) {
    const link = document.createElement('link');
    link.rel = 'stylesheet';
    link.href = manifest.style;
    link.dataset.shellExtId = manifest.id;
    link.dataset.shellExtRev = manifest.revision;
    link.dataset.shellExtGen = String(generation);
    link.onerror = () => {
      console.error(`shell extension ${manifest.id}: failed to load stylesheet ${manifest.style}`);
    };
    head.append(link);
  }

  const script = document.createElement('script');
  script.src = manifest.script;
  script.async = false;
  script.dataset.shellExtId = manifest.id;
  script.dataset.shellExtRev = manifest.revision;
  script.dataset.shellExtGen = String(generation);
  script.onload = () => {
    if (shellExtensionLoadGen.get(manifest.id) !== generation) return;
    if (!shellExtensionDefs.has(manifest.id)) {
      const msg = `${manifest.script} did not call window.viewportExtensions.register(${JSON.stringify(manifest.id)}, ...)`;
      shellExtensionStatus.set(manifest.id, {
        status: 'error',
        error: msg,
        revision: manifest.revision,
      });
      console.error(`shell extension ${manifest.id}: ${msg}`);
      return;
    }
    mountShellExtensionAll(manifest.id);
  };
  script.onerror = () => {
    if (shellExtensionLoadGen.get(manifest.id) !== generation) return;
    const msg = `could not load ${manifest.script}`;
    shellExtensionStatus.set(manifest.id, {
      status: 'error',
      error: msg,
      revision: manifest.revision,
    });
    console.error(`shell extension ${manifest.id}: ${msg}`);
  };
  head.append(script);
}

function sameShellExtensionManifest(a, b) {
  if (!a || !b) return false;
  return a.id === b.id
    && a.name === b.name
    && a.slot === b.slot
    && a.script === b.script
    && (a.style ?? null) === (b.style ?? null)
    && a.revision === b.revision
    && JSON.stringify(a.options ?? null) === JSON.stringify(b.options ?? null);
}

/* Reconcile the authoritative extension list from `extensions.list` or
   `extensions.changed`. Unchanged extensions stay mounted untouched; changed
   extensions cleanly unmount, swap cache-busted assets, and remount in place. */
function applyShellExtensions(entries, changedIds = [], removedIds = []) {
  const incoming = new Map();
  for (const entry of Array.isArray(entries) ? entries : []) {
    if (!entry || !validShellExtensionId(entry.id)
        || !VALID_SHELL_EXTENSION_SLOTS.has(entry.slot)
        || typeof entry.script !== 'string' || !entry.script) {
      continue;
    }
    incoming.set(entry.id, entry);
  }

  const forcedChanges = new Set(Array.isArray(changedIds) ? changedIds : []);
  const explicitRemovals = new Set(Array.isArray(removedIds) ? removedIds : []);

  /* 1. Unmount & remove extensions no longer present. */
  for (const id of [...shellExtensionManifests.keys(), ...explicitRemovals]) {
    if (incoming.has(id)) continue;
    shellExtensionLoadGen.set(id, (shellExtensionLoadGen.get(id) ?? 0) + 1);
    unmountShellExtension(id);
    removeExtensionHeadAssets(id);
    shellExtensionManifests.delete(id);
    shellExtensionDefs.delete(id);
    shellExtensionStatus.delete(id);
  }

  /* 2. Surgically hot-reload only extensions that are new or changed. */
  for (const [id, manifest] of incoming) {
    const previous = shellExtensionManifests.get(id);
    const unchanged = !forcedChanges.has(id) && sameShellExtensionManifest(previous, manifest);
    if (unchanged) continue;

    const gen = (shellExtensionLoadGen.get(id) ?? 0) + 1;
    shellExtensionLoadGen.set(id, gen);

    unmountShellExtension(id);
    removeExtensionHeadAssets(id);
    shellExtensionDefs.delete(id);
    shellExtensionManifests.set(id, manifest);
    shellExtensionStatus.set(id, {
      status: 'loading',
      error: null,
      revision: manifest.revision,
    });

    loadAndMountShellExtension(manifest, gen);
  }

  syncExtensionOverlays();
}

/* Public extension runtime API exposed on `window.viewportExtensions`. */
const viewportExtensionsApi = Object.freeze({
  register(id, definition) {
    if (!validShellExtensionId(id)) {
      throw new TypeError(`viewportExtensions.register: invalid extension id ${JSON.stringify(id)}`);
    }
    if (!definition || typeof definition.mount !== 'function') {
      throw new TypeError(`viewportExtensions.register(${id}): definition must provide mount(container, ctx)`);
    }
    const script = document.currentScript;
    if (script?.dataset?.shellExtId) {
      if (script.dataset.shellExtId !== id
          || Number(script.dataset.shellExtGen) !== shellExtensionLoadGen.get(id)) {
        throw new Error(`stale or mismatched extension registration for ${id}`);
      }
    }
    shellExtensionDefs.set(id, Object.freeze({
      id,
      mount: definition.mount,
      unmount: typeof definition.unmount === 'function' ? definition.unmount : null,
    }));
  },

  list() {
    const items = [];
    for (const [id, manifest] of shellExtensionManifests) {
      const st = shellExtensionStatus.get(id);
      items.push(Object.freeze({
        id: manifest.id,
        name: manifest.name,
        slot: manifest.slot,
        revision: manifest.revision,
        version: manifest.version ?? null,
        description: manifest.description ?? null,
        status: st?.status ?? 'loading',
        error: st?.error ?? null,
      }));
    }
    return Object.freeze(items);
  },

  getState() {
    return buildShellExtensionState();
  },

  reload(id) {
    const msg = { type: 'extensions.reload' };
    if (typeof id === 'string' && id.trim() !== '') msg.id = id.trim();
    send(msg);
  },
});

if (typeof window !== 'undefined') {
  window.viewportExtensions = viewportExtensionsApi;
}
