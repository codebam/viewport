/* SPDX-License-Identifier: MIT
 *
 * The canvas layout: every workspace an unbounded plane, panned and zoomed.
 *
 * The fifth layout model, beside the i3-style tree in tiling.js, niri's strip
 * in scrolling.js, the orbits in solar.js and the focus history in matrix.js.
 * Like the last two it computes rectangles rather than handing the problem to
 * flexbox, and for a reason none of the others have: there is no arrangement
 * of rows and columns that expresses "wherever you put it". A window on the
 * canvas has a position because someone gave it one, and the layout's whole
 * job is to keep that position while the view over it moves.
 *
 * The model, in full:
 *
 *   - A workspace is a plane with no edges. Windows sit on it at world
 *     coordinates and keep them: nothing an already-placed window does moves
 *     any other, and opening one never reflows what is open.
 *   - Each workspace has a viewport over its plane — an origin and a zoom —
 *     and since a workspace lives on one monitor at a time, that is one canvas
 *     per workspace per monitor, each panned where its user left it.
 *   - Panning moves the viewport, not the windows. Zooming out draws them
 *     smaller *without resizing them*: the client keeps the size it was
 *     configured with and the compositor scales the buffer, which is the same
 *     bargain the overview and solar's outer orbit make. See geometry.js.
 *   - What falls outside the viewport is not drawn and is reported to the
 *     compositor as off screen. That is what keeps a plane with fifty windows
 *     on it costing the same as one with four.
 *
 * Zoom is capped at 1.0, and the cap is about what is *above* it rather than
 * what is below. A buffer drawn larger than it was painted is a blurry one, and
 * the alternative — reconfiguring every client on every zoom step — is exactly
 * the per-frame resize storm this layout exists to avoid.
 *
 * Below 1.0 the canvas is fully usable, including the pointer. That was not
 * true when this layout was written: `surface_under` in the compositor
 * hit-tested a click against the window's mapped rectangle with no scale term
 * in it, so a click on a zoomed-out window landed further into the client the
 * further across it you clicked, and dragging one moved it from a grip that was
 * not where the hand was. `ViewportState::unscaled` (state.rs) now takes a
 * screen position back into the window's own coordinates about the same corner
 * the renderer scales it about, and the three hit tests — surface_under,
 * window_under, clipped_out — all go through it.
 *
 * The tiling tree still says which windows exist and which workspace they are
 * on; this reads that and never writes it, which is what lets window.move, the
 * session format and the overview keep working without knowing the canvas
 * exists. It is the same bargain solar.js and matrix.js make, and the reason
 * all three are additions rather than rewrites.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* Every number the layout depends on. Read at use rather than captured, so
 * editing one and reloading the shell takes effect without a restart. */
const CANVAS = {
  /* How far out the view may go. Far enough that a plane holding a dozen
     windows fits on one screen, near enough that what is on it is still
     readable — past this a window is a coloured rectangle and fit-to-all is
     showing you nothing you could act on. */
  minZoom: 0.25,

  /* And how far in. One, exactly: past it the compositor is enlarging a buffer
     the client painted smaller, which is blur, and the way to avoid the blur is
     to reconfigure every client on every step — the resize storm this layout
     exists to avoid. Zooming *out* has no such problem and is not capped here;
     see the header. */
  maxZoom: 1,

  /* Multiplicative, so stepping out and back in returns to where it started
     rather than drifting — additive steps do not compose. */
  zoomStep: 1.25,

  /* How far one pan key moves the view, in *screen* pixels. Converted to world
     units against the current zoom at use, so a keypress moves the same
     visible distance however far out the view is — which is what the hand
     expects, and what a fixed world step conspicuously does not do. */
  panStep: 240,

  /* And how far one move key carries a window across the plane. In world
     units, unlike the pan: moving a window is an edit to the plane, so the
     same key should make the same edit whatever the view happens to be doing.
     A screen-relative step would move a window four times further when zoomed
     out, which is the same key meaning two things. */
  moveStep: 120,

  /* A new window's size, as a fraction of the visible area. Big enough to work
     in, small enough that a second one beside it is not immediately in the
     way. */
  width: 0.45,
  height: 0.55,

  /* How far each successive new window is offset from the last, so that
     opening three terminals without moving anything leaves three terminals you
     can tell apart rather than one with two behind it. The stagger a window
     manager has used since 1985, for the same reason. */
  cascade: 48,

  /* Space kept around the plane when fitting all of it on screen, in screen
     pixels. A fit with no margin puts the outermost windows flush against the
     edge, with nothing to show that the plane ends there. */
  margin: 48,

  /* The gap when following focus is not here: it is the configured gap, read
     at the time of the pan by canvasFollowMargin(). */

  /* How small a drag may make a window, in world units. A resize that can
     reach zero leaves a rectangle too small to take hold of and no way to grow
     it again. */
  minSize: 120,
};

/* ------------------------------------------------------------------------
 * The plane
 *
 * Two pieces of state, both kept outside the window records because both
 * outlive them: where each window sits on its plane, and where each plane is
 * being looked at from.
 *
 * Each plane's coordinates are its own. Nothing relates workspace 3's origin to
 * workspace 7's, so a window carried between them cannot keep its numbers —
 * see canvasCarry, which is what happens instead.
 * --------------------------------------------------------------------- */

/* view id -> { x, y, width, height } in world units. Width and height are the
 * *client's* size and never the drawn one: zoom scales what is painted, not
 * what the window is, and mixing the two here is what would make zooming out
 * resize every client on the workspace. */
const canvasPlaces = new Map();

/* workspace -> { x, y, zoom }. x and y are the world coordinate at the top
 * left of the visible area, so panning is one subtraction and nothing has to
 * know where the middle of anything is. */
const canvasViewports = new Map();

/* The viewport for a workspace, created at the origin if it has none. Every
 * reader goes through this so that "never looked at yet" and "looked at and
 * panned back to zero" are the same state, which they are. */
function canvasViewportOf(workspace) {
  let viewport = canvasViewports.get(workspace);
  if (!viewport) {
    viewport = { x: 0, y: 0, zoom: 1 };
    canvasViewports.set(workspace, viewport);
  }
  return viewport;
}

/* ------------------------------------------------------------------------
 * Surviving a reload
 *
 * Both maps above are page state, and a reload is a new page: `location.reload`
 * throws away every place and every viewport, and the compositor then replays
 * `view.added` for every window that was open. Without something to put back,
 * that is a desktop swept into a cascade in the middle of the screen — the
 * plane is *where the windows are*, so losing it loses the layout entirely,
 * which is a worse outcome than any other layout suffers from a reload. The
 * tiling tree comes back because the session saves it; this is the same
 * mechanism for the same reason.
 *
 * Keyed by application rather than by view id, exactly as the saved floating
 * rects are. A reload keeps the ids and a restart does not, and one of these
 * has to work across both.
 * --------------------------------------------------------------------- */

