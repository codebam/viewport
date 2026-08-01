/* SPDX-License-Identifier: MIT
 *
 * The solar layout: one window in the middle, the rest in orbit around it.
 *
 * The third layout model, beside the i3-style tree in tiling.js and niri's
 * strip in scrolling.js. Unlike both of those it computes rectangles. An orbit
 * is not expressible in flexbox — there is no arrangement of rows and columns
 * that puts four windows at the corners of a fifth — so this is the one place
 * in the shell that does arithmetic rather than handing the problem to the
 * browser. See docs/solar.md for the model and the reasoning; what follows is
 * the implementation of it.
 *
 * Everything else about the design survives. The rectangles are written as
 * inline style and then *measured* by geometry.js like every other window's, so
 * a window mid-transition is still reported at wherever it actually got to
 * rather than where it was aimed. The tiling tree is still the source of which
 * windows exist and in what order; this reads that order and never writes it,
 * which is what lets window.move, the session format and the overview keep
 * working without knowing solar exists.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* Every number the layout depends on. Read at use rather than captured, so
 * editing one and reloading the shell takes effect without a restart. */
const SOLAR = {
  /* Fraction of the output's usable area the sun occupies. The sun keeps the
     output's aspect ratio, so each of its dimensions is √sunArea of the
     output's — 0.60 gives 77.5%, and a margin of 11.3% around it. */
  sunArea: 0.60,
  sunAreaMin: 0.30,
  sunAreaMax: 0.85,
  sunAreaStep: 0.05,

  /* A satellite's size as a fraction of the sun's. Wider than the margin the
     sun leaves, on purpose: a satellite is partly eclipsed by the sun rather
     than ringed neatly outside it, which is the whole look. */
  innerRatio: 0.42,
  /* How far out the inner ring sits. 1 puts a satellite hard against the
     screen edge; lower tucks it under the sun and leaves the outer band free
     for the cold windows. Below about 0.5 it vanishes behind the sun. */
  innerClearance: 0.72,

  /* What a cold window is drawn at. Its *client* keeps the size of an inner
     satellite and the compositor shrinks the surface — see the note on
     placements below for why this is not simply a smaller rectangle. */
  outerScale: 0.40,
  /* How far in each lap of the outer slot table steps, so the window after a
     full lap is not exactly underneath the one it followed. */
  outerLapInset: 0.86,
  outerMaxLaps: 3,

  /* Slot angles, clockwise from screen-right with y downward: 0 right, 90
     down, 180 left, 270 up. Filled in the order written.

     Fixed tables rather than redistributing n windows over 2π, deliberately:
     opening a third window must not move the first two. Corners lead the inner
     table because a centred sun leaves the most room diagonally; the top comes
     last in both because the bar is there. */
  innerSlots: [45, 315, 135, 225, 0, 180, 90, 270],
  outerSlots: [90, 0, 180, 45, 135, 315, 225, 270],

  /* A window parked on the companion monitor in the Lagrange field. Larger
     than an outer orbit because it has a screen to itself rather than a
     margin. */
  lagrangeScale: 0.55,
  lagrangeClearance: 0.82,

  /* The resting opacity of each tier. The sun is never anything but 1: the
     window being typed into is not decoration. A companion star is the sun of
     a monitor that does not hold keyboard focus. */
  opacity: { sun: 1, companion: 0.90, inner: 0.78, outer: 0.40 },

  /* Radial weight in the ray-cast focus score. Without it, ray-casting on a
     two-ring layout always picks the far ring: a cold window dead on the axis
     beats an inner satellite ten degrees off it, every time. */
  rayBias: 0.35,
};

/* 'binary'   — every monitor runs its own system, independently.
 * 'lagrange' — the focused monitor keeps the sun and the inner orbit, and
 *              parks its cold windows on the other one. */
let solarField = 'binary';

/* workspace -> the window that is its sun. Remembered per workspace rather
 * than derived from focusedId alone, so a workspace no monitor is showing
 * still has a centre and does not reshuffle when you come back to it. */
const solarSuns = new Map();

/* workspace -> how many slots the satellites have been rotated by. */
const solarSpins = new Map();

/* id -> where the last plan put its centre, and in which tier. Ray-cast focus
 * scores against this: it is the layout as it actually stands, which is not
 * recoverable from the tree. */
