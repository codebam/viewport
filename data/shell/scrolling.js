/* SPDX-License-Identifier: MIT
 *
 * The scrolling layout, and the overview.
 *
 * niri's model: an endless strip of columns that scrolls to follow focus, where
 * opening a window never resizes the ones already open. The overview lives here
 * because it is the same problem — every workspace laid out at once, scaled.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */
/* ------------------------------------------------------------------------
 * Scrolling layout (niri)
 *
 * A workspace is an endless horizontal strip of columns. Each column holds one
 * or more windows stacked vertically, and columns keep the width they were
 * given: opening a window never reflows the ones already there, it just makes
 * the strip longer. The view scrolls to keep the focused column on screen.
 *
 * The tree still holds the structure — the root's children are the columns —
 * so everything that walks or edits it keeps working. What differs is the
 * rendering: fixed widths and a scroll offset instead of flex-grow.
 * --------------------------------------------------------------------- */

/* Column widths as a fraction of the output, cycled by layout.column.width.
 * Same set as niri's default preset list. */
const COLUMN_WIDTHS = [1 / 3, 1 / 2, 2 / 3, 1];

/* Width of the divider between columns, which is --gap (the inner gap). Read
   from the stylesheet so the two cannot drift apart, with a fallback for the
   case where computed styles are unavailable (the test harness has no layout
   engine). */
function gapPx(workspace = renderingWorkspace ?? activeWorkspace()) {
  const configured = workspaceRule(workspace).gaps?.inner;
  if (Number.isFinite(configured)) return configured;
  const raw = typeof getComputedStyle === 'function'
    ? getComputedStyle(document.documentElement).getPropertyValue('--gap')
    : '';
  const value = parseInt(raw, 10);
  return Number.isFinite(value) ? value : 8;
}

/* The outer gap: extra space around the edge of the output, added *on top of*
   the inner gap. Sway's `gaps.outer`. Default 0, so without it the desktop's
   edge is just the inner gap, exactly as it has always been. */
function gapOuterPx(workspace = renderingWorkspace ?? activeWorkspace()) {
  const configured = workspaceRule(workspace).gaps?.outer;
  if (Number.isFinite(configured)) return configured;
  /* The computed value first, because a theme may set this in a stylesheet
     rather than through `gaps` — and the inline property after it, which is
     where applyGaps() writes and the only one a shell running without a
     browser (tests/shell.test.js) has. */
  const raw = (typeof getComputedStyle === 'function'
    ? getComputedStyle(document.documentElement).getPropertyValue('--gap-outer')
    : '') || document.documentElement.style.getPropertyValue('--gap-outer');
  const value = parseInt(raw, 10);
  return Number.isFinite(value) ? value : 0;
}

/* Whether smart gaps drop the inner gap for a lone window. */
let gapsSmart = false;

/* Whether smart radius squares that same window's corners, and null for "not
   said".

   Null means off. It used to follow `gaps.smart`, on the argument that smart
   gaps push a lone window against the edge of the screen and a rounded corner
   there is a notch of wallpaper in the corner of the monitor. That argument is
   still true and it is still not worth what it cost: a radius that was asked
   for went unhonoured on every desktop with one window on it, which reads as
   the setting being broken rather than as a rule being applied — and it comes
   back the moment a second window opens, which reads as nothing at all.
   Somebody who wants sway's behaviour asks for it with `border.smart`. */
let borderSmart = null;

/* Whether a lone window is drawn square. See `borderSmart`.
 *
 * Following the gaps means following the *reason* for them, not the setting:
 * smart gaps square a lone window because they push it against the edge of the
 * screen, where a rounded corner is a notch of wallpaper in the corner of the
 * monitor. An outer gap keeps it off that edge — the window is inset from the
 * screen on every side and its corners are over the desktop, where a curve is
 * simply a curve — so there is nothing there to square it for.
 *
 * Set on its own, `border.smart` is taken at its word either way: someone who
 * asked for square corners is not asking about gaps. */
function smartRadius() {
  return borderSmart === true;
}

/* True when the workspace shows a single window that fills the tiling area —
   the condition under which smart gaps collapse to the outer gap only.

   Filling it is half the condition, not an afterthought. In the strip a column
   keeps the width it was given, so a lone half-width column does not reach its
   own screen edge no matter what the padding is: dropping the edge gap there
   only makes the column a little wider and slides it, which is not what smart
   gaps are for. Every other layout gives a single window the whole area, so
   there the count alone settles it. */