/* Places from the saved session, waiting for their windows to come back. */
let canvasSlots = [];

/* The plane, as the session file holds it. Places carry the workspace they
 * were on so that a window returning to a different one is not given a rect
 * from a plane it is no longer part of; viewports are per workspace already. */
function serialiseCanvas() {
  const places = [];
  for (const [id, rect] of canvasPlaces) {
    const view = views.get(id);
    const app = view?.app_id || view?.title;
    const workspace = workspaceOf(id);
    if (!app || workspace === null) continue;
    places.push({ app, workspace,
      x: rect.x, y: rect.y, width: rect.width, height: rect.height });
  }

  const viewports = {};
  for (const [workspace, viewport] of canvasViewports) {
    viewports[workspace] = { x: viewport.x, y: viewport.y, zoom: viewport.zoom };
  }
  return { places, viewports };
}

/* Put a saved plane back. The viewports go straight in — nothing has to claim
 * them, since a workspace exists whether or not anything is on it — and the
 * places wait to be claimed by the windows as they are replayed. */
function restoreCanvas(saved) {
  canvasSlots = Array.isArray(saved?.places)
    ? saved.places.filter((slot) => slot && slot.app
      && Number.isFinite(slot.x) && Number.isFinite(slot.y)
      && slot.width > 0 && slot.height > 0)
    : [];

  for (const [workspace, viewport] of Object.entries(saved?.viewports ?? {})) {
    if (!Number.isFinite(viewport?.x) || !Number.isFinite(viewport?.y)) continue;
    canvasViewports.set(Number(workspace), {
      x: viewport.x, y: viewport.y, zoom: canvasClampZoom(viewport.zoom),
    });
  }
}

/* The place this application left behind, if it left one.
 *
 * The plane it was on first: a place is a coordinate on one particular plane,
 * and handing a window arriving on workspace 3 the rect it had on workspace 1
 * is putting it somewhere nobody chose. Failing that, the application's place
 * on any plane, which is the case where a window came back somewhere else —
 * the rect is still the size and the corner it had, which beats the middle of
 * the screen.
 *
 * Removed as it is taken, so two windows of the same application claim two
 * different places rather than both landing on the first. The same splice
 * `claimFloatSlot` does, for the same reason. */
function claimCanvasSlot(app, workspace) {
  if (canvasSlots.length === 0 || !app) return null;

  let at = canvasSlots.findIndex(
    (slot) => slot.app === app && slot.workspace === workspace);
  if (at < 0) at = canvasSlots.findIndex((slot) => slot.app === app);
  if (at < 0) return null;

  const [slot] = canvasSlots.splice(at, 1);
  return slot;
}

/* Give up on places nothing came back for, on the same timeout as the tree's
 * unclaimed slots. Without this a window opened an hour later would be handed
 * the rect of something that closed during the restart. */
function dropCanvasSlots() {
  canvasSlots = [];
}

/* This window has been put where it is by a person rather than by the layout.
 *
 * The one thing that outranks a placement rule: a window someone has dragged
 * stays dragged. Kept on the view record rather than in a set of its own, so it
 * goes when the window does. */
function markCanvasMoved(id) {
  const view = views.get(id);
  if (view) view.canvasMoved = true;
}

/* The client is a different size from the one it was asked to be.
 *
 * A place is the size the client is *asked* to be, and asking is not the same
 * as getting: a client configured below its minimum may ignore it, so the
 * compositor raises the configure to that minimum instead of sending a request
 * it knows will be refused. The shell was never told, so it went on holding a
 * rectangle for a window that is a different size — and every sum built on that
 * rectangle was wrong by the difference. Centring a dialog on its parent is one
 * such sum, and came out half the difference off.
 *
 * The shell's own idea of the minimum comes from `view.added` and can go stale:
 * a client may raise its minimum long after it mapped, and Chrome does. So this
 * takes the size as fact rather than trying to predict it, and updates the
 * minimum to match — the place has just proved the old one wrong.
 *
 * One round trip and it settles: the next relayout asks for the size the client
 * already has, the compositor's clamp does nothing, and nothing more is sent. */
function canvasConfigured(id, width, height) {
  if (layoutMode !== 'canvas') return false;
  if (!(width > 0) || !(height > 0)) return false;
  const rect = canvasPlaces.get(id);
  if (!rect) return false;
  if (rect.width === width && rect.height === height) return false;

  /* Which axis was raised, worked out before the place is overwritten. The
     compositor raises an axis to the client's minimum and leaves the other
     alone, so an axis that came back larger has just named that minimum and an
     axis that came back unchanged has said nothing at all. Adopting both would
     take whatever the layout happened to ask for on the quiet axis and write it
     down as a floor the client never claimed. */
  const raisedWidth = width > rect.width;
  const raisedHeight = height > rect.height;

  rect.width = width;
  rect.height = height;

  /* Without this the next resize drag clamps against the stale number the
     client sent when it opened, asks for that, gets clamped again, and the two
     are back out of step. */
  const view = views.get(id);
  if (view) {
    if (raisedWidth) view.minWidth = width;
    if (raisedHeight) view.minHeight = height;
  }
  return true;
}

/* This window has just been told whose dialog it is.
 *
 * A dialog an application opens itself arrives with its parent named, and is
 * placed on it. One that comes from somewhere else does not: a file chooser is
 * the portal's window, in another process, and the parent reaches the
 * compositor over xdg-foreign long after the window has mapped — by which time
 * the canvas has already given it a place in the middle of the view, attached
 * to nothing.
 *
 * So the place is thrown away and made again, now that there is something to
 * make it from. Only a place the *layout* chose: a window that has been dragged
 * has been put somewhere by a person, and a late message from a client is not a
 * reason to move it. Nothing else does this, because nothing else places a
 * window relative to another one.
 *
 * The relayout is the caller's — this runs from the message handler, which ends
 * in one anyway. */
function canvasReparented(id) {
  if (layoutMode !== 'canvas' || id == null) return false;
  const view = views.get(id);
  if (!view || view.parent == null) return false;
  if (view.canvasMoved) return false;
  if (!canvasPlaces.has(id)) return false;
  if (!canvasPlaces.has(view.parent)) return false;

  canvasPlaces.delete(id);
  return true;
}