const solarLastPlan = new Map();

/* ------------------------------------------------------------------------
 * Geometry
 * --------------------------------------------------------------------- */

function solarRadians(degrees) {
  return (degrees * Math.PI) / 180;
}

/* An angle's point on the boundary of the rectangle [-rx, rx] x [-ry, ry].
 *
 * The Chebyshev normalisation is what distinguishes this from the ellipse
 * inscribed in the same rectangle, and all three reasons for it matter:
 *
 *   - 45 degrees lands on the actual corner rather than at 0.707 of the way
 *     out. Corners are where a centred sun leaves the most free space, so a
 *     satellite sent to one should arrive there.
 *   - |dx| <= rx and |dy| <= ry hold for every angle. Give rx the value
 *     (width - w)/2 and the window is inside the output by construction: no
 *     clamping anywhere, and no window ever half off the screen at some
 *     awkward angle.
 *   - "Near the edges and corners", which is what the outer orbit is, is
 *     literally what a rectangle boundary is.
 */
function solarProject(degrees, rx, ry) {
  const theta = solarRadians(degrees);
  const c = Math.cos(theta);
  const s = Math.sin(theta);
  const longest = Math.max(Math.abs(c), Math.abs(s));
  /* Only reachable if both cos and sin are zero, which they never are. Guarded
     because a NaN here would be written into a style and lose the window. */
  const t = longest > 0 ? 1 / longest : 0;
  return { dx: rx * c * t, dy: ry * s * t };
}

/* The sun's rectangle: `fraction` of the area, centred, at the area's aspect
 * ratio. Taking the square root of the fraction on each axis is what keeps the
 * product exactly `fraction` of the whole. */
function solarSunRect(area, fraction) {
  const k = Math.sqrt(Math.max(0, Math.min(1, fraction)));
  const width = Math.round(area.width * k);
  const height = Math.round(area.height * k);
  return {
    x: Math.round(area.x + (area.width - width) / 2),
    y: Math.round(area.y + (area.height - height) / 2),
    width,
    height,
  };
}

/* ------------------------------------------------------------------------
 * The layout itself
 *
 * A pure function of (ids, sun, area, spin) — no DOM, no globals, nothing
 * measured. That is what lets tests/shell.test.js check the arithmetic
 * directly rather than through a stubbed getBoundingClientRect that returns
 * fixed numbers and proves nothing about it.
 * --------------------------------------------------------------------- */

/* A placement is the rectangle the *client* gets, plus how it is drawn.
 *
 * `width` and `height` are the client's size and `scale` is what the
 * compositor draws it at, so an outer window occupies `scale` of its own
 * rectangle on screen. Two separate numbers rather than one small rectangle,
 * because focus moves constantly and every focus change reshuffles the orbits:
 * a cold window given a genuinely smaller rectangle would be reconfigured —
 * and would relayout itself — several times a second. Drawn scale costs the
 * compositor a surface-tree walk and costs the client nothing at all. It is
 * the same mechanism the overview uses, for the same reason.
 *
 * `lift` asks for the window to be stacked above the rest. The compositor
 * offers exactly two z-bands and raises everything the shell marked floating
 * above everything it did not (state.rs, restack), so this maps onto that:
 * only the sun is lifted, which is the rule the model exists to enforce.
 */
function solarPlacement(id, tier, rect, scale, opacity, lift) {
  return {
    id,
    tier,
    x: Math.round(rect.x),
    y: Math.round(rect.y),
    width: Math.max(1, Math.round(rect.width)),
    height: Math.max(1, Math.round(rect.height)),
    /* Where it is drawn, for ray-cast focus. The centre of what is on screen,
       which for a scaled window is not the centre of its rectangle. */
    cx: Math.round(rect.x + (rect.width * scale) / 2),
    cy: Math.round(rect.y + (rect.height * scale) / 2),
    scale,
    opacity,
    lift,
  };
}

/* Place one output's worth of a solar system.
 *
 * Returns { here, spilled }: the windows placed in this area, and the cold
 * ones held back for a Lagrange field elsewhere. `spilled` is always empty
 * unless `holdOuter` was asked for, so a caller that does not park anything can
 * ignore it.
 */
