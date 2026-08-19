/* SPDX-License-Identifier: MIT
 *
 * The screen-share chooser, and the remote-control one.
 *
 * An application asks to share the screen, the portal asks the compositor, and
 * the compositor asks the person sitting there. This draws the asking.
 *
 * Two questions through one dialog, because there is one keyboard and one
 * person and the compositor has one chooser slot. A 'screencast.pick' with a
 * 'devices' list is the RemoteDesktop portal rather than ScreenCast: the
 * application is asking to drive this machine, not only to watch it. The rows
 * and the keys are identical — it may also have asked for a picture, in which
 * case there is a list to choose from — and what changes is the sentence at
 * the top, which is the part somebody is actually agreeing to.
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

/* A list of words as somebody would say it: 'a', 'a and b', 'a, b and c'.
 *
 * Written out rather than joined with commas because this line is a sentence
 * the person is being asked to agree to, and 'keyboard, mouse' in the middle of
 * one reads as a fragment of a form. */
function listPhrase(words) {
  if (words.length <= 1) return words.join('');
  return `${words.slice(0, -1).join(', ')} and ${words[words.length - 1]}`;
}

/* What to call a source whose own name is empty.
 *
 * Only the two kinds that borrow a client's text can be empty: a window with
 * no title, and — in principle — a monitor with no name. The kinds that stand
 * for something the compositor decides moment to moment ('all-outputs',
 * 'follow-window', 'follow-output') arrive named, because there is nothing
 * else they could be called. */
const SCREENCAST_UNNAMED = {
  output: 'a monitor',
  window: 'an untitled window',
};

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
  /* Whether this is the chooser appearing or the chooser being redrawn.
   *
   * The compositor owns the highlight and answers every arrow key by sending
   * the whole list again, so this function runs once per keypress and builds a
   * new dialog element every time. An entrance declared on the element — which
   * is what this was — therefore played again on each move down the list.
   * Asked before the rebuild, because afterwards there is nothing left to ask. */
  const wasUp = !screencastEl.hidden;

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

  /* What is being asked for. An empty list — or a compositor too old to send
     one — is the screen share this dialog has always been. */
  const devices = Array.isArray(screencastPick.devices)
    ? screencastPick.devices
    : [];
  const remote = devices.length > 0;

  const title = document.createElement('div');
  title.className = 'screencast-title';
  title.textContent = remote
    ? 'Let an application control this computer'
    : 'Share your screen';
  dialog.append(title);

  const help = document.createElement('div');
  help.className = 'screencast-help';
  help.textContent = remote
    ? 'An application is asking to use this desktop as if it were sitting here.'
    : 'An application is asking for a picture of this desktop.';
  dialog.append(help);

  /* The device set, spelled out and marked as the warning it is.
   *
   * Separate from the help line rather than folded into it because it is the
   * specific thing being agreed to and the one part that varies between one
   * request and the next: a session that only wants the mouse is a different
   * decision from one that wants the keyboard too. The compositor sends the
   * words already in a fixed order, so this joins rather than sorts. */
  if (remote) {
    const grant = document.createElement('div');
    grant.className = 'screencast-devices';
    grant.textContent = `It would be able to use the ${listPhrase(devices)}.`;
    dialog.append(grant);
  }

  const list = document.createElement('div');
  list.className = 'screencast-list';

  const sources = Array.isArray(screencastPick.sources)
    ? screencastPick.sources
    : [];
  /* A remote session that also asked for a picture has rows to choose between,
     and without a word here the list reads as a second, unrelated question.
     One that did not has no rows at all, and the dialog is a plain yes or no. */
  if (remote && sources.length > 0) {
    const heading = document.createElement('div');
    heading.className = 'screencast-help';
    heading.textContent = 'It would also see:';
    dialog.append(heading);
  }
  sources.forEach((source, index) => {
    const row = document.createElement('div');
    row.className = 'screencast-source';
    if (index === screencastPick.selected) row.classList.add('selected');
    row.dataset.kind = source.kind;

    const label = document.createElement('div');
    label.className = 'screencast-label';
    /* A window with no title is still a window, and a row with no text in it
       looks like a bug rather than a choice.
       The compositor always names the three kinds that point at nothing in
       particular — there is no title to fall back from — so only the two that
       carry a client's own text need standing in for. */
    label.textContent = source.label
      || SCREENCAST_UNNAMED[source.kind]
      || 'something to share';
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
  /* The verb has to match what Enter does. 'Enter share' over a dialog that
     hands somebody the keyboard would be describing the wrong consent, and a
     list with no rows in it has nothing to choose between. */
  keys.textContent = [
    sources.length > 1 ? '↑↓ choose' : null,
    remote ? 'Enter allow' : 'Enter share',
    'Esc cancel',
  ]
    .filter(Boolean)
    .join(' · ');
  dialog.append(keys);

  screencastEl.append(dialog);
  if (!wasUp) animateScreencastIn(dialog, [...list.children]);

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
