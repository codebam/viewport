/* SPDX-License-Identifier: MIT
 *
 * The matrix layout: the focused window large on the left, the rest of the
 * focus history halving away down the right.
 *
 * The fourth layout model, beside the i3-style tree in tiling.js, niri's strip
 * in scrolling.js and the orbits in solar.js. Like solar it computes
 * rectangles rather than handing the problem to flexbox, and for the same
 * reason: what a window gets is a function of *how recently it was focused*,
 * not of where it sits in the tree, and there is no arrangement of rows and
 * columns that expresses "each one takes half of what the one before it left".
 *
 * The model, in full:
 *
 *   - The focused window takes 60% of the width, full height, on the left.
 *   - The remaining 40% is a column, and the second-most-recently focused
 *     window takes half of it. The third takes half of what is left, the
 *     fourth half of that, and so on.
 *   - The halving stops when the next slot would be shorter than
 *     MATRIX.minSlotHeight. That bound is what makes the depth logarithmic:
 *     a 1050px column at 100px minimum holds four slots and never more,
 *     whether there are five windows open or fifty.
 *   - Everything past that is stacked in the last slot, most recent on top.
 *     The ones underneath are hidden — kept in the DOM so the client stays
 *     alive, drawn nowhere, and reported to the compositor as off screen.
 *
 * Order is focus history and nothing else. The tiling tree still says which
 * windows exist and which workspace they are on; this reads that and never
 * writes it, which is what lets window.move, the session format and the
 * overview keep working without knowing the matrix exists. It is the same
 * bargain solar.js makes, and the reason both are additions rather than
 * rewrites.
 *
 * Nothing here is driven by a clock. The whole layout is recomputed on the
 * three events that can change it — a window opened, a window closed, focus
 * moved — plus the resize that changes what there is to divide up. There is no
 * animation loop and no easing in this file; motion.js tweens between the
 * rectangles this produces, exactly as it does for every other layout.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* Every number the layout depends on. Read at use rather than captured, so
 * editing one and reloading the shell takes effect without a restart. */
const MATRIX = {
  /* The focused window's share of the width. */
  primaryRatio: 0.60,

  /* How short a slot is allowed to get before the column stops dividing.
     This is the only thing bounding the depth: without it the tenth window
     would be handed two pixels, which is not a window, it is a line. */
  minSlotHeight: 100,

  /* Space between the primary and the column, and between slots — and, taken
     off the area before anything is placed, around the outside of both.

     The number itself is `--gap` from the stylesheet, which the shell reads at
     use through gapPx() and passes in: a theme that sets a different gap moves
     this layout with it, exactly as it moves the tiling tree's dividers and the
     scrolling strip's columns. What is written here is only the fallback the
     pure kernel falls back on when a caller supplies nothing, which is the same
     8 gapPx() answers with where there are no computed styles. */
  gap: 8,
};

/* ------------------------------------------------------------------------
 * Focus history
 *
 * The stack the layout is a function of. Index 0 is the window in focus,
 * index 1 the one before it, and so on — the order Alt+Tab would step
 * through, which is exactly what the column is showing.
 *
 * One list for the whole session rather than one per workspace. A window
 * carried to another workspace keeps its place in the history, which is what
 * you want: the thing you were just in stays the thing you were just in, and
 * arriving on the workspace it went to finds it at the top of the column
 * rather than at the bottom.
 * --------------------------------------------------------------------- */

const focusStack = [];

/* Move a window to the front, wherever it was. The splice is the whole state
 * transition for WINDOW_FOCUSED — no rebuild, no re-sort, and the layout that
 * follows reads the array as it now stands. */
function matrixFocused(id) {
  if (id == null) return;
  const at = focusStack.indexOf(id);
  if (at === 0) return;
  if (at > 0) focusStack.splice(at, 1);
  focusStack.unshift(id);
}

/* A new window goes to the front. The compositor focuses what it just mapped,
 * so this is nearly always followed by a matrixFocused for the same id — but
 * only nearly: a window that opens on a workspace nobody is looking at is
 * never focused, and would otherwise sit at the bottom of a column it is the
 * newest thing in. */
function matrixOpened(id) {
  matrixFocused(id);
}