/* A window sent to another workspace, and where it lands on the plane it
 * arrives at.
 *
 * Its coordinates cannot come with it. Each plane's origin is its own and
 * nothing relates one to another, so the same numbers on a different plane
 * name a different place — and the two failures that produces are both worse
 * than they sound. Left where they were, the window arrives wherever that
 * happens to be on the new plane: with any distance panned on either side,
 * that is somewhere off the screen, and sending a window to a workspace makes
 * it vanish. And because focus follows a window into view, going to that
 * workspace then drags the whole plane across to find it — so the windows that
 * were already there are the ones that disappear instead. Sending one window
 * away moved everything.
 *
 * What comes across is its position *on the screen*: whatever offset it had
 * from the corner of the view it had, it has from the corner of the view it
 * arrives in. It lands where it looked like it was, nothing else on either
 * plane moves, and following it is a no-op because it is already in sight.
 *
 * Through the zooms, because the two planes need not be looked at from the
 * same distance: the offset that is preserved is the one in screen pixels, not
 * the one in world units. */
function canvasCarry(id, from, to) {
  if (layoutMode !== 'canvas') return;
  if (from === null || to === null || from === to) return;

  const place = canvasPlaces.get(id);
  if (!place) return;

  const source = canvasViewportOf(from);
  const target = canvasViewportOf(to);
  place.x = target.x + (place.x - source.x) * source.zoom / target.zoom;
  place.y = target.y + (place.y - source.y) * source.zoom / target.zoom;
}

/* Drop a closed window's place. Called from removeView beside solarForget and
 * matrixClosed, for the same reason both of those are: this is state about a
 * window kept outside the window's own record, so it has to be forgotten by
 * hand. A stale entry here would hold a rectangle on the plane for a window
 * that no longer exists — invisible, since nothing draws it, and wrong the
 * moment fit-to-all measured the bounding box it is part of. */
function canvasClosed(id) {
  canvasPlaces.delete(id);
}

/* ------------------------------------------------------------------------
 * The layout itself
 *
 * Pure functions of (places, viewport, area) — no DOM, no globals, nothing
 * measured. That is what lets tests/shell.test.js check the arithmetic
 * directly rather than through a stubbed getBoundingClientRect that returns
 * fixed numbers and proves nothing about it.
 * --------------------------------------------------------------------- */

/* Zoom, held inside what the compositor can actually draw a click into. */
function canvasClampZoom(zoom) {
  if (!Number.isFinite(zoom)) return 1;
  return Math.min(CANVAS.maxZoom, Math.max(CANVAS.minZoom, zoom));
}

/* Where a world rectangle lands on the screen, and whether it lands there at
 * all.
 *
 * `area` is the region windows may be drawn in, in coordinates relative to the
 * output's `.windows` element — the same space matrixAreaOf works in, and the
 * space the inline `left`/`top` this produces are resolved against.
 *
 * `width` and `height` come back as the *client's* size with `scale` beside
 * them, exactly as solar's placements do: the element is drawn at
 * width * scale and geometry.js divides the scale back out to tell the
 * compositor what size the client should be. A window at zoom 0.5 is drawn
 * half-size and is still, to the client, the size it always was. */
function canvasPlacement(id, rect, viewport, area) {
  const zoom = viewport.zoom;
  const x = area.x + (rect.x - viewport.x) * zoom;
  const y = area.y + (rect.y - viewport.y) * zoom;
  const width = rect.width * zoom;
  const height = rect.height * zoom;

  /* Off screen is not "past the edge" but "shares no pixel with the area":
     a window hanging half over the right edge is on screen and is clipped by
     reportGeometry, and one entirely past it is not drawn at all. The two
     cases are different — the first is a surface the compositor crops, the
     second is a surface it stops painting — and the difference is this
     intersection. */
  const offscreen = x + width <= area.x || x >= area.x + area.width
    || y + height <= area.y || y >= area.y + area.height;

  return {
    id,
    x: Math.round(x),
    y: Math.round(y),
    width: Math.max(1, Math.round(rect.width)),
    height: Math.max(1, Math.round(rect.height)),
    scale: zoom,
    offscreen,
  };
}

/* The layout, for one plane's worth of windows.
 *
 * `items` is [{ id, rect }] in world coordinates. Deterministic and total: the
 * same three arguments always give the same placements, and every window in
 * comes back out with one, including the ones that end up off screen — they
 * carry `offscreen` rather than being dropped, so the caller can decide
 * whether "not drawn" also means "not reported", which it does. O(N). */
function canvasProject(items, viewport, area) {
  const out = [];
  if (!Array.isArray(items) || items.length === 0) return out;
  if (!area || !(area.width > 0) || !(area.height > 0)) return out;

  for (const item of items) {
    if (!item || !item.rect) continue;
    out.push(canvasPlacement(item.id, item.rect, viewport, area));
  }
  return out;
}

/* The bounding box of everything on the plane, in world units. Null when there
 * is nothing on it, which is a different answer from a zero-sized box and is
 * why the callers check for it rather than for an empty width. */
function canvasBounds(items) {
  let box = null;
  for (const item of items) {
    const rect = item?.rect;
    if (!rect) continue;
    if (!box) {
      box = { left: rect.x, top: rect.y,
        right: rect.x + rect.width, bottom: rect.y + rect.height };
      continue;
    }
    box.left = Math.min(box.left, rect.x);
    box.top = Math.min(box.top, rect.y);
    box.right = Math.max(box.right, rect.x + rect.width);
    box.bottom = Math.max(box.bottom, rect.y + rect.height);
  }
  return box;
}

/* A viewport showing everything at once, centred.
 *
 * The zoom is whichever of the two axes is the binding one, clamped — so a
 * plane that already fits is left at 1.0 rather than being blown up past it,
 * and one spread far too wide stops at minZoom and is fitted as well as it can
 * be rather than as well as it was asked to be. Centring after the clamp is
 * what makes the second case land on the middle of the plane instead of its
 * top left corner. */
function canvasFitViewport(items, area, margin = CANVAS.margin) {
  const box = canvasBounds(items);
  if (!box || !area || !(area.width > 0) || !(area.height > 0)) {
    return { x: 0, y: 0, zoom: 1 };
  }

  const width = Math.max(1, box.right - box.left);
  const height = Math.max(1, box.bottom - box.top);
  const zoom = canvasClampZoom(Math.min(
    (area.width - margin * 2) / width,
    (area.height - margin * 2) / height));

  /* The visible span in world units, which is what the view has to be centred
     against — the area is screen pixels and the plane is not. */
  const span = { x: area.width / zoom, y: area.height / zoom };
  return {
    x: (box.left + box.right) / 2 - span.x / 2,
    y: (box.top + box.bottom) / 2 - span.y / 2,
    zoom,
  };
}

