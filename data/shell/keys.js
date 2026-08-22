/* SPDX-License-Identifier: MIT
 *
 * The keyboard, for every surface the shell puts on screen.
 *
 * A picker that can only be finished with the pointer is a picker somebody
 * who cannot use a pointer cannot finish. The launcher and the passphrase box
 * each grew their own `keydown` because each has a text field and a text
 * field is obviously keyboard-shaped; the tray menu, the notification centre,
 * the power menu and the clipboard history have no field, so they were opened
 * by a chord and then abandoned to the mouse. This file is the half they were
 * all missing, written once, because five copies of an arrow-key handler is
 * five places for the wrapping arithmetic to be subtly different.
 *
 * Two things about the shell make this less obvious than it sounds.
 *
 * **Keys do not arrive unless the shell is given them.** The shell is a
 * Wayland client like any other, and on a desktop with a window open that
 * window has the keyboard. `shell.focus` is the request that moves it, and
 * the surface has to hand it back when it goes or the next keystroke lands
 * nowhere while the window that was being worked in still looks focused.
 * Before this file only the two surfaces with text fields asked; every
 * surface that can be opened now asks, which is what makes an arrow key
 * reach it at all. `keyNavOpen` and `keyNavClose` are that pair, and they
 * remember who had the keyboard per surface so two of them cannot overwrite
 * one another's answer.
 *
 * **The rows are rebuilt under the selection.** Every one of these lists is
 * redrawn whole whenever the compositor sends a new snapshot — a copy lands
 * in the clipboard, a notification is forgotten, an access point comes and
 * goes — so the selected element is destroyed several times while somebody is
 * steering with the arrow keys. The selection therefore lives here as an
 * index against a named surface, not as a DOM property and not as
 * `document.activeElement`, and `keyNavRefresh` paints it back on after each
 * rebuild. Keeping it on the element was tried first and loses the highlight
 * on every incoming message, which on the clipboard picker is every copy.
 *
 * Activation deliberately synthesises the click the pointer would have
 * delivered rather than calling the row's action directly: there is then
 * exactly one path from "this row was chosen" to "this message was sent",
 * and a row whose click handler grows a condition cannot acquire a keyboard
 * path that skips it.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* The class a surface puts on anything the arrow keys should stop at, and the
 * class this file puts on the one they have stopped at. Two classes rather
 * than reusing each picker's own `selected`/`active`: `active` on a power row
 * means "this is the profile in force" and `selected` on a launcher row means
 * "this is what Enter will start", and neither of those is "this is where the
 * keyboard is". A ring that meant three things would be wrong two thirds of
 * the time. */
const KEY_NAV_ROW = 'kbd-row';
const KEY_NAV_HERE = 'kbd-here';

/* Per surface: where the keyboard is in the list, and which window had the
 * keyboard before the surface took it. Keyed by the same name `setOverlay`
 * uses, so the thing that draws a rectangle and the thing that steers inside
 * it cannot disagree about which surface is being talked about. */
const keyNavState = new Map(); // name -> { index, restoreId, rows, on, keysOn, handler }

function keyNavFor(name) {
  let state = keyNavState.get(name);
  if (state === undefined) {
    state = { index: 0, restoreId: null, rows: null, on: null,
      keysOn: null, handler: null };
    keyNavState.set(name, state);
  }
  return state;
}

/* Take the keyboard for a surface that is opening.
 *
 * What had focus is read *before* the request goes out, because sending it is
 * what loses it: the compositor answers `shell.focus` with a `view.focused`
 * naming nothing, which sets `focusedId` to null on the way through. Called
 * twice for one opening — a picker that redraws itself on the way up — the
 * second call must not overwrite the first with that null, which is what the
 * `restoreId === null` guard is for.
 *
 * `index` is reset here and nowhere else: a surface should open on its first
 * row, and every rebuild while it is up should keep the row somebody has
 * steered to. */
function keyNavOpen(name, { index = 0 } = {}) {
  const state = keyNavFor(name);
  if (state.restoreId === null) state.restoreId = focusedId;
  state.index = index;
  send({ type: 'shell.focus' });
}

/* Give the keyboard back to whatever had it.
 *
 * A window that closed in the meantime is not chased: the compositor refuses a
 * `view.focus` for an id that is gone, and focus stays where it is, which is
 * where it would have been anyway. Sending it regardless would be one refused
 * message per picker close for the rest of the session. */
function keyNavClose(name) {
  const state = keyNavState.get(name);
  if (state === undefined) return;
  const restore = state.restoreId;
  /* And the listener with it, for the reason bindKeyNav gives: a surface bound
     on an element that outlives it — `#tray-menu` is the one — would otherwise
     keep every handler it was ever given, and the forgetting of the state here
     is exactly what would stop the next bind from finding the old one. */
  if (state.keysOn && state.handler) {
    state.keysOn.removeEventListener?.('keydown', state.handler);
  }
  keyNavState.delete(name);
  if (restore !== null && views.has(restore)) {
    send({ type: 'view.focus', id: restore });
  }
}

