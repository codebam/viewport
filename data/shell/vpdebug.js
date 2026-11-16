/* SPDX-License-Identifier: MIT
 *
 * Debug instrumentation for the geometry/animation path, off by default.
 *
 * Enabled by loading the shell with `?vpdebug=1` in the URL (see
 * ~/start-viewport.sh). Every line goes out through console.info, which the
 * CEF/WebKit backends forward to the compositor log, so a traced run leaves
 * the shell's side of the wire in /tmp/viewport.log next to the compositor's.
 *
 * What it answers, for a window that lands in the wrong place or not at all:
 * what the tree said (columns, widths, scroll), what the DOM measured, what
 * was actually sent over IPC, and when the geometry pump stopped sampling.
 * The heartbeat snapshots all three every two seconds so a bug caught on
 * screen can be read back out of the log without touching the session.
 *
 * One of the ordered scripts that make up the shell; loaded first so the
 * gate exists before any other script runs.
 */

/* On globalThis as well as window: in a browser they are the same object,
   and the test harness runs these scripts against a stub window that is
   not its global, where a bare vpdbg() call still has to resolve. */
globalThis.VPDBG = window.VPDBG = typeof location !== 'undefined'
  && new URLSearchParams(location.search).has('vpdebug');

globalThis.vpdbg = window.vpdbg = (tag, data) => {
  if (!globalThis.VPDBG) return;
  let line;
  try {
    line = JSON.stringify(data);
  } catch (e) {
    line = String(data);
  }
  console.info('[vpdbg] ' + tag + ' ' + line);
};

/* A rounded rect, or null for a detached element: the difference between
   "measured zero" and "not in the document" is exactly what a placement bug
   turns on. */
globalThis.vpdbgRect = window.vpdbgRect = (el) => {
  if (!el || !el.isConnected) return null;
  const r = el.getBoundingClientRect();
  return [Math.round(r.left), Math.round(r.top),
    Math.round(r.width), Math.round(r.height)];
};

if (window.VPDBG && typeof setInterval === 'function') {
  /* Late enough that every ordered script has run; the try/catch because a
     snapshot must never be the thing that breaks the desktop it watches. */
  setTimeout(() => setInterval(() => {
    try {
      vpdbg('beat', {
        t: Math.round(performance.now()),
        focused: focusedId,
        rendered: [...renderedIds],
        scrolls: [...scrollOffsets.entries()],
        outputs: [...outputs.values()].map((o) => ({
          name: o.name, ws: o.workspace, area: vpdbgRect(o.windowsEl),
        })),
        views: [...views.entries()].map(([id, v]) => ({
          id, ws: workspaceOf(id), hidden: v.el.hidden,
          float: v.floating ? [v.floating.x, v.floating.y,
            v.floating.width, v.floating.height] : null,
          /* What the compositor was last told, against where the hole is
             now: a divergence here is a pump that stopped too soon. */
          sent: v.box, clip: v.clip,
          rect: vpdbgRect(v.viewport),
          ent: v.entrance
            ? [Math.round(v.entrance.x), Math.round(v.entrance.y), v.entrance.scale]
            : null,
          tween: v.entranceTween === true,
        })),
      });
    } catch (e) {
      vpdbg('beat-error', { error: String(e) });
    }
  }, 2000), 1500);
}