/* Drop a closed window. Called from removeView, beside solarForget, for the
 * same reason that one is: this is state about a window that is kept outside
 * the window's own record, so it has to be forgotten by hand. */
function matrixClosed(id) {
  const at = focusStack.indexOf(id);
  if (at >= 0) focusStack.splice(at, 1);
}

/* The windows the matrix places on a workspace, most recently focused first.
 *
 * Floating windows are left out entirely, as they are in solar: a dialog is
 * floating because the compositor judged that tiling it is the wrong thing to
 * do (views.rs, wants_floating), and giving one a slot is that same mistake
 * with different arithmetic. relayoutAll places them itself.
 *
 * Windows the history has never heard of come last, in tree order. That is a
 * session restore: view.added is replayed for every window before anything is
 * focused, so on the first relayout after a restart the history is empty and
 * tree order is the only order there is.
 *
 * O(S + N) — one pass over the history and one over the workspace, no sort. */
function matrixOrderOf(workspace) {
  const root = workspaces.get(workspace);
  if (!root) return [];
  const ids = dynamicOrder(root).filter(
    (id) => views.has(id) && !isFloating(id));

  const pending = new Set(ids);
  const ordered = [];
  for (const id of focusStack) {
    if (pending.delete(id)) ordered.push(id);
  }
  for (const id of ids) {
    if (pending.has(id)) ordered.push(id);
  }
  return ordered;
}

/* ------------------------------------------------------------------------
 * The layout itself
 *
 * A pure function of (windows, screen) — no DOM, no globals, nothing
 * measured. That is what lets tests/shell.test.js check the arithmetic
 * directly rather than through a stubbed getBoundingClientRect that returns
 * fixed numbers and proves nothing about it.
 * --------------------------------------------------------------------- */

/* How many slots a column of this height divides into.
 *
 * Every slot but the last takes half of what it was given, less a gap, so the
 * k-th is about height/2^k tall and the count is logarithmic in the ratio
 * between the column and the minimum. Counted with a loop rather than a log:
 * the loop runs about five times, and it is the same arithmetic the placement
 * below does, so the two cannot disagree about where the bound falls — which a
 * closed form and a rounding rule quietly could.
 *
 * At least one, always. A column too short to hold even the minimum still has
 * to put the second window somewhere, and a slot smaller than the bound is a
 * better answer than a window that vanishes. */
function matrixCapacity(height, gap = MATRIX.gap, minimum = MATRIX.minSlotHeight) {
  let capacity = 0;
  let remaining = height;
  while (remaining >= minimum) {
    capacity++;
    /* What this slot would keep if it split rather than taking the rest. */
    const half = Math.round((remaining - gap) / 2);
    if (half < minimum) break;
    remaining -= half + gap;
  }
  return Math.max(1, capacity);
}

/* A placed window: the rectangle it occupies, and what the rectangle means.
 *
 * The *slot*, frame included, not the client's hole. A slot is a division of
 * the screen the way a tiling column is, so the two pixels of border the
 * stylesheet draws come out of it rather than being added around it — which is
 * what keeps the window against the right or bottom edge from overhanging the
 * screen by exactly that border. The hole is whatever is left inside, and
 * geometry.js measures it rather than being told.
 *
 * `slot` is -1 for the primary and the slot index otherwise, so the caller can
 * style by depth without re-deriving it. `hidden` is a window buried under
 * another in the last slot: it has a rectangle — the one it would occupy if it
 * came to the top — and is drawn nowhere. Keeping the rectangle rather than
 * zeroing it is what lets it animate out from under the stack when it is
 * focused again instead of appearing from the origin. */
function matrixGeometry(id, tier, slot, rect, hidden = false) {
  return {
    id,
    tier,
    slot,
    x: Math.round(rect.x),
    y: Math.round(rect.y),
    width: Math.max(1, Math.round(rect.width)),
    height: Math.max(1, Math.round(rect.height)),
    hidden,
  };
}