/* The gap left between a followed window and the edge it came in from, in
 * screen pixels and measured from the edge of the *area* rather than the edge
 * of the screen.
 *
 * Which is why it is usually zero. The area is already the output inset by
 * edgeGapPx, and the projection puts that inset in front of every place, so a
 * window panned flush against the area is drawn exactly where the gaps begin —
 * the same edge canvasFillFocused sizes to. A margin on top of that is a second
 * gap: 15 configured showed up as 30 on screen, which is close enough to the
 * bar's own height to read as the view ducking a bar that was not there.
 *
 * It is nonzero in the one case where the two disagree. Smart gaps drop the
 * inner gap for a workspace holding one window, so the area of a plane holding
 * one window runs to the bare edge of the monitor; the difference is made up
 * here, and following focus leaves the configured gap on a plane whatever
 * smart gaps have done to the area. */
function canvasFollowMargin(area) {
  const gap = gapPx() + gapOuterPx();
  return Math.max(0, gap - (area?.x ?? gap));
}

/* The smallest pan that brings a world rectangle fully on screen.
 *
 * Minimal on purpose: following focus by centring the focused window would
 * move the whole plane on every focus change, including the ones where the
 * window you moved to was already in plain sight. Nothing moves unless
 * something is off the edge, and then only far enough.
 *
 * Zoom is returned unchanged. Following focus is not a reason to change how
 * far out the view is — that is a decision the user made and this has no
 * business undoing it. */
function canvasFollow(rect, viewport, area, margin = null) {
  if (!rect || !area || !(area.width > 0) || !(area.height > 0)) return viewport;

  const zoom = viewport.zoom;
  const span = { x: area.width / zoom, y: area.height / zoom };
  /* The margin is screen pixels, so it is worth more world units the further
     out the view is — which is right: it is a gap you can see, not a gap on
     the plane. Defaulted here rather than in the signature because it is a
     question about the area, which the signature cannot ask. */
  const edge = (margin ?? canvasFollowMargin(area)) / zoom;

  let { x, y } = viewport;

  /* Right and bottom first, then left and top, so that a window larger than
     the screen ends up showing its top left corner rather than its bottom
     right. Both branches fire for it and the second wins, which is the
     ordinary convention: the part of an oversized window you want is the part
     that starts. */
  if (rect.x + rect.width + edge > x + span.x) {
    x = rect.x + rect.width + edge - span.x;
  }
  if (rect.x - edge < x) x = rect.x - edge;

  if (rect.y + rect.height + edge > y + span.y) {
    y = rect.y + rect.height + edge - span.y;
  }
  if (rect.y - edge < y) y = rect.y - edge;

  return { x, y, zoom };
}

/* Zoom about the middle of the screen.
 *
 * The world point under the centre of the area stays under it, which is what
 * makes stepping out and back in return to the same view — anchoring at the
 * viewport origin instead would walk the plane sideways a little on every
 * step, and the drift is not subtle after four of them. */
function canvasZoomed(viewport, factor, area) {
  const zoom = canvasClampZoom(viewport.zoom * factor);
  if (!area || !(area.width > 0) || !(area.height > 0)) {
    return { x: viewport.x, y: viewport.y, zoom };
  }
  const anchor = {
    x: viewport.x + (area.width / 2) / viewport.zoom,
    y: viewport.y + (area.height / 2) / viewport.zoom,
  };
  return {
    x: anchor.x - (area.width / 2) / zoom,
    y: anchor.y - (area.height / 2) / zoom,
    zoom,
  };
}

/* ------------------------------------------------------------------------
 * Reading the shell's state
 * --------------------------------------------------------------------- */

/* The area of an output windows may be placed in.
 *
 * Measured rather than derived from the output's mode, and inset by the edge
 * gap by hand, for the reasons written out over matrixAreaOf: the bar and a
 * panel's exclusive zone are already out of `.windows`, but its padding is not
 * — an absolutely positioned child is laid out against the padding box, so a
 * field at inset:0 covers the padding rather than sitting inside it. */
function canvasAreaOf(output) {
  const rect = output?.windowsEl?.getBoundingClientRect();
  if (!rect) return null;
  const edge = edgeGapPx(output?.workspace);
  const width = rect.width - edge * 2;
  const height = rect.height - edge * 2;
  if (width <= 0 || height <= 0) return null;
  return { x: edge, y: edge, width, height };
}

/* The windows the canvas places on a workspace, in tree order.
 *
 * Floating windows included, which is where this parts company with solar and
 * the matrix. Both of those leave them out because a dialog is floating exactly
 * so that it will *not* be tiled, and handing one a slot is that decision made
 * twice. There is no such argument here: a plane is not a division of space,
 * and every window on one is placed by hand at a rectangle of its own. Floating
 * is what they all are.
 *
 * Leaving them out is not neutral, either — it is a window nailed to the
 * screen. relayoutAll writes a floating window's own rect straight onto the
 * element, in screen coordinates, so one left out of the plane sits still while
 * everything around it pans, and no amount of panning or zooming will move it.
 * Which windows the compositor decides to float is not something the person
 * looking at the screen can see (views.rs, wants_floating, plus any rule in the
 * config file), so the fault arrives as "this one window ignores me".
 *
 * Tree order and not focus order, unlike the matrix: nothing about where a
 * window sits on the plane depends on when it was last used, and an order that
 * changed under focus would make the cascade of newly-opened windows shuffle
 * every time you clicked one. */
/* Two sources, because a floating window is frequently not in the tree at all:
 * some paths insert a leaf and then mark it floating, and some never insert one
 * — `idsOf` reads both for the same reason. Walking only the tree left those
 * windows with no place, which is not a window in the wrong spot but a window
 * the layout has never heard of. Deduplicated, since the first kind is in both.
 */
function canvasOrderOf(workspace) {
  const root = workspaces.get(workspace);
  const ids = [];
  const seen = new Set();

  if (root) {
    for (const id of dynamicOrder(root)) {
      if (!views.has(id) || seen.has(id)) continue;
      seen.add(id);
      ids.push(id);
    }
  }
  for (const [id, floating] of floatingEntries()) {
    if (floating.workspace !== workspace) continue;
    if (!views.has(id) || seen.has(id)) continue;
    seen.add(id);
    ids.push(id);
  }
  return ids;
}

/* Where a window is on the plane, placing it if this is the first time anyone
 * has asked.
 *
 * Two ways of placing one, and the first is the interesting one. A window that
 * was on screen a moment ago — because the layout was tiling until the user
 * pressed the key that got us here — has a `view.box`, which is exactly where
 * it was last drawn. Adopting that freezes the layout you were looking at onto
 * the plane, so switching into the canvas leaves the desktop looking identical
 * and merely makes it draggable. Seeding at an origin instead would collapse a
 * working tiling layout into a pile at the top left, which is a layout switch
 * that loses your arrangement.
 *
 * The box is in page coordinates and the plane is not, so it comes back
 * through the same projection the renderer applies, inverted: subtract the
 * output's own origin to get field coordinates, then the area's inset and the
 * viewport's, dividing out the zoom.
 *
 * Before either, the place this application left behind on the plane last
 * time, if the session had one. That is what makes a reload survivable: the
 * page comes back with both maps empty and every window replayed, so without
 * it a reload is a desktop swept into a pile in the middle of the screen. See
 * claimCanvasSlot.
 *
 * Failing all of that — a window opened while the canvas was already running,
 * a workspace nothing has drawn yet — the window is put at the middle of the
 * visible area at the default size, cascaded past whatever is already sitting
 * there. */