function singleWindowOn(workspace) {
  if (workspace == null) return false;
  if (leavesOf(workspace).filter((leaf) => !isMinimized(leaf.id)).length !== 1) return false;
  if (layoutModeOf(workspace) !== 'scrolling') return true;

  /* Same filter renderStrip uses: a child with no view of its own is a slot
     nothing has claimed yet and is not drawn, so it is not a column. */
  const columns = (workspaces.get(workspace)?.children ?? []).filter(hasLayoutView);
  return columns.length === 1 && (columns[0].width ?? 0) >= 1;
}

/* The effective padding around the edge of this workspace's tiling area. It
   is inner + outer normally; with smart gaps and a single window it is just
   the outer gap, so a lone window does not sit far from its own screen edge. */
function edgeGapPx(workspace) {
  const smart = workspaceRule(workspace).gaps?.smart ?? gapsSmart;
  if (smart && singleWindowOn(workspace)) return gapOuterPx(workspace);
  return gapPx(workspace) + gapOuterPx(workspace);
}

const COLUMN_HEIGHTS = [1 / 3, 1 / 2, 2 / 3, 1];

/* `area` may be supplied by the caller. The overview renders a strip into a
 * container that is not in the document yet, and an unattached element measures
 * zero — which collapsed every column to nothing and left the windows sized by
 * their own content instead of by the layout. */
function renderStrip(root, output, area = null) {
  const strip = document.createElement('div');
  strip.className = 'strip';

  if (area === null) {
    /* The measured rect is the border box, and `.windows` is padded by the
       edge gap (inner + outer) on every side. Sizing columns against it makes
       a full-width column two gaps wider than the space it can actually occupy
       — it overflowed the right edge and never sat centred. The strip lives
       inside the padding, so that is what it has to be measured against. */
    const rect = output.windowsEl.getBoundingClientRect();
    const pad = edgeGapPx(output.workspace);
    area = { width: rect.width - pad * 2, height: rect.height - pad * 2 };
  }
  const columns = root.children.filter(hasLayoutView);

  let offset = 0;
  let focusedStart = null;
  let focusedWidth = 0;

  /* Dividers come out of the columns' own share rather than being added on
     top. Otherwise two half-width columns plus the divider between them are
     wider than the screen, and focusing the second one has to scroll by a gap
     — the windows visibly shift when switching between two halves that ought
     to sit still. Taking (N-1) gaps and spreading them across N columns makes
     fractions that sum to 1 fill the width exactly. */
  const share = columns.length > 1
    ? gapPx() * (columns.length - 1) / columns.length : 0;

  columns.forEach((column, i) => {
    const width = Math.max(1,
      Math.round(area.width * (column.width ?? 1 / 2) - share));

    if (i > 0) {
      /* Grabbable edge, same as between tiled windows. It drags the column to
         its left; the gap it sits in is shell-drawn, so no compositor support
         is needed. */
      const divider = document.createElement('div');
      divider.className = 'divider';
      divider.addEventListener('mousedown', (event) =>
        beginColumnDrag(event, output.workspace, columns[i - 1]));
      strip.append(divider);
      /* Counted, or the scroll offset drifts by one gap per column and the
         focused column stops lining up with the edge of the screen. */
      offset += gapPx();
    }

    const el = document.createElement('div');
    el.className = 'column';
    el.style.width = `${width}px`;

    /* A column is itself a vertical stack, so the existing renderer handles
       what is inside it. */
    const inner = renderTree(column);
    if (inner) el.append(inner);
    strip.append(el);

    if (containsFocus(column)) {
      focusedStart = offset;
      focusedWidth = width;
    }
    offset += width;
  });

  /* Scroll the least amount that brings the focused column fully into view. A
     column wider than the screen is aligned to the left edge instead, since it
     cannot be fully shown either way. Suppressed mid-gesture: the fingers are
     driving, and snapping back to the focused column would fight them. */
  const dragging = gestureWorkspace === output.workspace
    || columnDragWorkspace === output.workspace;
  const workspace = output.workspace;
  const lastScroll = scrollOffsets.get(workspace) ?? 0;
  let scroll = lastScroll;
  if (focusedStart !== null && !dragging) {
    if (focusedWidth >= area.width || focusedStart < scroll) {
      scroll = focusedStart;
    } else if (focusedStart + focusedWidth > scroll + area.width) {
      scroll = focusedStart + focusedWidth - area.width;
    }
  }
  /* Never scroll past either end of the strip. */
  scroll = Math.max(0, Math.min(scroll, Math.max(0, offset - area.width)));
  scrollOffsets.set(workspace, scroll);

  /* The strip element is new every render, so it would simply appear already
     scrolled. Start it where the last one ended and move it in the same frame
     the windows are flipped, so the two animations run together. */
  const previous = scrollOffsets.has(workspace) ? lastScroll : scroll;
  strip.style.transform = `translateX(${-(dragging ? scroll : previous)}px)`;
  strip.dataset.scroll = String(scroll);
  if (dragging) strip.classList.add('dragging');

  return columns.length > 0 ? strip : null;
}