/* Every row the arrow keys stop at, in the order they are drawn.
 *
 * Hidden rows are skipped, which is what makes the tray menu's accordion work
 * without a second traversal: a submenu that is closed is a run of rows with
 * `hidden` set, and stepping past them is the same as their not being there.
 * So is a disabled row — dbusmenu marks those, and stopping the keyboard on
 * something that will refuse to be activated is a dead end somebody has to
 * press past. */
function keyNavRows(root) {
  const found = [];
  const walk = (el) => {
    for (const child of el.children ?? []) {
      if (child.hidden) continue;
      if (child.classList?.contains?.(KEY_NAV_ROW)
        && !child.classList.contains('disabled')) {
        found.push(child);
      }
      walk(child);
    }
  };
  if (root) walk(root);
  return found;
}

/* Bind a surface's keys, and paint the selection on for the first time.
 *
 * Called once per built dialog — which for most of these surfaces is once per
 * render, because they are rebuilt whole. The listener is on the dialog rather
 * than on the document so that two surfaces open at once (a tray menu over a
 * notification centre) do not both act on one press.
 *
 *   rows      () => the elements to steer between. Defaults to whatever
 *             carries `kbd-row` under the dialog, which is what every caller
 *             but the launcher wants; the launcher's list is a child element
 *             that is replaced on every keystroke, so it names its own.
 *   dismiss   () => take the surface down. Escape, and nothing else: a
 *             surface that also closed on Left would be one that could not
 *             hold a horizontal control.
 *   activate  (row, index) => what Enter does. Defaults to clicking the row.
 *   remove    (row, index) => what Delete does, where the list has a notion
 *             of forgetting a row. Absent means Delete does nothing rather
 *             than falling through to the client underneath, which would be
 *             a keystroke deleting a character in a text editor because a
 *             picker was open over it.
 *   orient    'vertical' (the default) or 'both'. 'both' also steers on Left
 *             and Right, for a list that is drawn as a grid.
 *   keysOn    the element the listener goes on. Defaults to the dialog, which
 *             is right for a surface that is all rows; the launcher binds it
 *             on its filter field instead, because that field is where the
 *             keyboard already is and a second listener on the dialog would
 *             see every press a second time as it bubbled past.
 *   focus     the element to put the keyboard in. Defaults to the dialog.
 */
function bindKeyNav(name, root, opts = {}) {
  const state = keyNavFor(name);
  state.rows = opts.rows ?? (() => keyNavRows(root));
  state.on = opts;
  /* The old listener first, where there is one.
   *
   * Most of these surfaces build a fresh dialog on every render, so the
   * element the last binding went on is already garbage and this does
   * nothing. The tray menu is the exception: it binds on `#tray-menu`, which
   * is in index.html and is the same element for the life of the page. Without
   * this, every opening of a tray menu left another handler on it, and by the
   * fifth one a single press of Down moved the highlight five rows. */
  if (state.keysOn && state.handler) {
    state.keysOn.removeEventListener?.('keydown', state.handler);
  }
  const keysOn = opts.keysOn ?? root;
  state.keysOn = keysOn;
  state.handler = (e) => keyNavKey(name, e);
  keysOn.addEventListener('keydown', state.handler);
  /* The dialog itself has to be focusable or the engine has nowhere to put
     the caret when the surface opens, and a keydown delivered to the document
     body would never reach a listener bound here. -1 rather than 0: it is
     reachable by script and by a click, and it is not a stop on the Tab
     order, which for a transient dialog would be a place Tab could land after
     the dialog had gone. */
  root.tabIndex = -1;
  (opts.focus ?? root).focus?.();
  keyNavRefresh(name);
}

/* Which row the keyboard is on, as an index. What the launcher's own
 * `launcherSelected` used to be: it is the same number and there should only
 * be one of it, or the highlight and the row Enter starts can disagree. */
function keyNavIndex(name) {
  return keyNavState.get(name)?.index ?? 0;
}

/* Put the keyboard on a particular row — the top of the list after the filter
 * changed under it, which is the one case where a surface knows better than
 * this file where the selection belongs. */
function keyNavSelect(name, index) {
  const state = keyNavState.get(name);
  if (state === undefined) return;
  state.index = index;
  keyNavRefresh(name);
}

/* Paint the selection back on after the rows have been rebuilt.
 *
 * Clamped rather than reset: a list that shrinks under the keyboard — the
 * clipboard picker while entries are being forgotten off the end of it —
 * should leave the selection on the last row rather than snapping it to the
 * top, which is where a `% length` would send it. */
