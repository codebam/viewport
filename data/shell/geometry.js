/* SPDX-License-Identifier: MIT
 *
 * Measuring where the browser put things, and telling the compositor.
 *
 * The hinge of the whole design: a window's frame is DOM, its contents are a
 * surface the compositor draws, and this is what keeps the two in the same
 * place. Geometry is measured, never assumed.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* One layout query per element per frame, rather than one per call.
 *
 * A pump frame measures every window: its hole, the area it is clipped to and,
 * when it is lifted, its frame. The area is the same element for every window
 * on an output, so a desk of eight windows asked for the same rectangle eight
 * times. In a browser where layout is a thread — Servo — each of those is a
 * cross-thread query and not a field read, and the shell was making 421 of
 * them a second while painting 26 frames.
 *
 * Only a pass that does not write is allowed to use this. reportGeometry
 * measures and sends IPC, and IPC touches no DOM, so a whole pump frame is one
 * such pass; flipFrom is not, and reads live. Outside a pass measureOf is a
 * plain getBoundingClientRect, which is what every other caller gets.
 */
let measurePass = null;

function measureOf(el) {
  if (!measurePass) return el.getBoundingClientRect();
  let rect = measurePass.get(el);
  if (!rect) {
    rect = el.getBoundingClientRect();
    measurePass.set(el, rect);
  }
  return rect;
}

/* Everything measured between these two answers from one layout. Nothing
   between them may write to the DOM: a pass that outlives a style change hands
   back where things used to be. */
function beginMeasurePass() {
  measurePass = new Map();
}

function endMeasurePass() {
  measurePass = null;
}

function windowsAreaOf(workspace) {
  const name = hostOfWorkspace(workspace);
  const output = name !== null ? outputs.get(name) : null;
  if (!output) return null;

  const rect = measureOf(output.windowsEl);
  return {
    left: rect.left, top: rect.top,
    right: rect.left + rect.width, bottom: rect.top + rect.height,
  };
}

/* Returns whether a view.layout was sent, which is what pumpGeometry() reads
   to decide the layout is still moving. Every path answers that one question:
   a window that vanished, one measured at zero size and one that has not moved
   all report false, however different their reasons. */