function solarPlacements({ ids, sun, area, hot = true, spin = 0,
    holdOuter = false }) {
  const here = [];
  const spilled = [];
  if (!area || area.width <= 0 || area.height <= 0 || ids.length === 0) {
    return { here, spilled };
  }

  const sunRect = solarSunRect(area, SOLAR.sunArea);
  const cx = area.x + area.width / 2;
  const cy = area.y + area.height / 2;

  if (sun != null && ids.includes(sun)) {
    here.push(solarPlacement(sun, 'sun', sunRect, 1,
      hot ? SOLAR.opacity.sun : SOLAR.opacity.companion, true));
  }

  /* The satellites, in tree order with the sun taken out. Removing rather than
     rotating the list is what keeps a focus change cheap to look at: focusing
     a satellite swaps it with the sun and leaves every other window exactly
     where it was, instead of shifting all of them by one slot. */
  const satellites = ids.filter((id) => id !== sun);
  if (satellites.length === 0) return { here, spilled };

  /* Spin rotates the assignment of windows to slots without touching the tree.
     Modulo twice: the remainder of a negative number is negative in
     JavaScript, and a negative index would silently place nothing. */
  const n = satellites.length;
  const offset = ((spin % n) + n) % n;
  const ordered = satellites.slice(offset).concat(satellites.slice(0, offset));

  const innerWidth = Math.round(SOLAR.innerRatio * sunRect.width);
  const innerHeight = Math.round(SOLAR.innerRatio * sunRect.height);
  const innerRx = SOLAR.innerClearance * (area.width - innerWidth) / 2;
  const innerRy = SOLAR.innerClearance * (area.height - innerHeight) / 2;

  const outerWidth = Math.round(SOLAR.outerScale * innerWidth);
  const outerHeight = Math.round(SOLAR.outerScale * innerHeight);

  const capacity = SOLAR.innerSlots.length;

  ordered.forEach((id, index) => {
    if (index < capacity) {
      const { dx, dy } = solarProject(SOLAR.innerSlots[index], innerRx, innerRy);
      here.push(solarPlacement(id, 'inner', {
        x: cx + dx - innerWidth / 2,
        y: cy + dy - innerHeight / 2,
        width: innerWidth,
        height: innerHeight,
      }, 1, SOLAR.opacity.inner, false));
      return;
    }

    /* Cold. Either parked on the other monitor, or pushed to this one's edge. */
    if (holdOuter) {
      spilled.push(id);
      return;
    }

    const j = index - capacity;
    const slots = SOLAR.outerSlots.length;
    const lap = Math.min(Math.floor(j / slots), SOLAR.outerMaxLaps);
    const inset = Math.pow(SOLAR.outerLapInset, lap);
    const rx = inset * (area.width - outerWidth) / 2;
    const ry = inset * (area.height - outerHeight) / 2;
    const { dx, dy } = solarProject(SOLAR.outerSlots[j % slots], rx, ry);

    /* The rectangle is the client's size — an inner satellite's — and the
       offset is worked out from the *drawn* size, because what has to stay on
       screen and clear of its neighbours is what the compositor paints. */
    here.push(solarPlacement(id, 'outer', {
      x: cx + dx - outerWidth / 2,
      y: cy + dy - outerHeight / 2,
      width: innerWidth,
      height: innerHeight,
    }, SOLAR.outerScale, SOLAR.opacity.outer, false));
  });

  return { here, spilled };
}

/* Windows parked on a monitor of their own: no sun, one ring, the whole
 * screen. Same projection and the same fixed-slot rule as an orbit, which is
 * why a window that spills over and back does not jump about. */
function solarLagrangePlacements(ids, area) {
  const placed = [];
  if (!area || area.width <= 0 || area.height <= 0) return placed;

  const sunRect = solarSunRect(area, SOLAR.sunArea);
  const width = Math.round(SOLAR.innerRatio * sunRect.width);
  const height = Math.round(SOLAR.innerRatio * sunRect.height);
  const drawnWidth = Math.round(SOLAR.lagrangeScale * width);
  const drawnHeight = Math.round(SOLAR.lagrangeScale * height);
  const cx = area.x + area.width / 2;
  const cy = area.y + area.height / 2;

  const slots = SOLAR.outerSlots.length;
  ids.forEach((id, index) => {
    const lap = Math.min(Math.floor(index / slots), SOLAR.outerMaxLaps);
    const inset = SOLAR.lagrangeClearance * Math.pow(SOLAR.outerLapInset, lap);
    const rx = inset * (area.width - drawnWidth) / 2;
    const ry = inset * (area.height - drawnHeight) / 2;
    const { dx, dy } = solarProject(SOLAR.outerSlots[index % slots], rx, ry);
    placed.push(solarPlacement(id, 'lagrange', {
      x: cx + dx - drawnWidth / 2,
      y: cy + dy - drawnHeight / 2,
      width,
      height,
    }, SOLAR.lagrangeScale, SOLAR.opacity.outer, false));
  });
  return placed;
}