/* The layout, for one screen's worth of windows in MRU order.
 *
 * `windows` is ids, or records with an `id` — index 0 is the focused window,
 * index 1 the one before it. `screen` is the area windows may be placed in,
 * as { x, y, width, height }.
 *
 * Deterministic and total: the same two arguments always give the same
 * rectangles, and every window in comes back out with one, including the ones
 * that end up buried. O(N), one pass, no allocation per window beyond the
 * result itself. */
function calculateLayout(windows, screen, options = {}) {
  const out = [];
  if (!Array.isArray(windows) || windows.length === 0) return out;
  if (!screen || !(screen.width > 0) || !(screen.height > 0)) return out;

  const ids = windows.map((w) => (w !== null && typeof w === 'object' ? w.id : w));
  const ratio = options.primaryRatio ?? MATRIX.primaryRatio;
  const gap = options.gap ?? MATRIX.gap;
  const minimum = options.minSlotHeight ?? MATRIX.minSlotHeight;

  const x = screen.x ?? 0;
  const y = screen.y ?? 0;

  /* One window is the whole screen. Sixty per cent of it with nothing beside
     it would be a window with a hole next to it, and no model wants that. */
  if (ids.length === 1) {
    out.push(matrixGeometry(ids[0], 'primary', -1,
      { x, y, width: screen.width, height: screen.height }));
    return out;
  }

  /* The column is what is left after the primary and the gap between them.
     Clamped so that a screen too narrow for both still gives each of them a
     pixel to be in rather than a negative width. */
  const primaryWidth = Math.max(1,
    Math.min(screen.width - 1, Math.round((screen.width - gap) * ratio)));
  const columnX = x + primaryWidth + gap;
  const columnWidth = Math.max(1, screen.width - primaryWidth - gap);

  out.push(matrixGeometry(ids[0], 'primary', -1,
    { x, y, width: primaryWidth, height: screen.height }));

  const capacity = matrixCapacity(screen.height, gap, minimum);
  const secondaries = ids.length - 1;

  let top = y;
  let remaining = screen.height;

  for (let slot = 0; slot < Math.min(capacity, secondaries); slot++) {
    /* The last slot placed takes everything left rather than half of it —
       either because the column has run out of depth, or because there are no
       more windows to give the other half to. Without that the column would
       end in dead space below the final window, which is space the window it
       belongs to could have had. */
    const last = slot === capacity - 1 || slot === secondaries - 1;
    const height = last ? remaining : Math.round((remaining - gap) / 2);
    const rect = { x: columnX, y: top, width: columnWidth, height };

    if (!last || secondaries <= capacity) {
      out.push(matrixGeometry(ids[slot + 1], 'slot', slot, rect));
    } else {
      /* Past the bound. Everything still unplaced shares this rectangle, most
         recent on top and the rest hidden underneath it — the tabbed
         container of the tiling tree, without the strip of titles, because
         what is in the stack is answerable from Alt+Tab order and the badge
         the stylesheet draws. */
      for (let i = slot + 1; i < ids.length; i++) {
        out.push(matrixGeometry(ids[i], 'stacked', slot, rect, i > slot + 1));
      }
    }

    top += height + gap;
    remaining -= height + gap;
  }

  return out;
}

/* ------------------------------------------------------------------------
 * Reading the shell's state
 * --------------------------------------------------------------------- */

/* The area of an output windows may be placed in.
 *
 * Measured, not derived from the output's mode: the bar and a panel's exclusive
 * zone are both in the stylesheet, and `.windows` is the element they have
 * already been subtracted from. The origin is the area's own top left rather
 * than the page's, because what comes out of the layout is written straight
 * back as `left` and `top` on a window absolutely positioned inside that
 * element — in page coordinates every window on the second monitor would be
 * offset by the width of the first.
 *
 * The gap is the one inset that is *not* already taken out. `.windows` is
 * padded by `--gap` in the stylesheet, but an absolutely positioned child is
 * laid out against its padding box, so `.matrix-field` at inset:0 covers that
 * padding rather than sitting inside it — and getBoundingClientRect returns the
 * border box, which includes it too. Both halves of the outer gap therefore
 * have to come off by hand, or the layout runs to the edge of the screen and
 * the desktop the stylesheet describes is not the one that is drawn. The
 * scrolling strip subtracts the same padding for the same reason. */