function reportGeometry(id) {
  const view = views.get(id);
  if (!view) return false;

  const rect = measureOf(view.viewport);
  /* In the overview the element is inside a scaled container, so the measured
     rect is where it appears but not the size the client should be. The
     compositor is told the real size and the factor to draw it at.

     The solar layout's outer orbit does the same thing for the same reason: a
     cold window keeps the size of a warm one and is merely drawn small, so
     that a focus change — which reshuffles every orbit on the workspace — does
     not reconfigure half the clients on it. See solar.js. */
  const scale = view.overview?.scale ?? view.solar?.scale
    ?? view.canvas?.scale ?? 1;
  /* The whole device pixels *inside* the measured rect, rather than the ones
     nearest its edges.

     Layout is fractional and a hole is not: a column 1671.4 pixels wide starts
     somewhere in the middle of a pixel, and rounding to nearest moves the hole
     half a pixel either way. Half of the time that is outwards, and the
     compositor then draws the client over the pixel the page painted the
     border into — which is invisible with a 2px border, where one pixel
     survives, and is the whole border when it is 1px wide. Rounding inwards
     can only ever leave a sliver of the frame showing, and the frame is what
     is drawn underneath anyway.

     Sizes stay divided by the thumbnail scale: the compositor is told the size
     the *client* should be, which is the on-screen rectangle only when it is
     being drawn at full size. */
  const inset = {
    left: Math.ceil(rect.left),
    top: Math.ceil(rect.top),
    right: Math.floor(rect.left + rect.width),
    bottom: Math.floor(rect.top + rect.height),
  };
  const box = {
    x: inset.left,
    y: inset.top,
    width: Math.round((inset.right - inset.left) / scale),
    height: Math.round((inset.bottom - inset.top) / scale),
  };

  if (box.width <= 0 || box.height <= 0) {
    send({ type: 'view.visible', id, visible: false });
    return false;
  }

  /* How much of the window falls inside its output.
   *
   * A scrolled strip pushes columns past the edge, and `overflow: hidden`
   * does not help: it bounds what the *shell* paints, and the window is a real
   * Wayland surface the compositor draws itself. Left unclipped, a column
   * scrolled off the left of one monitor appears on the monitor beside it. The
   * compositor crops the surface to this rect. */
  /* In the overview a window is bounded by its thumbnail, not by the output.
     In solar's Lagrange field it is bounded by the monitor it was parked on,
     which is not the one showing its workspace — clipped against that, a
     parked window would be cropped away entirely. */
  const cell = view.overview?.cell ?? view.solar?.cell ?? null;
  const area = cell
    ? (() => {
      const r = measureOf(cell);
      return { left: r.left, top: r.top,
        right: r.left + r.width, bottom: r.top + r.height };
    })()
    : windowsAreaOf(workspaceOf(id));
  let clip = null;
  if (area) {
    /* Intersect on screen, where the window is drawn — with a scale in effect
       the measured rect is much smaller than the window's real size, so
       intersecting the real size against screen coordinates would clip almost
       everything away and, at zero width, hide the window entirely.

       The result is then converted back into the window's own coordinates,
       which is the space the compositor expects and the only one that means
       anything to the client's buffer. */
    /* Against the inset rect, not the measured one, and inwards again at the
       output edge for the same reason: a clip that reaches half a pixel past
       the hole hands the overhang straight back. */
    const left = Math.max(inset.left, Math.ceil(area.left));
    const top = Math.max(inset.top, Math.ceil(area.top));
    const right = Math.min(inset.right, Math.floor(area.right));
    const bottom = Math.min(inset.bottom, Math.floor(area.bottom));

    clip = {
      x: Math.round(box.x + (left - inset.left) / scale),
      y: Math.round(box.y + (top - inset.top) / scale),
      width: Math.max(0, Math.round((right - left) / scale)),
      height: Math.max(0, Math.round((bottom - top) / scale)),
    };
  }

  /* The frame the shell drew around a floating window, so the compositor can
     draw that piece of the shell above the windows underneath it.
     
     Everything the shell paints is under every client surface — the windows
     are holes in one buffer — so a border that falls inside another window's
     hole is covered by that client. A tiled border never does: it sits in the
     gap between two windows, where there is no surface to hide it. A floating
     window is the case where it does, every time. */
  /* Lifted: above the windows it overlaps, rather than merely somewhere on the
     workspace. Floating is the usual reason and solar's sun is the other one —
     it sits over its own orbits by design, and its border falls inside their
     holes exactly as a dialog's falls inside the window beneath it. The
     canvas's focused window is the third: windows on a plane overlap freely,
     so the one being typed into has to be the one in front. */
  const lifted = isFloating(id) || view.solar?.lift === true
    || view.canvas?.lift === true;

  /* Bounded by the output, exactly as the clip above is, and for a sharper
     reason than tidiness.

     `.desktop` is `overflow: hidden`, so the page never paints a window's
     frame past the edge of the monitor it is on — but getBoundingClientRect
     measures the element, not what was painted of it, so a window dragged
     towards the next screen measures a frame that reaches onto it. The
     compositor takes that rectangle and draws *the shell's pixels there* above
     the windows. On the neighbouring monitor those pixels are that monitor's
     own desktop, borders and all, which is how dragging a window off the right
     of DP-1 put a strip of DP-2's window frames over DP-2's windows.

     So the frame reported is the frame drawn: intersected with the area, and
     dropped when nothing of it is left. */
  const frameEl = lifted && view.el ? measureOf(view.el) : null;
  let frame = frameEl
    ? {
      x: Math.round(frameEl.left),
      y: Math.round(frameEl.top),
      width: Math.round(frameEl.width),
      height: Math.round(frameEl.height),
    }
    : null;
  if (frame && area) {
    const left = Math.max(frame.x, Math.round(area.left));
    const top = Math.max(frame.y, Math.round(area.top));
    const right = Math.min(frame.x + frame.width, Math.round(area.right));
    const bottom = Math.min(frame.y + frame.height, Math.round(area.bottom));
    frame = right > left && bottom > top
      ? { x: left, y: top, width: right - left, height: bottom - top }
      : null;
  }

  /* Stacking is the compositor's — the space it keeps is what it draws from
     and what it tests a click against — and floating is the one thing about a
     window's stacking that only the shell knows. Without it a click on a tiled
     window raises that window over the dialog sitting on top of it.

     The compositor offers exactly two bands — restack() raises everything
     marked floating above everything else and stops there — so this is also
     how solar keeps its sun in front: the sun is lifted, the orbits are not,
     and the one window being typed into is never occluded. */
  const floating = lifted;

  /* Smart radius: whether this window is being drawn square. The stylesheet
     rounds the frame and the compositor crops the client, so the second has to
     be told what the first decided — the class above is CSS, and CSS is not
     something the compositor can read. */
  const square = !overviewActive && smartRadius()
    && singleWindowOn(workspaceOf(id));

  const prev = view.box;
  const prevClip = view.clip;
  const prevFrame = view.frame;
  const prevFloating = view.reportedFloating;
  const prevSquare = view.reportedSquare;
  if (prev && prev.x === box.x && prev.y === box.y &&
      prev.width === box.width && prev.height === box.height &&
      prev.scale === scale && sameBox(prevClip, clip) &&
      sameBox(prevFrame, frame) && prevFloating === floating &&
      prevSquare === square) {
    return false;
  }

  view.box = { ...box, scale };
  view.clip = clip;
  view.frame = frame;
  view.reportedFloating = floating;
  view.reportedSquare = square;

  const message = { type: 'view.layout', id, ...box };
  if (scale !== 1) message.scale = scale;
  if (clip) message.clip = clip;
  if (frame) message.frame = frame;
  if (floating) message.floating = true;
  if (square) message.square = true;
  /* Anything that gets past the comparison above differs, so this goes out
     unconditionally — including the case where the scale alone changed, which
     is worth a message even though the rect did not move. */
  send(message);
  return true;
}

