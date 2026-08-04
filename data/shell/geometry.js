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
function windowsAreaOf(workspace) {
  const name = hostOfWorkspace(workspace);
  const output = name !== null ? outputs.get(name) : null;
  if (!output) return null;

  const rect = output.windowsEl.getBoundingClientRect();
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

  const rect = view.viewport.getBoundingClientRect();
  /* In the overview the element is inside a scaled container, so the measured
     rect is where it appears but not the size the client should be. The
     compositor is told the real size and the factor to draw it at.

     The solar layout's outer orbit does the same thing for the same reason: a
     cold window keeps the size of a warm one and is merely drawn small, so
     that a focus change — which reshuffles every orbit on the workspace — does
     not reconfigure half the clients on it. See solar.js. */
  const scale = view.overview?.scale ?? view.solar?.scale ?? 1;
  const box = {
    x: Math.round(rect.left),
    y: Math.round(rect.top),
    width: Math.round(rect.width / scale),
    height: Math.round(rect.height / scale),
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
      const r = cell.getBoundingClientRect();
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
    const left = Math.max(rect.left, area.left);
    const top = Math.max(rect.top, area.top);
    const right = Math.min(rect.left + rect.width, area.right);
    const bottom = Math.min(rect.top + rect.height, area.bottom);

    clip = {
      x: Math.round(box.x + (left - rect.left) / scale),
      y: Math.round(box.y + (top - rect.top) / scale),
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
     holes exactly as a dialog's falls inside the window beneath it. */
  const lifted = isFloating(id) || view.solar?.lift === true;

  const frameEl = lifted ? view.el?.getBoundingClientRect() : null;
  const frame = frameEl
    ? {
      x: Math.round(frameEl.left),
      y: Math.round(frameEl.top),
      width: Math.round(frameEl.width),
      height: Math.round(frameEl.height),
    }
    : null;

  /* Stacking is the compositor's — the space it keeps is what it draws from
     and what it tests a click against — and floating is the one thing about a
     window's stacking that only the shell knows. Without it a click on a tiled
     window raises that window over the dialog sitting on top of it.

     The compositor offers exactly two bands — restack() raises everything
     marked floating above everything else and stops there — so this is also
     how solar keeps its sun in front: the sun is lifted, the orbits are not,
     and the one window being typed into is never occluded. */
  const floating = lifted;

  const prev = view.box;
  const prevClip = view.clip;
  const prevFrame = view.frame;
  const prevFloating = view.reportedFloating;
  if (prev && prev.x === box.x && prev.y === box.y &&
      prev.width === box.width && prev.height === box.height &&
      prev.scale === scale && sameBox(prevClip, clip) &&
      sameBox(prevFrame, frame) && prevFloating === floating) {
    return false;
  }

  view.box = { ...box, scale };
  view.clip = clip;
  view.frame = frame;
  view.reportedFloating = floating;

  const message = { type: 'view.layout', id, ...box };
  if (scale !== 1) message.scale = scale;
  if (clip) message.clip = clip;
  if (frame) message.frame = frame;
  if (floating) message.floating = true;
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
  const moved = [];
  for (const [id, view] of views) {
    if (view.el.hidden) continue;
    const from = before.get(id);
    if (!from) continue; // was hidden or is new: nothing to animate from

    const to = view.el.getBoundingClientRect();
    const dx = from.left - to.left;
    const dy = from.top - to.top;
    if (dx === 0 && dy === 0) continue;

    view.el.classList.add('flipping');
    view.el.style.transform = `translate(${dx}px, ${dy}px)`;
    moved.push(view.el);
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

function pumpGeometry() {
  pumpRemaining = PUMP_IDLE_FRAMES;
  if (pumping) return;
  pumping = true;

  let budget = PUMP_MAX_FRAMES;

  const step = () => {
    let changed = false;
    for (const [id, view] of views) {
      if (!view.el.hidden && reportGeometry(id)) changed = true;
    }

    pumpRemaining = changed ? PUMP_IDLE_FRAMES : pumpRemaining - 1;
    if (pumpRemaining > 0 && --budget > 0) {
      requestAnimationFrame(step);
    } else {
      pumping = false;
    }
  };

  requestAnimationFrame(step);
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

  /* Where everything was, before the tree is thrown away and rebuilt. */
  const before = new Map();
  for (const [id, view] of views) {
    if (!view.el.hidden) before.set(id, view.el.getBoundingClientRect());
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
          : (root
            ? (layoutMode === 'scrolling'
              ? renderStrip(root, output)
              : renderTree(root))
            : null)));

    output.windowsEl.replaceChildren();
    output.windowsEl.classList.toggle('scrolling', layoutMode === 'scrolling');
    output.windowsEl.classList.toggle('solar', layoutMode === 'solar');
    output.windowsEl.classList.toggle('matrix', layoutMode === 'matrix');
    if (rendered) output.windowsEl.append(rendered);

    /* Floating windows are positioned rather than laid out, so they are
       appended after the tree and take their rect from their own record. CSS
       lifts them above the tiled windows; the compositor stacks the real
       surfaces to match when it is told their new rects. */
    for (const [id, floating, view] of floatingEntries()) {
      if (overviewActive) break; // thumbnails place their own
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
       anything and there is nothing to lift. */
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

  /* Offset every window back to where it was and let it slide into place. */
  flipFrom(before);

  /* Measure after the browser has laid the new tree out, and keep measuring
     for as long as it is still moving. */
  pumpGeometry();
}