function canvasPlaceOf(id, workspace, output, viewport, area) {
  const existing = canvasPlaces.get(id);
  if (existing) return existing;

  /* Every way of arriving at a place goes out through here, so that none of
     them can hand a client a size it will refuse. A place is the size the
     client is asked to be: below its own minimum the client keeps the size it
     had, the frame does not, and the two disagree about where everything in
     the window is. */
  const keep = (place) => {
    const record = views.get(id);
    place.width = Math.max(
      CANVAS.minSize, record?.minWidth ?? 0, place.width);
    place.height = Math.max(
      CANVAS.minSize, record?.minHeight ?? 0, place.height);
    canvasPlaces.set(id, place);
    return place;
  };

  const zoom = viewport.zoom;
  const view = views.get(id);

  const slot = claimCanvasSlot(view?.app_id || view?.title, workspace);
  if (slot) {
    const place = {
      x: slot.x, y: slot.y, width: slot.width, height: slot.height,
    };
    return keep(place);
  }

  /* A dialog opens on top of the window it belongs to, centred.
   *
   * Before the rectangle it arrived with, which is the point. That rectangle
   * was chosen to centre the dialog *on the screen*, which is the right answer
   * in every layout where the window it belongs to is also on the screen. Here
   * the parent can be anywhere on a plane with no edges, and frequently is:
   * the dialog then opens in the middle of the view, unattached to anything
   * visible, while the window that raised it is somewhere off to one side. It
   * is the one placement question a canvas has that a screen-sized layout
   * never had to ask.
   *
   * Only when the parent has a place of its own — a dialog whose parent the
   * compositor could not name, or one whose parent is not on this plane, falls
   * through to the rectangle it came with. */
  const parent = view?.parent != null ? canvasPlaces.get(view.parent) : null;
  if (parent) {
    const size = floatingOf(id) ?? { width: 0, height: 0 };
    const width = Math.max(1, size.width || Math.round(parent.width / 2));
    const height = Math.max(1, size.height || Math.round(parent.height / 2));
    return keep({
      x: Math.round(parent.x + (parent.width - width) / 2),
      y: Math.round(parent.y + (parent.height - height) / 2),
      width,
      height,
    });
  }

  /* A floating window arrives with a rectangle already — from the compositor,
     or from a rule in the config file that says where this application opens.
     That rectangle is in the same coordinates the plane is drawn in, so it
     becomes the window's first place directly: a dialog told to open at 10,20
     opens at 10,20 and then pans with everything else, rather than being put in
     the middle of the screen because the canvas had an opinion of its own. Read
     before `view.box`, which for a window that has never been drawn is not
     there at all. */
  const floating = floatingOf(id);
  if (floating) {
    const place = {
      x: viewport.x + (floating.x - area.x) / zoom,
      y: viewport.y + (floating.y - area.y) / zoom,
      width: floating.width,
      height: floating.height,
    };
    return keep(place);
  }

  const box = view?.box;
  const host = output?.windowsEl?.getBoundingClientRect();

  if (box && host && box.width > 0 && box.height > 0) {
    const place = {
      x: viewport.x + (box.x - host.left - area.x) / zoom,
      y: viewport.y + (box.y - host.top - area.y) / zoom,
      width: box.width,
      height: box.height,
    };
    return keep(place);
  }

  const width = Math.round(area.width * CANVAS.width / zoom);
  const height = Math.round(area.height * CANVAS.height / zoom);
  /* The middle of what is being looked at, not the middle of the plane: a new
     window belongs where the user is, and the plane has no middle. */
  const centre = {
    x: viewport.x + (area.width / 2) / zoom,
    y: viewport.y + (area.height / 2) / zoom,
  };

  let x = Math.round(centre.x - width / 2);
  let y = Math.round(centre.y - height / 2);

  /* Step off anything already sitting here, on *this* plane.
   *
   * Two things this gets wrong if written the obvious way. Comparing against
   * every place in the map rather than the ones on this workspace makes an
   * empty second monitor cascade its first window past windows it will never
   * share a screen with: open four on the left-hand screen and the first one
   * on the right arrives four steps down and to the right of the middle, for
   * no reason anyone looking at that screen can see. Planes are independent,
   * so the comparison is too.
   *
   * And comparing origins for equality catches almost nothing: two windows a
   * single pixel apart are, to look at, one window with another hidden behind
   * it, and any arithmetic that lands near but not on an existing origin
   * defeats an exact test. The test is therefore whether the new origin is
   * within one cascade step of an existing one, which is the distance at which
   * the stagger stops being visible.
   *
   * Bounded by the number of windows on the plane: each step moves a fixed
   * distance in a fixed direction, so it cannot revisit a spot it has left. */
  const here = [];
  for (const [other, rect] of canvasPlaces) {
    if (other !== id && workspaceOf(other) === workspace) here.push(rect);
  }
  const crowded = () => here.some((rect) =>
    Math.abs(rect.x - x) < CANVAS.cascade
    && Math.abs(rect.y - y) < CANVAS.cascade);
  for (let step = 0; step < here.length + 1 && crowded(); step++) {
    x += CANVAS.cascade;
    y += CANVAS.cascade;
  }

  return keep({ x, y, width, height });
}

/* One workspace's windows and where they are, ready to project. */
function canvasItemsOf(workspace, output, viewport, area) {
  return canvasOrderOf(workspace).map((id) => ({
    id, rect: canvasPlaceOf(id, workspace, output, viewport, area),
  }));
}

/* Every output's placements, worked out on the way into a relayout. One pass
 * for all of them, so the caller has the whole plan before it draws any of it
 * — the same shape planSolar and planMatrix have, and for the same reason. */
function planCanvas() {
  const plan = new Map();
  for (const [name, output] of outputs) {
    const area = canvasAreaOf(output);
    if (!area) {
      plan.set(name, []);
      continue;
    }
    const viewport = canvasViewportOf(output.workspace);
    const items = canvasItemsOf(output.workspace, output, viewport, area);
    plan.set(name, canvasProject(items, viewport, area));
  }
  return plan;
}

/* The plan for one workspace, on the output showing it. The entry point the
 * tests and the commands below ask for by name, so neither has to know how a
 * workspace finds its monitor. */