/* Keep reporting geometry while the layout is still moving.
 *
 * Window frames are CSS, so they animate for free — but a window's *contents*
 * are a real surface the compositor draws at whatever rect it was last told.
 * Sampling once after a relayout would slide the frame smoothly and snap the
 * contents straight to the destination, which looks worse than no animation at
 * all. So geometry is resampled every frame until it stops changing.
 *
 * Self-terminating rather than tied to transition events: transitions get
 * interrupted, replaced and cancelled constantly while dragging, and a missed
 * `transitionend` would leave a window stranded mid-flight. A few extra frames
 * of sampling after everything settles is the cheaper mistake. */
const PUMP_IDLE_FRAMES = 3;
/* Hard ceiling, about a second. Rounding can leave two windows disagreeing by
 * a pixel forever, and a permanently spinning frame callback that sends IPC on
 * every tick is a worse bug than a window settling a frame late. */
const PUMP_MAX_FRAMES = 60;
let pumpRemaining = 0;
let pumping = false;

/* Animate the move from an old layout to a new one, by inverting it.
 *
 * The tree is rebuilt from scratch on every relayout, so a window's new
 * position arrives as a fresh set of parent elements — there is no property
 * change on a retained element for CSS to transition, which is why simply
 * declaring a transition animated nothing at all. The window elements
 * themselves *are* retained though, so the standard trick works: measure where
 * each one was, let the new layout land, then offset it back to where it came
 * from and release it.
 *
 * Position only, deliberately. A window's contents are a real surface, and the
 * compositor follows the frame by resizing the client — so animating size would
 * ask every client to relayout itself sixty times a second for the duration.
 * Sliding costs nothing extra; growing would cost a great deal. */
function flipFrom(before) {
  if (reducedMotion()) {
    /* Still has to reach its destination, just without the journey. */
    releaseStrips();
    return;
  }

  const strips = pendingStrips();

  /* Measure every window, then move every window — rather than measuring and
     moving one at a time.
   *
   * Interleaved, each `style.transform` invalidates the layout the next
   * `getBoundingClientRect` needs, so a relayout of eight windows is eight
   * layouts instead of one. That is a wasted millisecond in a browser whose
   * layout is a function call and a great deal more in one where it is a
   * thread: this is the same read-then-write split that the forced reflow
   * below is careful about, applied to the loop that precedes it. */
  const offsets = [];
  for (const [id, view] of views) {
    if (view.el.hidden) continue;
    const from = before.get(id);
    if (!from) continue; // was hidden or is new: nothing to animate from

    const to = view.el.getBoundingClientRect();
    const dx = from.left - to.left;
    const dy = from.top - to.top;
    if (dx === 0 && dy === 0) continue;

    offsets.push([view.el, dx, dy]);
  }

  const moved = [];
  for (const [el, dx, dy] of offsets) {
    el.classList.add('flipping');
    el.style.transform = `translate(${dx}px, ${dy}px)`;
    moved.push(el);
  }
  if (moved.length === 0 && strips.length === 0) return;

  /* One forced reflow for the whole batch, so the browser sees the offset
     position before the transition is re-enabled. Reading a layout property
     per element would do the same thing N times over. */
  void document.documentElement?.offsetWidth;

  for (const el of moved) {
    el.classList.remove('flipping');
    el.style.transform = '';
  }
  releaseStrips();
}

/* Strips that were rendered at their previous scroll position and still need
 * to be moved to their new one. */
function pendingStrips() {
  const found = [];
  for (const output of outputs.values()) {
    const strip = output.windowsEl.children[0];
    if (strip?.classList?.contains('strip')) found.push(strip);
  }
  return found;
}

