/* SPDX-License-Identifier: MIT
 *
 * The clipboard history picker.
 *
 * A Wayland selection is an offer from the client that owns it: close the
 * terminal you copied from and the clipboard is empty. The compositor brokers
 * every selection on the session, so it keeps the last few and hands them over
 * here — this file is the list, and nothing more. Nothing in the shell reads a
 * selection or knows what a mime type is.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* Open it, or take it down. Bound to Mod4+Shift+v by default, which arrives
 * here as a shell command — the compositor routes the chord, so opening is
 * never a key handler. Steering inside it is: the picker takes the keyboard
 * on the way up so that the arrow keys reach the rows rather than the window
 * underneath, and hands it back on the way down. See keys.js. */
function toggleClipboard() {
  if (clipboardOpen) {
    closeClipboard();
    return;
  }
  clipboardOpen = true;
  keyNavOpen('clipboard');
  /* Asked for on open rather than kept up to date: the compositor sends the
     history whenever it changes anyway, and a picker that is not on screen has
     no use for it. */
  send({ type: 'clipboard.query' });
  renderClipboard();
}

function closeClipboard() {
  if (!clipboardOpen) return;
  clipboardOpen = false;
  keyNavClose('clipboard');
  clipboardEl.replaceChildren();
  clipboardEl.hidden = true;
  setOverlay('clipboard', null);
}

/* What the compositor last sent. Drawn only if the picker is open — the
 * message arrives on every copy, and redrawing a hidden element would be a
 * composited frame of the whole desktop per copy. */
function applyClipboard(entries) {
  clipboardEntries = Array.isArray(entries) ? entries : [];
  if (clipboardOpen) renderClipboard();
}

/* One row per entry, newest first, which is the order the compositor keeps
 * them in. Rebuilt rather than synced: this is drawn when it opens and when
 * the history changes under it, not on a timer, and a dozen rows is nothing to
 * build. */
function renderClipboard() {
  clipboardEl.replaceChildren();
  clipboardEl.hidden = false;

  /* Over the output being looked at, rather than centred across all of them:
     the shell is one page spanning the whole layout, so a dialog centred in
     it lands in the middle of the desk — between two monitors, on the usual
     two-monitor desk. `#clipboard` is only the docking box; the dialog inside
     it is what `.clipboard-dialog`'s flex centring puts in the middle of that
     box, the same way renderScreencastPicker positions `#screencast`. */
  const output = outputs.get(activeOutputName());
  if (output?.rect) {
    Object.assign(clipboardEl.style, {
      left: `${output.rect.x}px`,
      top: `${output.rect.y}px`,
      width: `${output.rect.width}px`,
      height: `${output.rect.height}px`,
    });
  }

  const dialog = document.createElement('div');
  dialog.className = 'clipboard-dialog';
  /* A dialog says so, and says what it is for. Nothing in the shell is a
     document landmark an assistive client can arrive at by wandering — every
     one of these surfaces appears in answer to a chord — so the label is the
     only thing that will be read when it does. */
  dialog.setAttribute('role', 'dialog');
  dialog.setAttribute('aria-modal', 'true');
  dialog.setAttribute('aria-label', 'Clipboard history');

  const list = document.createElement('div');
  list.className = 'clipboard-list';
  list.setAttribute('role', 'listbox');
  list.setAttribute('aria-label', 'Copied items');

  if (clipboardEntries.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'clipboard-empty';
    /* Two reasons for an empty list and the shell cannot tell them apart, so
       it says the one that is true either way. */
    empty.textContent = 'Nothing copied yet.';
    list.append(empty);
  }

  for (const entry of clipboardEntries) {
    const row = document.createElement('button');
    row.className = 'clipboard-row';
    keyNavRowEl(row);

    const text = document.createElement('span');
    text.className = 'clipboard-text';
    /* Whitespace collapsed for the row only: what is pasted is what was
       copied, and a multi-line entry drawn as it stands would make one row as
       tall as the screen. */
    text.textContent = String(entry.text ?? '').replace(/\s+/g, ' ').trim();
    row.append(text);

    const forget = document.createElement('span');
    forget.className = 'clipboard-forget';
    forget.textContent = '✕';
    row.append(forget);

    /* What Delete does on this row. Written on the element rather than kept
       beside the list, because the list is rebuilt on every copy and a
       parallel array would have to be rebuilt with it. */
    row._keyRemove = () => send({ type: 'clipboard.forget', id: entry.id });

    row.addEventListener('click', (e) => {
      e.stopPropagation?.();
      /* The ✕ forgets the entry; anywhere else on the row pastes it. One
         listener, because the target says which was hit — and a second
         element with its own listener would be a second thing to keep in step
         with the row it belongs to. */
      if (e.target === forget) {
        send({ type: 'clipboard.forget', id: entry.id });
        return;
      }
      send({ type: 'clipboard.paste', id: entry.id });
      closeClipboard();
    });
    list.append(row);
  }
  dialog.append(list);

  const footer = document.createElement('div');
  footer.className = 'clipboard-footer';
  const clear = document.createElement('button');
  clear.className = 'clipboard-clear';
  clear.textContent = 'Forget everything';
  keyNavRowEl(clear);
  clear.addEventListener('click', (e) => {
    e.stopPropagation?.();
    /* What somebody asks for after copying a password. The selection itself is
       left where it is: taking the clipboard away from the application that
       owns it is not the shell's to do. */
    send({ type: 'clipboard.forget' });
  });
  footer.append(clear);
  dialog.append(footer);

  clipboardEl.append(dialog);

  /* Arrows to choose, Enter to paste, Delete to forget the row under the
     keyboard, Escape to go. Bound after the rows exist because the binding
     paints the selection on as it goes. */
  bindKeyNav('clipboard', dialog, {
    dismiss: closeClipboard,
    remove: (row) => row._keyRemove?.(),
  });

  /* Tell the compositor where the dialog is, so it draws that piece of the
     shell above the windows — see setOverlay's own comment in state.js. The
     dialog alone, not the docking box that spans the whole output: what is
     being chosen from is text, and covering the windows to offer it would be
     a strange way to ask. */
  reportClipboardRect(dialog);
}

function reportClipboardRect(dialog) {
  setOverlay('clipboard', dialog);
}