function recalculateCanvasLayout(workspace, areaGiven = null) {
  const output = outputs.get(hostOfWorkspace(workspace));
  const area = areaGiven ?? canvasAreaOf(output);
  if (!area) return [];
  const viewport = canvasViewportOf(workspace);
  return canvasProject(
    canvasItemsOf(workspace, output, viewport, area), viewport, area);
}

/* ------------------------------------------------------------------------
 * Driving the view
 *
 * Everything here edits one of the two maps and asks for a relayout. None of
 * it touches the tree, and none of it sends anything to the compositor: what
 * the compositor learns about a pan is that some windows have new rectangles,
 * which is what it learns about every other layout change too.
 * --------------------------------------------------------------------- */

/* The workspace being looked at, its output and its area, or null when the
 * canvas is not running or there is nothing to run it on. Every command starts
 * with this, so a chord left over in someone's config file does nothing rather
 * than throwing. */
function canvasTarget() {
  if (layoutMode !== 'canvas') return null;
  const output = outputs.get(activeOutputName());
  if (!output) return null;
  const area = canvasAreaOf(output);
  if (!area) return null;
  return { output, area, workspace: output.workspace };
}

/* Pan by a screen distance. The step is divided by the zoom so that a key
 * moves the view the same visible amount however far out it is. */
function canvasPan(dx, dy, byHand = false) {
  const target = canvasTarget();
  if (!target) return;
  const viewport = canvasViewportOf(target.workspace);
  viewport.x += dx / viewport.zoom;
  viewport.y += dy / viewport.zoom;
  /* A pan key is one step and animates; a pan drag is a stream of deltas and
     must not. See gestureRelayout in geometry.js. */
  if (byHand) gestureRelayout();
  else relayoutAll();
}

/* One pan key, as a direction. Panning right means looking further right,
 * which moves the windows left — the map convention, not the scrollbar one. */
function canvasPanDirection(direction) {
  const step = CANVAS.panStep;
  canvasPan(
    direction === 'left' ? -step : direction === 'right' ? step : 0,
    direction === 'up' ? -step : direction === 'down' ? step : 0);
}

/* Zoom in or out a step, or to an exact factor. */
function canvasZoom(arg) {
  const target = canvasTarget();
  if (!target) return;
  const viewport = canvasViewportOf(target.workspace);

  const exact = Number(arg);
  const next = Number.isFinite(exact) && exact > 0
    ? canvasZoomed(viewport, exact / viewport.zoom, target.area)
    : canvasZoomed(viewport,
      arg === 'out' ? 1 / CANVAS.zoomStep : CANVAS.zoomStep, target.area);

  canvasViewports.set(target.workspace, next);
  relayoutAll();
}

/* Show the whole plane. */
function canvasFit() {
  const target = canvasTarget();
  if (!target) return;
  const viewport = canvasViewportOf(target.workspace);
  const items = canvasItemsOf(
    target.workspace, target.output, viewport, target.area);
  canvasViewports.set(target.workspace,
    canvasFitViewport(items, target.area));
  relayoutAll();
}

/* Back to 1:1, on whatever is focused.
 *
 * The counterpart to fit, and the one that matters most: 1.0 is the only zoom
 * at which clicking into a window works, so this is "let me use this again"
 * and wants a key of its own rather than four presses of zoom-in. */
function canvasHome() {
  const target = canvasTarget();
  if (!target) return;
  const viewport = canvasViewportOf(target.workspace);
  /* About the middle of the screen, like every other zoom here: anchoring at
     the viewport origin instead shrinks the span towards the top left, so
     coming home from 0.25 with nothing focused throws what was in the middle
     of the view off the screen. */
  const at = canvasZoomed(viewport, 1 / viewport.zoom, target.area);

  const rect = focusedId != null && workspaceOf(focusedId) === target.workspace
    ? canvasPlaces.get(focusedId) : null;
  canvasViewports.set(target.workspace,
    rect ? canvasFollow(rect, at, target.area) : at);
  relayoutAll();
}

/* Size the focused window to the screen, without making it fullscreen.
 *
 * The plane has no maximised state and no edge to maximise against, so this is
 * a resize like any other: the window is given the rectangle the view is
 * currently showing. It stays an ordinary window on the plane — the frame is
 * still on it, the ones behind it are still there, and panning away from it
 * leaves it the size it was. Fullscreen proper (`window.fullscreen`) takes the
 * output and hides the rest, which is a different thing to want.
 *
 * The rectangle is the screen inset by the configured gap, so the window stops
 * where the gaps begin — the edge a tiled window would have had. Measured
 * against the output rather than taken from `area`, because the two disagree
 * under smart gaps: `edgeGapPx` drops the inner gap for a workspace holding one
 * window, and a plane holding one window would then fill to the bare edge of
 * the monitor. Smart gaps are about a tiled workspace with nothing to divide,
 * which is not a statement about a plane.
 *
 * Divided by the zoom because a place is in world units: at 0.5 the window has
 * to be twice the screen's width on the plane to cover the screen. That also
 * makes this the reliable way to fill the screen at any zoom, rather than
 * something that only lands at 1.0.
 *
 * The client's own minimum is not consulted, unlike canvasResizeBy: this only
 * ever grows a window to the screen, and a client whose minimum is larger than
 * the screen is already refusing every size the canvas can offer it. */
function canvasFillFocused() {
  const target = canvasTarget();
  if (!target || focusedId == null) return false;
  if (workspaceOf(focusedId) !== target.workspace) return false;
  const rect = canvasPlaces.get(focusedId);
  if (!rect) return false;

  const viewport = canvasViewportOf(target.workspace);
  const box = target.output?.windowsEl?.getBoundingClientRect();
  /* The inset from the area, which is what canvasFollow leaves too: zero
     normally, because the area is already inset by the gap, and the difference
     under smart gaps. Naming it the same way in both places is the point — a
     followed window and a filled one stop at the same edge, and a fill that
     worked out its own answer is how the two came to disagree.

     Falling back to `area` where there is nothing to measure, which is the
     fallback canvasAreaOf's callers get and what the test harness runs on. */
  const margin = canvasFollowMargin(target.area);
  const inset = target.area.x + margin;
  const width = box ? box.width - inset * 2 : target.area.width;
  const height = box ? box.height - inset * 2 : target.area.height;
  if (!(width > 0) || !(height > 0)) return false;

  /* The place is in world units and the projection puts `area.x` in front of
     it, so only what is left over goes on the plane: a window whose place is
     `viewport.x` is already drawn at `area.x`. */
  rect.x = viewport.x + margin / viewport.zoom;
  rect.y = viewport.y + margin / viewport.zoom;
  rect.width = width / viewport.zoom;
  rect.height = height / viewport.zoom;
  /* Put here by a person, so a late configure from the client does not get to
     place it again. See canvasReparented. */
  markCanvasMoved(focusedId);
  relayoutAll();
  return true;
}