function releaseStrips() {
  for (const strip of pendingStrips()) {
    const target = Number(strip.dataset.scroll);
    if (Number.isFinite(target)) {
      strip.style.transform = `translateX(${-target}px)`;
    }
  }
}

/* Measure every window on screen and tell the compositor where it is.
 *
 * One layout for the whole pass. Nothing in here writes to the DOM — a report
 * is a measurement and a message — so every window measured after the first is
 * answered from the same layout rather than asking for one of its own.
 *
 * Returns whether anything had moved since the last pass, which is what
 * pumpGeometry reads to decide the layout is still going. */
function reportAllGeometry() {
  let changed = false;
  beginMeasurePass();
  try {
    for (const [id, view] of views) {
      if (!view.el.hidden && reportGeometry(id)) changed = true;
    }
  } finally {
    endMeasurePass();
  }
  return changed;
}

function pumpGeometry() {
  pumpRemaining = PUMP_IDLE_FRAMES;
  if (pumping) return;
  pumping = true;

  let budget = PUMP_MAX_FRAMES;

  const step = () => {
    const changed = reportAllGeometry();

    pumpRemaining = changed ? PUMP_IDLE_FRAMES : pumpRemaining - 1;
    if (pumpRemaining > 0 && --budget > 0) {
      requestAnimationFrame(step);
    } else {
      pumping = false;
    }
  };

  requestAnimationFrame(step);
}

/* A gesture in progress: a window being dragged or resized by hand, or the
 * canvas being panned. All three arrive from the compositor as a stream of
 * deltas rather than as one event, and all three want the same thing.
 *
 * One relayout per frame. A mouse reporting every few milliseconds asks the
 * shell to lay the desktop out more often than the screen is drawn, and a
 * relayout is not cheap: the tree is rebuilt, every window is measured twice
 * and a layout is forced. Doing that two or three times between frames does
 * not move the window any further — only the last pass before the paint is
 * ever seen — but it does spend the time that frame needed, which is what a
 * drag that stutters is made of. The deltas accumulate in the layout's own
 * state (a rect on the plane, a floating window's x and y), so coalescing
 * loses nothing: the frame draws wherever the pointer has got to.
 *
 * Ends on the button release, which the compositor sends as `layout.drag.end`
 * — the gesture is swallowed there, so the release is not something the shell
 * could see for itself. The timer is the fallback for the gesture that ends
 * without one: a VT switch takes the pointer away and no button is ever
 * reported up. The same 120ms the dragged window's own class uses. */
const GESTURE_IDLE_MS = 120;
let gesturing = false;
let gestureTimer = null;
let relayoutFrame = false;

/* Whether a drag, a resize or a pan is under way. Read by relayoutAll, which
 * neither animates nor waits a frame to report geometry while one is. */
function isGesturing() {
  return gesturing;
}

/* The button came up: the compositor says so on the release that ends the
 * gesture it has been swallowing. The timer above is the fallback for a
 * gesture that ends without one — a VT switch takes the pointer away and no
 * release is ever reported. */
function endGesture() {
  if (!gesturing) return;
  gesturing = false;
  clearTimeout(gestureTimer);
  gestureTimer = null;
  relayoutAll();
}

/* One relayout for this frame, however many deltas arrive before it. */
function gestureRelayout() {
  gesturing = true;
  clearTimeout(gestureTimer);
  gestureTimer = setTimeout(() => {
    gesturing = false;
    /* Once more with the ordinary rules, now that the hand has stopped. The
       layout is already where it belongs, so nothing moves — what this
       restores is the animation and the FLIP for whatever happens next. */
    relayoutAll();
  }, GESTURE_IDLE_MS);

  if (relayoutFrame) return;
  relayoutFrame = true;
  requestAnimationFrame(() => {
    relayoutFrame = false;
    relayoutAll();
  });
}

/* Whether the user has asked for less motion. Checked at the point of use so
 * changing the setting takes effect without a reload. */
function reducedMotion() {
  return typeof matchMedia === 'function' &&
    matchMedia('(prefers-reduced-motion: reduce)').matches;
}

/* Fade a newly opened window in.
 *
 * This one cannot be CSS. The frame is the shell's, but the window's contents
 * are a surface the compositor draws, and no style here touches it — so the
 * opacity is tweened in JS and sent over IPC, where the compositor applies it
 * to the surface itself. Skipped entirely when motion is reduced.
 *
 * What is left here is the part that is about windows: where the fade stops,
 * and what to do when there is not going to be one. The tween itself is
 * surfaceOpacityTween in motion.js, along with the rest of the shell's
 * hand-driven animation.
 *
 * Returns whether a tween was actually started, which is what lets a caller
 * that also has an opinion about this window's opacity — solar, whose tiers
 * rest well below 1 — leave a fade in progress alone rather than talking over
 * it. */
