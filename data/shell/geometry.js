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
     compositor is told the real size and the factor to draw it at. */
  const scale = view.overview?.scale ?? 1;
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
  /* In the overview a window is bounded by its thumbnail, not by the output. */
  const cell = view.overview?.cell;
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
  const frameEl = isFloating(id) ? view.el?.getBoundingClientRect() : null;
  const frame = frameEl
    ? {
      x: Math.round(frameEl.left),
      y: Math.round(frameEl.top),
      width: Math.round(frameEl.width),
      height: Math.round(frameEl.height),
    }
    : null;

  const prev = view.box;
  const prevClip = view.clip;
  const prevFrame = view.frame;
  if (prev && prev.x === box.x && prev.y === box.y &&
      prev.width === box.width && prev.height === box.height &&
      prev.scale === scale && sameBox(prevClip, clip) &&
      sameBox(prevFrame, frame)) {
    return false;
  }

  view.box = { ...box, scale };
  view.clip = clip;
  view.frame = frame;

  const message = { type: 'view.layout', id, ...box };
  if (scale !== 1) message.scale = scale;
  if (clip) message.clip = clip;
  if (frame) message.frame = frame;
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
 * to the surface itself. Short, and skipped entirely when motion is reduced. */
const FADE_MS = 120;

function fadeIn(id) {
  if (reducedMotion()) return;

  send({ type: 'view.opacity', id, opacity: 0 });

  const started = performance.now();
  let frame = 0;
  const step = (now) => {
    const t = Math.min((now - started) / FADE_MS, 1);
    /* Sampled every other frame. The compositor answers each view.opacity by
       walking the window's whole surface tree with
       wlr_scene_node_for_each_buffer(), so the price of the tween is one tree
       walk per sample per window opened — and over 120ms a 30Hz ramp is
       indistinguishable from a 60Hz one. The value cannot simply be left to
       the compositor to interpolate: it holds whatever it was last told and
       animates nothing, so the intermediate samples *are* the fade.
       The final sample is never skipped, because it is the one that leaves the
       window fully opaque. */
    if (t === 1 || ++frame % 2 === 0) {
      /* Ease-out, matching the CSS curve closely enough that a window opening
         beside one that is moving does not look like a different animation. */
      send({ type: 'view.opacity', id, opacity: 1 - Math.pow(1 - t, 3) });
    }
    if (t < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
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
    const rendered = overviewActive
      ? renderOverview(output, assignment.get(name) ?? [])
      : (root
        ? (layoutMode === 'scrolling'
          ? renderStrip(root, output)
          : renderTree(root))
        : null);

    output.windowsEl.replaceChildren();
    output.windowsEl.classList.toggle('scrolling', layoutMode === 'scrolling');
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

    output.emptyEl.hidden = idsOf(output.workspace).length > 0;

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
    output.el.classList.toggle('bar-hidden', hidden);
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
    const barFloats = barMode === 'auto'
      && !hidden
      && !(fullscreenHere && !overviewActive);
    setOverlay(`bar:${name}`, barFloats ? output.barEl : null);
    renderBar(name);
  }

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

    view.el.hidden = !visible;
    if (!visible && view.box !== null) {
      view.box = null;
      send({ type: 'view.visible', id, visible: false });
    }
    view.el.classList.toggle('focused', id === focusedId);
    view.el.classList.toggle('selected', selectedIds.has(id));
    view.el.classList.toggle('fullscreen', isFullscreen(id));
  }

  /* Offset every window back to where it was and let it slide into place. */
  flipFrom(before);

  /* Measure after the browser has laid the new tree out, and keep measuring
     for as long as it is still moving. */
  pumpGeometry();
}