/* ------------------------------------------------------------------------
 * Reading the shell's state
 * --------------------------------------------------------------------- */

/* The windows solar places on a workspace: everything tiled, in tree order.
 *
 * Floating windows are left out entirely. A dialog is floating because tiling
 * it is the wrong thing to do — that judgement is the compositor's, from what
 * the client said about itself (views.rs, wants_floating) — and giving one an
 * orbital slot is the same mistake as giving it a tiling column. The existing
 * floating path in relayoutAll places them, here as everywhere else. */
function solarIdsOf(workspace) {
  const root = workspaces.get(workspace);
  if (!root) return [];
  return dynamicOrder(root).filter(
    (id) => views.has(id) && !isFloating(id));
}

/* Which window is a workspace's sun.
 *
 * The focused one, when focus is on this workspace. Otherwise whatever was
 * last focused here, and failing that the first window in tree order — a
 * workspace that has never held focus still has to have a centre. The answer
 * is written back, so it is stable until focus moves again. */
function solarSunOf(workspace, ids = solarIdsOf(workspace)) {
  if (ids.length === 0) {
    solarSuns.delete(workspace);
    return null;
  }
  if (focusedId != null && ids.includes(focusedId)) {
    solarSuns.set(workspace, focusedId);
    return focusedId;
  }
  const remembered = solarSuns.get(workspace);
  if (remembered != null && ids.includes(remembered)) return remembered;
  solarSuns.set(workspace, ids[0]);
  return ids[0];
}

/* The area of an output windows may be placed in.
 *
 * Measured rather than derived from the output's mode: the bar, a panel's
 * exclusive zone and the gap are all in the stylesheet, and `.windows` is the
 * element they have already been subtracted from. A theme that moves any of
 * them moves this, which is the point.
 *
 * The origin is zero and not the element's page position, because what comes
 * out of the layout is written straight back as `left` and `top` on a window
 * absolutely positioned *inside* that element. Floating windows keep their
 * rects the same way and for the same reason: in page coordinates every window
 * on the second monitor would be offset by the width of the first
 * (windows.js, setFloating). The arithmetic is all relative, so passing a
 * different origin in through recalculateSolarLayout() simply moves the result
 * — nothing here depends on the origin being zero. */
function solarAreaOf(output) {
  const rect = output?.windowsEl?.getBoundingClientRect();
  if (!rect || rect.width <= 0 || rect.height <= 0) return null;
  return { x: 0, y: 0, width: rect.width, height: rect.height };
}

/* The layout for one workspace on one output.
 *
 * The entry point the rest of the shell asks for by name, and a thin one: it
 * resolves the workspace's windows, its sun and its spin out of shell state and
 * hands them to solarPlacements(), which is where the arithmetic is and which
 * knows nothing about any of that. `outputGeometry` may be omitted, in which
 * case the output showing the workspace is measured. */
function recalculateSolarLayout(workspace, outputGeometry = null) {
  const area = outputGeometry
    ?? solarAreaOf(outputs.get(hostOfWorkspace(workspace)));
  const ids = solarIdsOf(workspace);
  const sun = solarSunOf(workspace, ids);
  return solarPlacements({
    ids,
    sun,
    area,
    hot: sun === focusedId,
    spin: solarSpins.get(workspace) ?? 0,
  }).here;
}

/* The monitor a Lagrange field may go on: another output, showing nothing.
 *
 * Only an empty one. A monitor with windows on it is showing something someone
 * chose to put there, and burying that under another workspace's cold windows
 * is worse than a crowded outer orbit. With nowhere to park, lagrange behaves
 * as binary — which is also what a single-monitor desktop gets. */