function fadeIn(id) {
  /* Where the fade stops. Usually opaque, but the solar layout rests its cold
     windows well below that — and a fade that always ended at 1 would bring
     every one of them up bright and leave it bright until something else
     caused a relayout, which is a window that looks focused and is not. */
  const resting = typeof solarRestingOpacity === 'function'
    ? solarRestingOpacity(id) : 1;

  if (reducedMotion()) {
    /* No journey, but it still has to arrive. Reduced motion asks for nothing
       to move, not for every cold window to open at full brightness and stay
       there until something else happens to cause a relayout. */
    if (resting !== 1) send({ type: 'view.opacity', id, opacity: resting });
    return false;
  }

  send({ type: 'view.opacity', id, opacity: 0 });

  const started = surfaceOpacityTween(resting, (opacity) => {
    send({ type: 'view.opacity', id, opacity });
  });

  /* The window was just told to be invisible. If no tween is going to bring it
     back up — a shell whose tween engine did not load is the way that happens
     — this has to, or the fade that never started is a window that never
     appears. */
  if (!started) send({ type: 'view.opacity', id, opacity: resting });
  return started;
}

/* Fade in every window a relayout has just brought on screen.
 *
 * Switching workspace replaced one set of windows with another between two
 * frames: the departing ones vanished and the arriving ones were simply there,
 * which is the one moment on this desktop where nothing moves and everything
 * changes.
 *
 * Only the arrivals are faded. Fading the departures out would mean keeping
 * them rendered after the workspace stopped being the one on screen, and two
 * workspaces' windows visible at once is a worse thing to look at than a
 * switch that is merely quick — the same reason the tabbed containers do not
 * animate their switch either.
 *
 * `before` is the set of windows that were on screen; anything rendered now
 * and missing from it has just arrived. Taking the difference rather than
 * fading everything matters on a switch between two workspaces that share a
 * window, which stays put and should not blink. */
function fadeInArrivals(before) {
  if (reducedMotion()) return;
  for (const id of renderedIds) {
    if (!before.has(id)) fadeIn(id);
  }
}

/* How long the class below is left on. It only has to outlast the animation
 * shell.css runs off it: once that has finished the element is already showing
 * its resting style, so removing the class late changes nothing on screen and
 * removing it early would cut the animation off. Generous on purpose, because
 * the duration on the CSS side comes from a custom property a theme can
 * change and this number cannot follow it. */
const FLARE_MS = 400;

/* The focus ring arriving, rather than appearing.
 *
 * Driven from here and not from `.window.focused` in the stylesheet, and that
 * is the whole point of it being in JavaScript: relayoutAll() re-renders the
 * tree, a divider drag runs one per mousemove, and an animation hung on a
 * state restarts every time its element is rendered again. Attached to the
 * moment focus actually moves, it runs once per focus change and nothing else
 * can trigger it.
 *
 * It animates a one-pixel outline and nothing else. No rect changes, so
 * nothing is remeasured and the compositor is told nothing at all. */
function flareFocus(el) {
  if (reducedMotion()) return;
  /* Only when it is already running, which is alt-tabbing back and forth
     inside the duration: adding a class an element already has is not a
     change, so without this the second flare would not happen. The reflow is
     what makes the removal take effect before the class goes back on, and it
     is guarded because forcing a layout on every focus change to cover a case
     that is usually not true is a poor trade — flipFrom already forces one per
     relayout and does not need a second. */
  if (el.classList.contains('focus-flare')) {
    el.classList.remove('focus-flare');
    void el.offsetWidth;
  }
  el.classList.add('focus-flare');
  setTimeout(() => el.classList.remove('focus-flare'), FLARE_MS);
}

function sameBox(a, b) {
  if (a === b) return true;
  if (!a || !b) return false;
  return a.x === b.x && a.y === b.y &&
    a.width === b.width && a.height === b.height;
}

const resizeObserver = new ResizeObserver((entries) => {
  for (const entry of entries) {
    const id = Number(entry.target.dataset.viewId);
    if (Number.isFinite(id)) reportGeometry(id);
  }
});