function keyNavRefresh(name) {
  const state = keyNavState.get(name);
  if (!state?.rows) return;
  const rows = state.rows();
  if (rows.length === 0) {
    state.index = 0;
    return;
  }
  state.index = Math.max(0, Math.min(state.index, rows.length - 1));
  rows.forEach((row, i) => {
    const here = i === state.index;
    row.classList?.toggle?.(KEY_NAV_HERE, here);
    /* Told to the engine as well as painted, because the ring is what a
       sighted user sees and this is what a screen reader is read from: an
       assistive client walks the accessibility tree and asks which item is
       current, and a class name is not in that tree. Cheap to set here and
       impossible to remember at five call sites. */
    row.setAttribute?.('aria-selected', here ? 'true' : 'false');
    row.tabIndex = here ? 0 : -1;
    if (here) {
      row.focus?.();
      /* `block: 'nearest'` and never `behavior: 'smooth'`: a list scrolling
         under the keyboard is motion, the shell honours
         `prefers-reduced-motion` everywhere else, and an instant scroll is
         what the setting asks for. It is also what a held arrow key needs —
         a smooth scroll that has not finished when the next press arrives
         queues, and the list drifts on after the finger comes off. */
      row.scrollIntoView?.({ block: 'nearest' });
    }
  });
}

/* Move the keyboard by `delta` rows, wrapping.
 *
 * Wrapping rather than stopping, which is the launcher's behaviour and was
 * the right one: these lists are short, the wrap is how the last row is
 * reached from the first, and nothing here is a scrollbar somebody is
 * tracking the position of. */
function keyNavStep(name, delta) {
  const state = keyNavState.get(name);
  if (!state?.rows) return;
  const rows = state.rows();
  if (rows.length === 0) return;
  state.index = (state.index + delta + rows.length) % rows.length;
  keyNavRefresh(name);
}

/* The row the keyboard is on, or null for an empty list. */
function keyNavRow(name) {
  const state = keyNavState.get(name);
  if (!state?.rows) return null;
  return state.rows()[state.index] ?? null;
}

/* One press, dispatched.
 *
 * Everything unhandled is left alone rather than swallowed: a surface that
 * consumed every key would eat the compositor's own chords, and the chord
 * that closes the surface is the one somebody reaches for when a surface has
 * gone wrong. */
function keyNavKey(name, e) {
  const state = keyNavState.get(name);
  if (state === undefined) return;
  const both = state.on?.orient === 'both';
  const rows = state.rows ? state.rows() : [];
  const index = state.index;

  switch (e.key) {
    case 'ArrowDown':
      e.preventDefault?.();
      keyNavStep(name, 1);
      return;
    case 'ArrowUp':
      e.preventDefault?.();
      keyNavStep(name, -1);
      return;
    case 'ArrowRight':
      if (!both) return;
      e.preventDefault?.();
      keyNavStep(name, 1);
      return;
    case 'ArrowLeft':
      if (!both) return;
      e.preventDefault?.();
      keyNavStep(name, -1);
      return;
    case 'Home':
      e.preventDefault?.();
      state.index = 0;
      keyNavRefresh(name);
      return;
    case 'End':
      e.preventDefault?.();
      state.index = Math.max(0, rows.length - 1);
      keyNavRefresh(name);
      return;
    case 'Enter':
    case ' ':
      e.preventDefault?.();
      keyNavActivate(name);
      return;
    case 'Delete':
    case 'Backspace': {
      const remove = state.on?.remove;
      if (!remove) return;
      e.preventDefault?.();
      const row = rows[index];
      if (row) remove(row, index);
      return;
    }
    case 'Escape':
      e.preventDefault?.();
      state.on?.dismiss?.();
      return;
    default:
  }
}

/* Do what the row does.
 *
 * The default is a synthetic click, so the keyboard and the pointer travel the
 * same handler. `stopPropagation` is supplied because every one of these rows
 * calls it — the document's own listener closes pickers on any click that
 * missed one, and a click this file invented is not a click that missed. */
function keyNavActivate(name) {
  const state = keyNavState.get(name);
  const row = keyNavRow(name);
  if (!row) return;
  const custom = state?.on?.activate;
  if (custom) {
    custom(row, state.index);
    return;
  }
  keyNavClick(row);
}

/* Deliver a click to a row without a pointer.
 *
 * `el.click()` where the engine has one, which is the real thing and carries
 * the real event; the listener walk is the fallback for an element that is not
 * a button and so has no `click()` of its own. Both go through the row's own
 * handler, which is the whole point. */
function keyNavClick(row) {
  if (typeof row.click === 'function') {
    row.click();
    return;
  }
  for (const fn of row.listeners?.click ?? []) {
    fn({ target: row, preventDefault() {}, stopPropagation() {} });
  }
}

/* Mark an element as a row the keyboard stops at, with the label an assistive
 * client reads it out by.
 *
 * The label is separate from the text because several of these rows are drawn
 * out of glyphs — a Wi-Fi row's strength is four bars in a private-use
 * codepoint, and "󰤨 home connected" read aloud is a nonsense syllable
 * followed by the useful part. Where a row's text already says what it is,
 * the caller passes nothing and the text stands. */
function keyNavRowEl(el, label) {
  el.classList?.add?.(KEY_NAV_ROW);
  el.setAttribute?.('role', 'option');
  el.tabIndex = -1;
  if (label) el.setAttribute?.('aria-label', label);
  return el;
}