function solarCompanionOf(name) {
  for (const [other, output] of outputs) {
    if (other === name) continue;
    if (solarIdsOf(output.workspace).length > 0) continue;
    if (!solarAreaOf(output)) continue;
    return other;
  }
  return null;
}

/* Every output's placements, worked out together.
 *
 * One pass for all of them rather than one per output as they are rendered,
 * because a Lagrange field puts one output's windows on another and the
 * outputs are rendered in whatever order the map is in. Computing the whole
 * plan first means the companion already knows what it is holding by the time
 * it is drawn. */
function planSolar() {
  solarLastPlan.clear();
  const plan = new Map();
  for (const name of outputs.keys()) plan.set(name, []);

  const active = activeOutputName();

  for (const [name, output] of outputs) {
    const area = solarAreaOf(output);
    if (!area) continue;
    const ids = solarIdsOf(output.workspace);
    if (ids.length === 0) continue;

    const sun = solarSunOf(output.workspace, ids);

    /* Only the focused monitor parks anything. The other one is running its
       own system and its cold windows belong in its own margin. */
    const companion = (solarField === 'lagrange' && name === active)
      ? solarCompanionOf(name) : null;

    const { here, spilled } = solarPlacements({
      ids,
      sun,
      area,
      hot: sun === focusedId,
      spin: solarSpins.get(output.workspace) ?? 0,
      holdOuter: companion !== null,
    });

    plan.get(name).push(...here);

    if (companion && spilled.length > 0) {
      const field = solarAreaOf(outputs.get(companion));
      plan.get(companion).push(...solarLagrangePlacements(spilled, field));
    }
  }

  for (const placements of plan.values()) {
    for (const placement of placements) solarLastPlan.set(placement.id, placement);
  }
  return plan;
}

/* ------------------------------------------------------------------------
 * Rendering
 * --------------------------------------------------------------------- */

/* Forget everything solar did to a window, and undo it.
 *
 * Called on the way into any relayout that is not solar's. The inline rect and
 * the opacity are not the stylesheet's to reset — a window left with left/top
 * from an orbit would be positioned in the middle of a tiling column, and one
 * left at 0.4 opacity would stay dim in every other layout for the rest of the
 * session. */
function clearSolarState() {
  let touched = false;
  for (const [id, view] of views) {
    if (!view.solar) continue;
    view.solar = null;
    view.el.classList.remove('orbit', 'sun', 'inner', 'outer', 'lagrange',
      'companion');
    Object.assign(view.el.style,
      { left: '', top: '', width: '', height: '' });
    if (view.solarOpacity !== undefined && view.solarOpacity !== 1) {
      send({ type: 'view.opacity', id, opacity: 1 });
    }
    view.solarOpacity = undefined;
    touched = true;
  }
  return touched;
}

/* What a window should rest at once anything fading it in has finished.
 *
 * geometry.js tweens a new window from 0 and would otherwise land every one of
 * them on 1, which for a cold window is the wrong answer by a factor of two
 * and a half — it would fade in bright and stay bright until something else
 * caused a relayout. */
function solarRestingOpacity(id) {
  if (layoutMode !== 'solar') return 1;
  return views.get(id)?.solar?.opacity ?? 1;
}

/* One output's system, as elements.
 *
 * Positioned absolutely inside a container of its own rather than written onto
 * the output's `.windows` directly, so that the container is what the FLIP in
 * geometry.js measures against and a workspace switch does not animate every
 * window from wherever the previous workspace's window of the same index
 * happened to be. */