/* Drag a window across the plane, in response to Mod4 + left drag.
 *
 * The compositor forwards the gesture as a delta in *screen* pixels — it knows
 * where the pointer went and nothing about the plane — so the step is divided
 * by the zoom on the way in. That is what makes a drag track the pointer at any
 * zoom rather than running away from it by a factor of four when the view is
 * out at 0.25.
 *
 * Nothing is clamped. A floating window is held against the edge of its screen
 * so that a window dragged off it can be got back, and a plane has no edge to
 * hold anything against — dragging a window a long way away is a thing you are
 * allowed to do here, and `canvas.fit` is how you find it again.
 *
 * Returns whether it did anything, so moveByDelta can fall through to the
 * floating path in every other layout. */
function canvasDragBy(id, dx, dy) {
  if (layoutMode !== 'canvas' || id == null) return false;
  /* A place is kept for the session, so one malformed delta is not a bad
     frame — it is a NaN written into the plane, and everything read off it
     afterwards, bounds and fit included, is NaN too. */
  if (!Number.isFinite(dx) || !Number.isFinite(dy)) return false;
  const rect = canvasPlaces.get(id);
  if (!rect) return false;

  const zoom = canvasViewportOf(workspaceOf(id)).zoom;
  rect.x += dx / zoom;
  rect.y += dy / zoom;
  /* Put here by a person, so a late message from a client does not get to
     move it again. See canvasReparented. */
  markCanvasMoved(id);

  /* Follow the pointer exactly while the drag lasts; the class comes off once
     the window stops moving. The same treatment, and the same 120ms, a
     floating window gets in moveByDelta — a window being dragged should not be
     easing toward the pointer. */
  const view = views.get(id);
  if (view) {
    view.el.classList.add('dragging');
    clearTimeout(view.dragTimer);
    view.dragTimer = setTimeout(
      () => view.el.classList.remove('dragging'), 120);
  }

  gestureRelayout();
  return true;
}

/* Resize a window on the plane, in response to Mod4 + right drag.
 *
 * A real resize, unlike everything else here: the place *is* the client's size,
 * so this is the one gesture on the canvas that reconfigures a client. That is
 * what the gesture means, and it is a resize someone is doing by hand rather
 * than one the layout is doing sixty times a second, which is the cost the rest
 * of this file is arranged to avoid.
 *
 * Clamped to a minimum, so a drag cannot shrink a window to nothing and leave
 * a rectangle too small to take hold of again. */
function canvasResizeBy(id, dx, dy, west, north) {
  if (layoutMode !== 'canvas' || id == null) return false;
  /* As in canvasDragBy: a place outlives the gesture that wrote it. */
  if (!Number.isFinite(dx) || !Number.isFinite(dy)) return false;
  const rect = canvasPlaces.get(id);
  if (!rect) return false;

  /* The client's own minimum first, and this layout's floor only where the
     client did not name one. A place *is* the size the client is asked to be,
     so a place under the client's minimum is a request it will refuse — and a
     window drawn at a size it refused is one whose picture and whose idea of
     itself disagree, which is a click landing somewhere other than where it
     was aimed. */
  const view = views.get(id);
  const floorWidth = Math.max(CANVAS.minSize, view?.minWidth ?? 0);
  const floorHeight = Math.max(CANVAS.minSize, view?.minHeight ?? 0);

  /* A pull on the left or top edge pins the opposite one, which on a plane
     means moving the place by however much the size actually changed — after
     the clamp, so a window that has stopped shrinking has also stopped
     sliding. In world units on both counts: the place is on the plane and the
     delta came off the screen, so the zoom divides the delta and nothing
     else. */
  const zoom = canvasViewportOf(workspaceOf(id)).zoom;
  const width = Math.max(floorWidth, rect.width + (west ? -dx : dx) / zoom);
  const height = Math.max(floorHeight, rect.height + (north ? -dy : dy) / zoom);
  if (west) rect.x += rect.width - width;
  if (north) rect.y += rect.height - height;
  rect.width = width;
  rect.height = height;
  markCanvasMoved(id);
  gestureRelayout();
  return true;
}

/* Pan the plane, in response to Mod4 + left drag on the desktop.
 *
 * The delta is where the pointer went, and the view goes the other way: drag
 * the desktop to the right and what is on it comes with you, which is the
 * direction every canvas has moved since the first one. Panning the view right
 * instead would have the plane sliding out from under the hand. */
function canvasPanBy(dx, dy) {
  if (layoutMode !== 'canvas') return false;
  if (!Number.isFinite(dx) || !Number.isFinite(dy)) return false;
  canvasPan(-dx, -dy, true);
  return true;
}

/* Move the focused window across the plane. An edit to the plane, so the step
 * is in world units — see CANVAS.moveStep. */
function canvasMoveFocused(direction) {
  const target = canvasTarget();
  if (!target || focusedId == null) return false;
  const rect = canvasPlaces.get(focusedId);
  if (!rect) return false;

  const step = CANVAS.moveStep;
  rect.x += direction === 'left' ? -step : direction === 'right' ? step : 0;
  rect.y += direction === 'up' ? -step : direction === 'down' ? step : 0;
  markCanvasMoved(focusedId);

  /* Follow it, or moving a window towards the edge walks it off the screen and
     the next press is moving something you cannot see. */
  canvasViewports.set(target.workspace,
    canvasFollow(rect, canvasViewportOf(target.workspace), target.area));
  relayoutAll();
  return true;
}

/* Step focus through the windows on the plane, including the ones off screen.
 *
 * Mod4+Tab is the compositor's own `focus next` in every layout that draws all
 * of its windows. A plane does not: a window panned out of view is left out of
 * the render, which tells the compositor its surface is not on screen — and the
 * compositor's cycle walks what is on screen, so those windows could not be
 * reached by the key whose whole job is reaching them. Panning back to a window
 * to Tab to it is the manoeuvre this layout exists to avoid.
 *
 * So the cycle is answered here, from `canvasOrderOf`, which is every window on
 * the workspace whether it is drawn or not — the same list the layout projects
 * from, in the same order, so Tab follows the plane rather than the order the
 * windows happened to open in. Focusing one is enough to see it: the
 * `view.focused` handler calls `canvasFocused`, which pans the view onto it.
 *
 * The same shape as scrollFocus, and for the reason its header gives: where
 * the window you want is off screen, the layout has to answer the question. */