/* Reshape existing workspaces when the layout model changes at runtime.
 *
 * The two models share the tree but read it differently: the strip needs the
 * root to be horizontal with one child per column, and each column to carry a
 * width. Coming back the other way, those widths are meaningless and the tabbed
 * containers a strip never has are left alone. Without this a switch mid-session
 * renders a tree the new model cannot make sense of. */
function normaliseForLayout(workspace = null) {
  for (const [n, root] of workspaces) {
    if (workspace !== null && workspace !== n) continue;
    if (layoutModeOf(n) === 'scrolling') {
      root.dir = 'horizontal';
      root.layout = 'split';
      for (const column of root.children) {
        if (column.width === undefined) column.width = COLUMN_WIDTHS[1];
        if (column.type === 'split') column.dir = 'vertical';
      }
    } else {
      for (const column of root.children) delete column.width;
    }
  }
  scrollOffsets.clear();
}

/* Every workspace at once, laid out as a grid of miniature desktops.
 *
 * Each thumbnail renders the workspace's real tree — the same renderTree and
 * renderStrip used for the live layout — inside a container sized to the output
 * and then scaled down. That is what makes the miniatures accurate rather than
 * an approximation of the layout: they *are* the layout, drawn smaller.
 *
 * The windows inside are real surfaces. The compositor shrinks them for us,
 * told through the scale on view.layout, so nothing is asked to resize. */
/* Which workspaces each output shows in the overview.
 *
 * A window element exists once in the DOM, so a workspace can only be drawn on
 * one screen — rendering every workspace on every output would have each grid
 * steal the windows from the last. Dealing them out instead means both monitors
 * are used, which is the point of having two.
 *
 * Each output keeps the workspace it is currently displaying, so the thumbnail
 * of what you were just looking at is on the screen you were looking at. The
 * rest are dealt round-robin in order. */
function overviewAssignment() {
  /* Every workspace, not only the occupied ones. An overview is how you get
     somewhere, and the empty workspace you want to move a window to has to be
     visible to be a target. It also keeps the grid a grid: with one workspace
     in use the alternative is a single cell at almost full size, which looks
     like nothing happened. */
  const names = [...outputs.keys()];
  const assignment = new Map(names.map((name) => [name, []]));

  const remaining = sortedWorkspaceIds();
  for (const name of names) {
    const own = outputs.get(name).workspace;
    const at = remaining.indexOf(own);
    if (at >= 0) {
      remaining.splice(at, 1);
      assignment.get(name).push(own);
    }
  }

  let turn = 0;
  for (const n of remaining) {
    assignment.get(names[turn % names.length]).push(n);
    turn++;
  }

  for (const list of assignment.values()) list.sort((a, b) => a - b);
  return assignment;
}