function renderSolar(placements, output) {
  const el = document.createElement('div');
  el.className = 'solar-field';

  for (const placement of placements) {
    const view = views.get(placement.id);
    if (!view) continue;

    /* A fullscreen window covers the output and is not in any orbit while it
       does. It is usually the sun as well — going fullscreen is something you
       do to the window you are in — but a client can ask for it on its own
       behalf without holding focus, and a video playing at 40% opacity in the
       corner of the screen it was told to fill is not a layout decision
       anybody made. */
    const covering = isFullscreen(placement.id);

    view.solar = {
      scale: covering ? 1 : placement.scale,
      opacity: covering ? 1 : placement.opacity,
      lift: covering || placement.lift,
      tier: placement.tier,
      /* What the window is clipped against. Only set for a window parked on a
         monitor that is not its workspace's host: reportGeometry otherwise
         clips against the output showing the workspace, which for a parked
         window is the wrong screen and would crop it away entirely. */
      cell: placement.tier === 'lagrange' ? output.windowsEl : null,
    };

    view.el.classList.add('orbit');
    for (const tier of ['sun', 'inner', 'outer', 'lagrange']) {
      view.el.classList.toggle(tier, !covering && tier === placement.tier);
    }
    /* A sun that is not on the monitor holding the keyboard: dimmer, and
       without the focus ring, so which of two monitors is live is answerable
       by looking at it. */
    view.el.classList.toggle('companion',
      !covering && placement.tier === 'sun'
      && placement.opacity !== SOLAR.opacity.sun);

    /* Sized at the client's rectangle and drawn at `scale`, so the element is
       the *drawn* size and reportGeometry divides it back out. Written as a
       transform rather than as a smaller rectangle for the reason in
       solarPlacement: a cold client is never asked to resize. */
    Object.assign(view.el.style, {
      left: `${placement.x}px`,
      top: `${placement.y}px`,
      width: `${Math.round(placement.width * placement.scale)}px`,
      height: `${Math.round(placement.height * placement.scale)}px`,
      flexGrow: '',
    });

    renderedIds.add(placement.id);
    el.append(view.el);
  }

  return el;
}

/* Put each tier's opacity on the wire.
 *
 * Opacity is not CSS. The frame is the shell's, but the window's contents are
 * a surface the compositor draws and no style here can touch it, so the tier's
 * dimming is a message.
 *
 * Only when it changed: a relayout runs on every focus change and on every
 * frame of a drag, and the compositor answers each view.opacity by walking the
 * window's whole surface tree. And not at all for a window that is fading in,
 * which is already being driven from zero to this same value a frame at a time
 * — sent here as well, it would flash at full brightness on the frame it
 * opened and then start the fade.
 *
 * Called from relayoutAll once the rendering has settled which windows are on
 * screen, rather than from renderSolar, because that is the earliest point at
 * which either of those two facts is known. */
function settleSolarOpacity(fading) {
  for (const [id, view] of views) {
    if (view.el.hidden || fading.has(id)) continue;
    const wanted = view.solar?.opacity ?? 1;

    /* A window the compositor has never been told a rectangle for has just
       been opened, and addView fades it in *after* the relayout that placed
       it. Sending its resting opacity here would therefore go out ahead of
       that fade: the window would appear at its tier's brightness, drop to
       nothing, and fade back to where it already was. The fade lands on the
       same value — it asks solarRestingOpacity for it — so the value is only
       recorded, not sent. */
    if (view.box === null) {
      view.solarOpacity = wanted;
      continue;
    }

    /* Absent is one, because that is what the compositor holds for a window
       nothing has said anything about. */
    if ((view.solarOpacity ?? 1) === wanted) {
      view.solarOpacity = wanted;
      continue;
    }
    view.solarOpacity = wanted;
    send({ type: 'view.opacity', id, opacity: wanted });
  }
}

/* ------------------------------------------------------------------------
 * Commands
 * --------------------------------------------------------------------- */

/* An angle difference brought into (-180, 180]. */
function solarWrapDegrees(degrees) {
  let d = degrees % 360;
  if (d > 180) d -= 360;
  if (d <= -180) d += 360;
  return d;
}

const SOLAR_RAYS = { right: 0, down: 90, left: 180, up: 270 };

/* Directional focus, by casting a ray from the sun.
 *
 * Not a question the compositor can answer for this layout — the same reason
 * the scrolling strip takes directional focus into the shell — because "the
 * window to the left" of a sun with satellites at four corners is a matter of
 * which corner the ray passes closest to, not of which rectangle shares an
 * edge.
 *
 * Angular error decides, with distance as a tiebreak. The bias is what stops a
 * cold window at the screen edge that happens to be dead on the axis from
 * beating an inner satellite ten degrees off it: on a two-ring layout, an
 * unbiased ray-cast picks the far ring every single time. Anything more than a
 * right angle off is behind you and is not a candidate at all. */
