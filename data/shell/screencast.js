/* SPDX-License-Identifier: MIT
 *
 * The screen-share chooser.
 *
 * An application asks to share the screen, the portal asks the compositor, and
 * the compositor asks the person sitting there. This draws the asking.
 *
 * It draws only. The highlight moves and the choice is made in the compositor,
 * which sends the whole list again each time — the shell is a page the
 * compositor composites and it receives no input of its own, so a chooser that
 * handled its own keys would handle none. That is the same split the overview
 * runs on: the shell decides what it looks like, the compositor decides what
 * the keyboard does.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* What the compositor last sent, so a redraw does not need it re-sent. */
let screencastPick = null;

/* Which request is on screen. An answer for one that has already been dealt
 * with must not put the chooser back up. */
let screencastPickId = 0;

function showScreencastPicker(message) {
  screencastPick = message;
  screencastPickId = message.id;
  renderScreencastPicker();
}

function hideScreencastPicker(id) {
  /* Only the one that is up. A late 'done' for an earlier request would
     otherwise take down the chooser that replaced it. */
  if (id !== undefined && id !== screencastPickId) return;
  screencastPick = null;
  screencastPickId = 0;
  renderScreencastPicker();
}

function renderScreencastPicker() {
  /* replaceChildren rather than clearing textContent: the second draws over
     the first only if the first is really gone, and a chooser that stacked
     would keep the old highlight visible under the new one. */
  screencastEl.replaceChildren();
  if (!screencastPick) {
    screencastEl.hidden = true;
    /* Nothing above the windows any more. Without this the compositor keeps
       drawing the piece of the shell the chooser was in, over whatever is
       there now. */
    reportScreencastRect(null);
    return;
  }
  screencastEl.hidden = false;

  /* Over the monitor being looked at, rather than centred across all of them:
     the shell is one page spanning the whole layout, so a dialog centred in it
     lands in the middle of the desk — between two monitors, on the usual
     two-monitor desk. */
  const output = outputs.get(activeOutputName());
  if (output?.rect) {
    Object.assign(screencastEl.style, {
      left: `${output.rect.x}px`,
      top: `${output.rect.y}px`,
      width: `${output.rect.width}px`,
      height: `${output.rect.height}px`,
    });
  }

  const dialog = document.createElement('div');
  dialog.className = 'screencast-dialog';

  const title = document.createElement('div');
  title.className = 'screencast-title';
  title.textContent = 'Share your screen';
  dialog.append(title);

  const help = document.createElement('div');
  help.className = 'screencast-help';
  help.textContent = 'An application is asking for a picture of this desktop.';
  dialog.append(help);

  const list = document.createElement('div');
  list.className = 'screencast-list';

  const sources = Array.isArray(screencastPick.sources)
    ? screencastPick.sources
    : [];
  sources.forEach((source, index) => {
    const row = document.createElement('div');
    row.className = 'screencast-source';
    if (index === screencastPick.selected) row.classList.add('selected');
    row.dataset.kind = source.kind;

    const label = document.createElement('div');
    label.className = 'screencast-label';
    /* A window with no title is still a window, and a row with no text in it
       looks like a bug rather than a choice. */
    label.textContent = source.label || (source.kind === 'output'
      ? 'a monitor'
      : 'an untitled window');
    row.append(label);

    if (source.detail) {
      const detail = document.createElement('div');
      detail.className = 'screencast-detail';
      detail.textContent = source.detail;
      row.append(detail);
    }
    list.append(row);
  });
  dialog.append(list);

  const keys = document.createElement('div');
  keys.className = 'screencast-keys';
  keys.textContent = '↑↓ choose · Enter share · Esc cancel';
  dialog.append(keys);

  screencastEl.append(dialog);

  /* Keep the highlight in view. The list of windows is as long as the desktop
     is busy, and the one that is highlighted is the only row that matters. */
  const selected = list.querySelector('.selected');
  if (selected && selected.scrollIntoView) {
    selected.scrollIntoView({ block: 'nearest' });
  }

  reportScreencastRect(dialog);
}

/* Tell the compositor where the dialog is, so it can draw that piece of the
 * shell above the windows.
 *
 * The shell is one buffer underneath the whole desktop — every window is
 * painted into a hole in it — so a dialog it draws is behind them by
 * construction. Naming the rectangle is what lets the compositor draw the
 * same buffer a second time, cropped to this, on top. The dialog alone rather
 * than the whole screen: what is being chosen between is the windows, and
 * covering them to ask about them would be a strange way to ask. */
function reportScreencastRect(dialog) {
  /* One of several things that can float now, so it goes through the shared
     list rather than owning the compositor's single slot. */
  setOverlay('screencast', dialog);
}