function matrixAreaOf(output) {
  const rect = output?.windowsEl?.getBoundingClientRect();
  if (!rect) return null;
  const edge = edgeGapPx(output?.workspace);
  const width = rect.width - edge * 2;
  const height = rect.height - edge * 2;
  if (width <= 0 || height <= 0) return null;
  return { x: edge, y: edge, width, height };
}

/* The layout for one workspace, on the output showing it.
 *
 * The entry point the rest of the shell asks for by name, and a thin one: it
 * resolves the workspace's windows and their order out of shell state and
 * hands them to calculateLayout(), which is where the arithmetic is and which
 * knows nothing about any of that — including where the gap between two windows
 * comes from, which is why it is read here and passed in. */
function recalculateMatrixLayout(workspace, outputGeometry = null) {
  const area = outputGeometry
    ?? matrixAreaOf(outputs.get(hostOfWorkspace(workspace)));
  return calculateLayout(matrixOrderOf(workspace), area, { gap: gapPx() });
}

/* Every output's rectangles, worked out on the way into a relayout. One pass
 * for all of them, so the caller has the whole plan before it draws any of it
 * — and so "is this monitor empty" is answerable from what was planned rather
 * than from what has been rendered so far. */
function planMatrix() {
  const plan = new Map();
  for (const [name, output] of outputs) {
    const area = matrixAreaOf(output);
    plan.set(name, area
      ? recalculateMatrixLayout(output.workspace, area) : []);
  }
  return plan;
}

/* ------------------------------------------------------------------------
 * Rendering
 * --------------------------------------------------------------------- */

/* Forget everything the matrix did to a window, and undo it.
 *
 * Called on the way into any relayout that is not the matrix's. The inline
 * rect is not the stylesheet's to reset: a window left carrying left/top from
 * a slot would be positioned in the middle of a tiling column, and one left
 * hidden would stay invisible for the rest of the session. Cheap when there is
 * nothing to undo. */
function clearMatrixState() {
  for (const [, view] of views) {
    if (!view.matrix) continue;
    view.matrix = null;
    view.el.classList.remove('tile', 'primary', 'slot', 'stacked');
    view.el.hidden = false;
    delete view.el.dataset.stack;
    Object.assign(view.el.style,
      { left: '', top: '', width: '', height: '' });
  }
}

/* One output's matrix, as elements.
 *
 * Positioned absolutely inside a container of its own rather than onto the
 * output's `.windows` directly, so the container is what the FLIP in
 * geometry.js measures against and a workspace switch does not animate every
 * window from wherever the previous workspace's window of the same index
 * happened to be. Same reason renderSolar has one. */
function renderMatrix(geometries, output) {
  const el = document.createElement('div');
  el.className = 'matrix-field';

  /* How many windows the deepest slot is holding, for the badge. Counted
     before anything is drawn: the window that shows it is the top of the
     stack, and it has to be told a number that includes the ones under it. */
  const buried = geometries.filter((g) => g.tier === 'stacked').length;

  for (const geometry of geometries) {
    const view = views.get(geometry.id);
    if (!view) continue;

    view.matrix = { tier: geometry.tier, slot: geometry.slot };

    /* A window under the top of the stack is in the DOM and drawn nowhere.
       Saying so is a matter of leaving it out of renderedIds and nothing else:
       relayoutAll hides what it did not render and tells the compositor the
       window is off screen, so its surface stops being painted into a hole
       that is not there. That is the same route a collapsed tab takes. */
    view.el.classList.add('tile');
    for (const tier of ['primary', 'slot', 'stacked']) {
      view.el.classList.toggle(tier, tier === geometry.tier);
    }

    if (geometry.tier === 'stacked' && !geometry.hidden && buried > 1) {
      view.el.dataset.stack = String(buried);
    } else {
      delete view.el.dataset.stack;
    }

    Object.assign(view.el.style, {
      left: `${geometry.x}px`,
      top: `${geometry.y}px`,
      width: `${geometry.width}px`,
      height: `${geometry.height}px`,
      flexGrow: '',
    });

    if (!geometry.hidden) renderedIds.add(geometry.id);
    el.append(view.el);
  }

  return el;
}