function solarRay(direction) {
  const phi = SOLAR_RAYS[direction];
  if (phi === undefined) return;

  const workspace = activeWorkspace();
  const ids = solarIdsOf(workspace);
  const sun = solarSunOf(workspace, ids);
  const origin = sun != null ? solarLastPlan.get(sun) : null;
  if (!origin) return;

  let best = null;
  let bestScore = Infinity;
  let furthest = 1;
  const candidates = [];

  for (const id of ids) {
    if (id === sun) continue;
    const placement = solarLastPlan.get(id);
    if (!placement) continue;
    const dx = placement.cx - origin.cx;
    const dy = placement.cy - origin.cy;
    const distance = Math.hypot(dx, dy);
    if (distance === 0) continue;
    const error = Math.abs(solarWrapDegrees(
      (Math.atan2(dy, dx) * 180) / Math.PI - phi));
    if (error > 90) continue;
    furthest = Math.max(furthest, distance);
    candidates.push({ id, error, distance });
  }

  for (const { id, error, distance } of candidates) {
    const score = error / 90 + SOLAR.rayBias * (distance / furthest);
    if (score < bestScore) {
      bestScore = score;
      best = id;
    }
  }

  /* Nothing that way on this monitor. Running off the edge onto the next one
     is what every other layout here does, and the setting that governs it is
     the same one. */
  if (best === null) {
    if (focusCrossesOutputs) focusOutputDirection(direction);
    return;
  }
  send({ type: 'view.focus', id: best });
}

/* Rotate which window is in which slot, leaving the sun and the tree alone.
 *
 * The gesture for "show me the next few" on a workspace with more windows than
 * the inner orbit holds: spinning brings cold windows round into the warm
 * slots without changing focus and without reordering anything. */
function solarSpin(step) {
  const workspace = activeWorkspace();
  const count = solarIdsOf(workspace).length - 1;
  if (count <= 1) return;
  solarSpins.set(workspace, (solarSpins.get(workspace) ?? 0) + step);
  relayoutAll();
}

/* Throw the focused window at the other monitor, where it arrives as the sun.
 *
 * Not moveViewToOutput(): that takes a direction and answers it from the
 * *active* output, which during a slingshot is not necessarily the one the
 * window is on, and it leaves the window wherever the receiving layout puts it.
 * The promotion is the point — a window flung across the desk that landed cold
 * in a corner would have to be hunted for — so the move is done here and the
 * landing workspace is told what its centre now is. */
function solarSlingshot() {
  if (focusedId == null) return;
  const id = focusedId;
  const workspace = workspaceOf(id);
  if (workspace === null) return;

  const target = solarOtherOutput(
    hostOfWorkspace(workspace) ?? activeOutputName());
  const output = target !== null ? outputs.get(target) : null;
  if (!output || output.workspace === workspace) return;

  /* A dialog stays a dialog on the other screen, but it cannot be a sun: the
     tree is what solar reads, and a floating window is not in it. */
  if (isFloating(id)) setFloating(id, false);
  removeLeaf(id);
  workspaceRoot(output.workspace).children.push(newLeaf(id));
  treeGeneration++;

  solarSuns.set(output.workspace, id);
  setActiveOutput(target);
  send({ type: 'view.focus', id });
  relayoutAll();
}

/* The next output round, by name. Two monitors is the case this is for, and
 * with more than two "the other one" is the next in the compositor's order,
 * which is left to right. */
function solarOtherOutput(name) {
  const names = [...outputs.keys()];
  if (names.length < 2) return null;
  const index = names.indexOf(name);
  if (index < 0) return names[0];
  return names[(index + 1) % names.length];
}

/* Grow or shrink the sun. The resize gesture for this layout: a satellite's
 * size is a function of the sun's, so there is nothing else to drag. */
function solarMass(step) {
  const next = Math.min(SOLAR.sunAreaMax, Math.max(SOLAR.sunAreaMin,
    SOLAR.sunArea + step * SOLAR.sunAreaStep));
  if (next === SOLAR.sunArea) return;
  SOLAR.sunArea = next;
  relayoutAll();
}

function solarToggleField() {
  solarField = solarField === 'binary' ? 'lagrange' : 'binary';
  relayoutAll();
}

/* Drop a workspace's remembered centre when it empties, so a workspace that is
 * filled again later starts from its first window rather than from a window
 * that closed. Called from removeView. */
function solarForget(id) {
  for (const [workspace, sun] of solarSuns) {
    if (sun === id) solarSuns.delete(workspace);
  }
  solarLastPlan.delete(id);
}