function renderOverview(output, list) {
  const grid = document.createElement('div');
  grid.className = 'overview';

  if (list.length === 0) return grid;
  const area = output.windowsEl.getBoundingClientRect();

  /* Squarish grid: enough columns that the thumbnails stay wide rather than
     tall, since an output is wider than it is high. */
  const columns = Math.ceil(Math.sqrt(list.length)) || 1;
  const rows = Math.ceil(list.length / columns);

  /* Each thumbnail is a picture of a monitor, so it is shaped like one.
   *
   * The grid tracks are whatever shape the grid is divided into, which on an
   * ultrawide with four workspaces is nothing like the output: the miniature
   * inside was scaled to fit and pinned to the top left, so every thumbnail
   * was a monitor-shaped image in the corner of a differently-shaped box, with
   * dead space down one side. Sizing the box from the output's own ratio makes
   * the two the same thing, and the miniature then fills it exactly.
   *
   * The gaps and the padding come out of the available space first — they are
   * `--gap * 2` in the stylesheet, and a track worked out without them is wider
   * than the track that gets laid out, which puts the last column off the edge. */
  const spacing = gapPx() * 2;
  const trackWidth = (area.width - spacing * (columns + 1)) / columns;
  const trackHeight = (area.height - spacing * (rows + 1)) / rows;
  const ratio = area.height > 0 ? area.width / area.height : 16 / 9;
  /* Whichever of the two runs out first, so the box fits its track in both
     directions and keeps the ratio. */
  const thumbWidth = Math.max(0, Math.min(trackWidth, trackHeight * ratio));
  const thumbHeight = ratio > 0 ? thumbWidth / ratio : 0;
  const scale = area.width > 0 ? thumbWidth / area.width : 0;

  grid.style.gridTemplateColumns = `repeat(${columns}, 1fr)`;
  /* Explicit rows as well, so the tracks are even and the last row is the
     same height as the others rather than being sized by its content — which
     is what decides whether the grid reads as centred. */
  grid.style.gridTemplateRows = `repeat(${rows}, 1fr)`;

  /* And the same overview for an engine that has no grid — Servo, today. The
     two declarations above are dropped there and `@supports not (display:
     grid)` in shell.css lays the thumbnails out as a wrapping flex row
     instead; what that needs from here is the width one full row takes, or it
     wraps wherever the output's width happens to run out. Ceiled, because the
     track arithmetic makes `columns` of them fit exactly and a fraction over
     wraps a row one thumbnail early.

     Asked of the engine rather than assumed: this is the one thing the
     stylesheet cannot tell the script. An engine too old to answer at all is
     treated as having no grid, which costs it a layout it can do and gives it
     a desktop either way. */
  if (!(typeof CSS === 'object' && CSS.supports
        && CSS.supports('display', 'grid'))) {
    grid.style.maxWidth =
      `${Math.ceil(columns * thumbWidth + (columns + 1) * spacing)}px`;
    grid.style.marginInline = 'auto';
  }

  for (const n of list) {
    const cell = document.createElement('div');
    cell.className = 'thumb'
      + (n === output.workspace ? ' current' : '')
      + (idsOf(n).length === 0 ? ' empty-workspace' : '');

    const label = document.createElement('span');
    label.className = 'thumb-label';
    label.textContent = String(n);
    cell.append(label);

    /* The box is the monitor's shape, and centred in its track by the grid's
       `place-items` — a thumbnail that fills its track would not need this,
       and one that keeps a ratio always has slack in one direction. */
    cell.style.width = `${thumbWidth}px`;
    cell.style.height = `${thumbHeight}px`;

    const inner = document.createElement('div');
    inner.className = 'thumb-inner';
    inner.style.width = `${area.width}px`;
    inner.style.height = `${area.height}px`;
    inner.style.transform = `scale(${scale})`;

    const root = workspaces.get(n);
    const rendered = root
      ? (layoutModeOf(n) === 'scrolling'
        ? renderStrip(root, { ...output, workspace: n },
          { width: area.width, height: area.height })
        : renderTree(root))
      : null;
    if (rendered) inner.append(rendered);

    /* Floating windows belong to the thumbnail of their own workspace too. */
    for (const [id, floating, view] of floatingEntries()) {
      if (floating.workspace !== n || isMinimized(id)) continue;
      view.el.classList.add('floating');
      Object.assign(view.el.style, {
        left: `${floating.x}px`, top: `${floating.y}px`,
        width: `${floating.width}px`, height: `${floating.height}px`,
      });
      inner.append(view.el);
      renderedIds.add(id);
    }

    /* Everything drawn in this thumbnail is drawn at its scale, and clipped to
       the thumbnail rather than to the whole output — otherwise a window whose
       layout overflows spills across its neighbours, since the surfaces the
       compositor draws are not bounded by the thumbnail's `overflow: hidden`. */
    for (const id of idsOf(n)) {
      /* Not every leaf has a window: a restored session leaves slots in the
         tree for applications that have not come back yet. */
      const view = views.get(id);
      if (view) view.overview = { scale, cell };
    }

    overviewThumbs.set(n, cell);

    cell.append(inner);
    cell.addEventListener('mousedown', () => {
      /* Switch the output the thumbnail is on, not whichever was last active:
         clicking a workspace on the right monitor means you want it there. */
      setOverview(false);
      setActiveOutput(output.name);
      switchWorkspace(output.name, n);
    });
    grid.append(cell);
  }

  return grid;
}