function canvasFocusStep(forward) {
  const target = canvasTarget();
  if (!target) return false;

  const ids = canvasOrderOf(target.workspace);
  if (ids.length === 0) return false;

  /* Nothing focused, or focus is on another workspace: start at the end the
     key is coming from rather than doing nothing, which is what Tab on a
     freshly switched workspace has to mean. */
  const at = focusedId != null ? ids.indexOf(focusedId) : -1;
  const next = at < 0
    ? (forward ? 0 : ids.length - 1)
    : (at + (forward ? 1 : -1) + ids.length) % ids.length;

  send({ type: 'view.focus', id: ids[next] });
  return true;
}

/* Bring the newly focused window into view.
 *
 * Called from the view.focused handler beside matrixFocused, and a no-op in
 * every other layout. Minimal, per canvasFollow: focusing something already on
 * screen moves nothing at all. */
function canvasFocused(id) {
  if (layoutMode !== 'canvas' || id == null) return;
  const workspace = workspaceOf(id);
  if (workspace === null) return;
  const output = outputs.get(hostOfWorkspace(workspace));
  const area = canvasAreaOf(output);
  if (!area) return;
  const rect = canvasPlaces.get(id);
  if (!rect) return;
  canvasViewports.set(workspace,
    canvasFollow(rect, canvasViewportOf(workspace), area));
}

/* ------------------------------------------------------------------------
 * Rendering
 * --------------------------------------------------------------------- */

/* Forget everything the canvas did to a window, and undo it.
 *
 * Called on the way into any relayout that is not the canvas's. The inline
 * rect is not the stylesheet's to reset: a window left carrying left/top from
 * the plane would be positioned in the middle of a tiling column. The *places*
 * are deliberately kept — leaving the canvas and coming back should find the
 * plane as it was left, which is the one piece of this state that is worth
 * more than the layout it belongs to. Cheap when there is nothing to undo. */
function clearCanvasState() {
  for (const [, view] of views) {
    if (!view.canvas) continue;
    view.canvas = null;
    view.el.classList.remove('plane', 'front');
    /* The minimum goes back to the client's own, unscaled: every other layout
       is flexbox, and that is where the element's own min-width is what
       enforces it. Left at a scaled value, a window that had been zoomed out
       would arrive in a tiling column able to shrink to a quarter of what the
       client accepts. */
    Object.assign(view.el.style, {
      left: '', top: '', width: '', height: '',
      minWidth: view.minWidth > 0 ? `${view.minWidth}px` : '',
      minHeight: view.minHeight > 0 ? `${view.minHeight}px` : '',
    });
  }
}

/* One output's plane, as elements.
 *
 * Positioned absolutely inside a container of its own rather than onto the
 * output's `.windows` directly, so the container is what the FLIP in
 * geometry.js measures against and a workspace switch does not animate every
 * window from wherever the previous workspace's window of the same index
 * happened to be. Same reason renderSolar and renderMatrix have one. */
function renderCanvas(placements, output) {
  const el = document.createElement('div');
  el.className = 'canvas-field';

  for (const placement of placements) {
    const view = views.get(placement.id);
    if (!view) continue;

    /* Off the edge of the view. Left out of renderedIds and out of the DOM,
       which is the whole of what "not drawn" means here: relayoutAll hides
       what it did not render and tells the compositor the window is off
       screen, so its surface stops being painted into a hole that is not
       there. That is the same route a collapsed tab takes, and it is what
       stops a plane holding fifty windows from costing fifty holes. */
    if (placement.offscreen) continue;

    /* A fullscreen window covers the output and is not on the plane while it
       does. Its place is untouched, so leaving fullscreen puts it back exactly
       where it was — the same treatment solar gives a covering window. */
    const covering = isFullscreen(placement.id);
    const scale = covering ? 1 : placement.scale;

    view.canvas = {
      scale,
      /* In front of the windows it overlaps, so its frame is drawn over their
         holes rather than under them, and so the compositor raises its surface
         above theirs. Only the focused one: the compositor offers exactly two
         stacking bands (restack() in state.rs), so marking every window on the
         plane would put them all in the top band and settle nothing. See
         reportGeometry, where this becomes `floating` on the wire. */
      lift: covering || placement.id === focusedId,
    };

    view.el.classList.add('plane');
    /* Not `floating` as well. relayoutAll marks the windows it pins to the
       screen with that class and the canvas pins nothing; leaving it on a
       window that was floating under the previous layout would have two rules
       with two opinions about the same rectangle. */
    view.el.classList.remove('floating');
    /* Drawn over what it overlaps, which on a plane is a real question — see
       the `.front` rule in shell.css.
       Never on a covering window, though it is lifted on the wire: `.front`
       and `.window.fullscreen` are equally specific and `.front` is written
       later in the stylesheet, so a fullscreen window carrying both would take
       the canvas's layer instead of fullscreen's — the exact trap the comment
       over that rule describes, which is why solar drops its tier classes for
       a covering window too. Fullscreen is already in front of everything;
       saying so twice is what breaks it. */
    view.el.classList.toggle('front', !covering && view.canvas.lift);

    /* Sized at the client's rectangle and drawn at `scale`, so the element is
       the *drawn* size and reportGeometry divides it back out. Written this way
       rather than as a smaller rectangle for the reason in the header: a client
       on a zoomed-out plane is never asked to resize.

       Written for a covering window too, and overridden: `.window.fullscreen`
       carries `inset: 0 !important` precisely so that a layout holding a rect
       in inline style does not have to strip and restore it. */
    /* The client's minimum, scaled with everything else.
     *
     * addView puts the minimum on the element so that flexbox enforces it,
     * which is right in a layout made of flexboxes and actively wrong in one
     * that draws a window smaller than it is. `min-width` is in drawn pixels
     * and beats `width`, so an unscaled minimum stops the element shrinking
     * partway through a zoom out: the window is then drawn at its minimum
     * while reportGeometry divides the measured size by the scale and tells
     * the compositor the *client* should be that much bigger. Zooming out then
     * grows every client instead of shrinking its picture — the resize storm
     * this layout is arranged to avoid, arriving through the stylesheet — and
     * where a click lands stops matching what is on screen, because the client
     * is laid out for a size nothing is drawn at.
     *
     * Chrome is where it shows first, having the largest minimum of anything
     * most people run. */
    const minWidth = Math.round((view.minWidth ?? 0) * scale);
    const minHeight = Math.round((view.minHeight ?? 0) * scale);
    Object.assign(view.el.style, {
      left: `${placement.x}px`,
      top: `${placement.y}px`,
      width: `${Math.round(placement.width * scale)}px`,
      height: `${Math.round(placement.height * scale)}px`,
      minWidth: minWidth > 0 ? `${minWidth}px` : '',
      minHeight: minHeight > 0 ? `${minHeight}px` : '',
      flexGrow: '',
    });

    renderedIds.add(placement.id);
    el.append(view.el);
  }

  return el;
}