function relayoutAll() {
  /* A dynamic tiling mode derives the shape from which windows are open, so
     the tree may be out of date before anything is measured. Cheap when
     nothing changed: it compares the window set against what it last built
     for and returns. */
  arrangeAll();

  /* Solar positions absolutely and dims what is not in focus, neither of which
     any other layout would undo — a window left carrying an orbit's left/top
     would be placed in the middle of a tiling column, and one left at 0.4
     would stay dim for the rest of the session. Cheap when there is nothing to
     undo. */
  if (layoutMode !== 'solar' || overviewActive) clearSolarState();

  /* The matrix positions absolutely and hides what is buried in its deepest
     slot, neither of which any other layout would undo: a window left with a
     slot's left/top would sit in the middle of a tiling column, and one left
     hidden would stay invisible for the rest of the session. */
  if (layoutMode !== 'matrix' || overviewActive) clearMatrixState();

  /* The canvas positions absolutely for the same reason both of those do, and
     leaves the same inline rect behind: a window carrying a place on the plane
     would sit in the middle of a tiling column. Its *places* survive — see
     clearCanvasState — so leaving the canvas and coming back finds the plane
     as it was left. */
  if (layoutMode !== 'canvas' || overviewActive) clearCanvasState();

  /* Every output's orbits, worked out in one pass before any of them is drawn.
     A Lagrange field puts one monitor's cold windows on another, and the
     outputs are rendered in whatever order the map is in, so the companion has
     to already know what it is holding by the time its turn comes. */
  const solar = (layoutMode === 'solar' && !overviewActive)
    ? planSolar() : null;

  /* The matrix's rectangles, for every output at once. One pass rather than
     one per output as it is drawn, so that whether a monitor is empty is
     answerable from the plan rather than from how far the drawing has got. */
  const matrix = (layoutMode === 'matrix' && !overviewActive)
    ? planMatrix() : null;

  /* And the canvas's, in one pass for the same reason: placing a window that
     has never been placed reads the viewport of the workspace it is on, so
     every output's plan has to be settled before any of them is drawn. */
  const canvas = (layoutMode === 'canvas' && !overviewActive)
    ? planCanvas() : null;

  /* Where everything was, before the tree is thrown away and rebuilt.
     Not while a gesture is running: a window under the hand is not animated
     from where it was a delta ago — see flipFrom's call site below — so the
     measurement would be a forced layout per window per frame for a slide
     nothing is going to play. */
  const before = new Map();
  if (!isGesturing()) {
    for (const [id, view] of views) {
      if (!view.el.hidden) before.set(id, view.el.getBoundingClientRect());
    }
  }

  /* workspace -> output showing it. A workspace appears at most once. */
  const shown = new Map();
  for (const [name, output] of outputs) shown.set(output.workspace, name);

  /* Render first, then decide what is visible. Which windows are on screen is
     now a result of rendering rather than something knowable in advance: a
     collapsed tab, or a column scrolled off the strip, is on its workspace and
     still not shown. */
  renderedIds = new Set();

  if (overviewActive) {
    clearOverviewState();
  }
  const assignment = overviewActive ? overviewAssignment() : null;

  for (const [name, output] of outputs) {
    const root = workspaces.get(output.workspace);
    /* Only one output shows the overview: a window element exists once in the
       DOM, so two grids would fight over the same windows and the second would
       simply steal them. The others go blank for the duration. */
    /* Solar is asked for the output rather than for the root: what it draws
       here is not this workspace's tree but this monitor's share of the plan,
       which in a Lagrange field includes windows belonging to the workspace on
       the other screen. */
    const rendered = overviewActive
      ? renderOverview(output, assignment.get(name) ?? [])
      : (solar
        ? renderSolar(solar.get(name) ?? [], output)
        : (matrix
          ? renderMatrix(matrix.get(name) ?? [], output)
          : (canvas
            ? renderCanvas(canvas.get(name) ?? [], output)
            : (root
              ? (layoutMode === 'scrolling'
                ? renderStrip(root, output)
                : renderTree(root))
              : null))));

    output.windowsEl.replaceChildren();
    output.windowsEl.classList.toggle('scrolling', layoutMode === 'scrolling');
    output.windowsEl.classList.toggle('solar', layoutMode === 'solar');
    output.windowsEl.classList.toggle('matrix', layoutMode === 'matrix');
    output.windowsEl.classList.toggle('canvas', layoutMode === 'canvas');
    /* Everything this output draws follows the hand directly while a gesture
       is running. The dragged window has a class of its own and has had one
       for as long as dragging has worked; this is for the windows it is not.
       Panning the canvas moves every window on the plane, and a resize drag
       moves the neighbour the divider is being taken from — both of those
       were easing toward the pointer over the animation duration, which is
       the whole plane sliding along behind the cursor. */
    output.windowsEl.classList.toggle('gesture', isGesturing());
    /* Smart gaps: a lone window on the workspace drops the inner gap, and the
       CSS padding has to match what edgeGapPx() (used by the layout math)
       computes, or the drawn edge and the measured one drift apart. */
    output.windowsEl.classList.toggle('smart-single',
      gapsSmart && singleWindowOn(output.workspace));
    /* Smart radius: the same lone window, drawn square. A separate class from
       the one above because the two settings can be set apart — and off in the
       overview, where the windows are thumbnails whose corners are the
       thumbnail's, not the desktop's. */
    output.windowsEl.classList.toggle('smart-square',
      !overviewActive && smartRadius() && singleWindowOn(output.workspace));
    if (rendered) output.windowsEl.append(rendered);

    /* Floating windows are positioned rather than laid out, so they are
       appended after the tree and take their rect from their own record. CSS
       lifts them above the tiled windows; the compositor stacks the real
       surfaces to match when it is told their new rects. */
    for (const [id, floating, view] of floatingEntries()) {
      if (overviewActive) break; // thumbnails place their own
      /* And the canvas places its own, floating included: on a plane every
         window is placed by hand at a rectangle of its own, so there is nothing
         for "floating" to mean and nothing here to add. Writing the rect again
         would nail the window to the screen — the rect is in screen
         coordinates, so the window would sit still while the plane panned
         underneath it. */
      if (canvas) break;
      if (floating.workspace !== output.workspace) continue;
      view.el.classList.add('floating');
      output.windowsEl.append(view.el);
      renderedIds.add(id);
      if (isFullscreen(id)) continue; // covers the output; rect ignored
      Object.assign(view.el.style, {
        left: `${floating.x}px`,
        top: `${floating.y}px`,
        width: `${floating.width}px`,
        height: `${floating.height}px`,
        flexGrow: '',
      });
    }

    /* A monitor holding another workspace's Lagrange field has nothing of its
       own on it and is not empty, so the placeholder is answered by what was
       drawn rather than by whose workspace it was. */
    output.emptyEl.hidden = idsOf(output.workspace).length > 0
      || (solar?.get(name)?.length ?? 0) > 0;

    /* A fullscreen window covers the whole output, bar included — that is what
     * fullscreen means, and a video with a status bar across the top is not
     * fullscreen. The bar also stays hidden while explicitly toggled off. */
    const fullscreenHere = fullscreenOn(output.workspace) !== null;
    output.el.classList.toggle('overview-active', overviewActive);
    output.el.classList.toggle('has-fullscreen',
      fullscreenHere && !overviewActive);
    /* Under 'auto' the bar is hidden except while Mod4 is held; the per-output
       toggle (Mod4+n) still wins, so a bar someone asked for does not vanish
       the moment they let go of the key that revealed it. */
    const hidden = barMode === 'auto'
      ? (output.barHidden && !logoHeld)
      : output.barHidden;
    /* The reveal is an edge, not a state, and this runs on every relayout, so
       what the bar was doing last time is remembered rather than read back off
       the element — the first pass over a new output finds no `bar-hidden`
       class on it and has to count as an arrival all the same.

       An edge is what there is to work with: `display` cannot be transitioned
       and an element coming from display:none has no previous style to move
       from, so the entrance has to be started by whatever did the revealing.

       Both ways of hiding it count. `bar-hidden` is the toggle and the auto
       mode; fullscreen covers the output, bar included, through a class of its
       own — and coming back out of a fullscreen video is the bar arriving
       exactly as much as pressing Mod4+n is. */
    const onScreen = !hidden && !(fullscreenHere && !overviewActive);
    const revealed = onScreen && !output.barShown;
    output.barShown = onScreen;
    output.el.classList.toggle('bar-hidden', hidden);
    if (revealed) animateBarIn(output);
    /* Auto draws the bar over the windows rather than above them, so revealing
       it does not resize anything. */
    output.el.classList.toggle('bar-auto', barMode === 'auto');
    /* And the compositor has to be told, or it is drawn over nothing. The
       shell is one buffer *under* the clients: a bar given no room of its own
       lands behind every window on the output, which looks like it covers
       their borders — those are drawn by the shell — and stops dead at the
       edge of the client's own surface. Naming the rectangle gets this buffer
       drawn again, cropped to the bar, in front.
       Only under 'auto': the other modes reserve space, so nothing is over
       anything and there is nothing to lift.

       And it takes the pointer, which it used to decline.
     *
     * Declining was for the windows underneath: the bar is revealed by Mod4,
     * Mod4 is what a window is dragged and resized with, and a bar that took
     * those left a window moved up under it unable to be grabbed there. What
     * it cost was every click the bar itself exists to receive — a workspace
     * pill, a window's title in the taskbar, a widget — because under 'auto'
     * the bar is on screen *only* while Mod4 is held, so a click on it always
     * arrives with that modifier down. A bar that cannot be clicked is not a
     * trade, it is a strip of decoration.
     *
     * So the bar takes the pointer and the compositor declines the gesture
     * over it instead: a Mod4 press on anything the shell drew in front is a
     * click on that thing rather than a drag of what is behind it. The cost is
     * the top few pixels of a window that has been moved under the bar, which
     * can still be grabbed anywhere else in it. */
    const barFloats = barMode === 'auto' && onScreen;
    setOverlay(`bar:${name}`, barFloats ? output.barEl : null);
    renderBar(name);
  }

  /* Windows a fade was just started on, so that anything else with an opinion
     about their opacity does not talk over the tween. */
  const faded = new Set();

  for (const [id, view] of views) {
    const workspace = workspaceOf(id);
    /* Normally a window is on screen only if its workspace is: `shown` maps
       each displayed workspace to the output showing it. The overview breaks
       that rule on purpose — it draws every workspace at once, including the
       ones no monitor is displaying — so there the thumbnail's own render is
       the whole answer. Without this exception a window on a workspace that
       happened to be off screen stayed hidden, and its thumbnail came out
       labelled empty. */
    const visible = overviewActive
      ? renderedIds.has(id)
      : (workspace !== null && shown.has(workspace) && renderedIds.has(id));

    /* A window that was not on screen a moment ago fades in exactly as a
       newly opened one does, and for the same reason: its contents are a
       surface the compositor draws, so the only way to bring it up gently is
       to tween the opacity over IPC. This is what gives a workspace switch
       some motion — nothing else about it moves, the outgoing windows are
       hidden and the incoming ones simply exist — and it is what makes the
       overview arrive as miniatures appearing rather than as a grid that was
       always there.

       Bounded by the same 120ms as an opening window, and sampled at the same
       30Hz, so switching to a workspace holding four windows costs the
       compositor four surface-tree walks per sample and then stops. A window
       already on screen is not touched, so a relayout that moves things about
       fades nothing. */
    const wasVisible = !view.el.hidden;
    view.el.hidden = !visible;
    if (visible && !wasVisible && fadeIn(id)) faded.add(id);
    if (!visible && view.box !== null) {
      view.box = null;
      send({ type: 'view.visible', id, visible: false });
    }
    /* Before the class is set, so this reads the previous frame's answer: the
       flare belongs to focus moving, not to the window being re-rendered while
       it happens to hold focus. */
    if (id === focusedId && !view.el.classList.contains('focused')) {
      flareFocus(view.el);
    }
    view.el.classList.toggle('focused', id === focusedId);
    view.el.classList.toggle('selected', selectedIds.has(id));
    view.el.classList.toggle('fullscreen', isFullscreen(id));
  }

  /* The orbits' resting opacities, now that it is settled which windows are on
     screen and which of those are already fading in from zero. Sending them
     from renderSolar instead would put a window's resting value on the wire
     immediately before the fade set it back to zero, which is a window that
     flashes at its final brightness on the frame it opens. */
  if (solar) settleSolarOpacity(faded);

  /* Offset every window back to where it was and let it slide into place.
     Except under the hand: a drag is already a window following a pointer at
     the pointer's own speed, and a slide toward each frame's destination is
     the ice `.window.dragging` exists to prevent — applied to everything else
     the gesture moved, which on the canvas is the whole plane when it pans.
     `releaseStrips` still has to run, exactly as on the reduced-motion path:
     it is what puts a scrolled strip at its new offset rather than animating
     it there. */
  if (isGesturing()) releaseStrips();
  else flipFrom(before);

  /* The workspace set as it now stands, for anything outside the shell that
     asked to be told through `ext-workspace-v1` — an external bar's workspace
     buttons are drawn from this and nothing else. Here because everything that
     changes the workspaces ends in a relayout: a switch, a window opening, the
     last one on a workspace closing. Cheap when nothing changed; it sends only
     when the list differs from the one last sent. */
  publishWorkspaces();

  /* Measure after the browser has laid the new tree out, and keep measuring
     for as long as it is still moving. */
  /* Under the hand, measure now rather than next frame. The pump samples on
     an animation frame, so a relayout that already runs on one reports the
     drag a whole frame after the frame that drew it — the shell's border
     under the pointer and the client's picture one step behind it, which is
     the drag lagging its own window. The forced layout this costs is the one
     the FLIP above would have forced anyway and no longer does. */
  if (isGesturing()) reportAllGeometry();
  pumpGeometry();
}

