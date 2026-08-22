/* SPDX-License-Identifier: MIT
 *
 * The notification centre.
 *
 * A notification is a popup and then it is nothing: one that arrived over a
 * fullscreen window, or while the screens were blanked, was never seen and
 * cannot be gone back to. This is where it can be — the list of what has been
 * notified, drawn from the copy the compositor keeps.
 *
 * The copy is the compositor's rather than this page's, and deliberately: the
 * shell is restarted when it crashes and reloaded when its stylesheet changes,
 * and a history kept here would be lost by both. So this file draws a list it
 * is given and sends back two verbs — forget one, forget all. See
 * `notification.list` in docs/ipc.md.
 *
 * The popups themselves are session.js. They are the same notifications and a
 * different job: one is a thing on screen with a timer, this is a record with
 * no timer at all.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* Open it, or take it down. Bound to Mod4+Shift+m by default, which arrives
 * here as a shell command — the compositor routes the chord, so opening is
 * never a key handler. Steering inside it is: the centre takes the keyboard
 * on the way up so that the arrow keys reach the rows rather than the window
 * underneath, and hands it back on the way down. See keys.js. */
function toggleNotificationCentre() {
  if (notificationCentreOpen) {
    closeNotificationCentre();
    return;
  }
  notificationCentreOpen = true;
  keyNavOpen('notifications-centre');
  /* Asked for on open rather than kept up to date: the compositor sends the
     history whenever it changes anyway, and a centre that is not on screen has
     no use for it. */
  send({ type: 'notification.list' });
  renderNotificationCentre();
}

function closeNotificationCentre() {
  if (!notificationCentreOpen) return;
  notificationCentreOpen = false;
  keyNavClose('notifications-centre');
  notificationCentreEl.replaceChildren();
  notificationCentreEl.hidden = true;
  setOverlay('notifications-centre', null);
}

/* What the compositor last sent. Drawn only if the centre is open — the
 * message arrives on every notification, and redrawing a hidden element would
 * be a composited frame of the whole desktop per message. */
function applyNotificationHistory(entries) {
  notificationHistory = Array.isArray(entries) ? entries : [];
  if (notificationCentreOpen) renderNotificationCentre();
}

/* How long ago, in the shortest form that is still true.
 *
 * Seconds are not drawn: a message from eleven seconds ago and one from
 * forty are both "just now" to somebody reading a list. Anything older than a
 * day is a date, because "37h" is arithmetic and "Tue 09:14" is a memory.
 *
 * Zero means the notification carries no stamp — an older compositor, or a
 * test that built one by hand — and draws as nothing rather than as 1970. */
function notificationAge(at, now = Date.now()) {
  if (!at) return '';
  const seconds = Math.max(0, Math.floor(now / 1000) - at);
  if (seconds < 60) return 'just now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const when = new Date(at * 1000);
  return when.toLocaleString(undefined, {
    weekday: 'short',
    hour: '2-digit',
    minute: '2-digit',
  });
}

/* One row per notification, newest first, which is the order the compositor
 * keeps them in. Rebuilt rather than synced: this is drawn when it opens and
 * when the history changes under it, not on a timer, and fifty rows is nothing
 * to build. */
function renderNotificationCentre() {
  notificationCentreEl.replaceChildren();
  notificationCentreEl.hidden = false;

  /* Over the output being looked at, for the reason renderClipboard gives:
     the shell is one page spanning the whole layout, so a dialog centred in it
     lands between the monitors rather than on one. */
  const output = outputs.get(activeOutputName());
  if (output?.rect) {
    Object.assign(notificationCentreEl.style, {
      left: `${output.rect.x}px`,
      top: `${output.rect.y}px`,
      width: `${output.rect.width}px`,
      height: `${output.rect.height}px`,
    });
  }

  const dialog = document.createElement('div');
  dialog.className = 'notification-centre-dialog';
  dialog.setAttribute('role', 'dialog');
  dialog.setAttribute('aria-modal', 'true');
  dialog.setAttribute('aria-label', 'Notifications');
  /* A click inside the dialog stays inside it. The clipboard picker leaves
     this to its rows, because every row there is a button and a click that
     missed one is a click that meant to close it; a centre is mostly text, so
     the same rule would make reading a long message a way to dismiss the list
     it is in. What closes it is a click outside the dialog, which the
     document's own listener sees. */
  dialog.addEventListener('click', (e) => e.stopPropagation?.());

  const list = document.createElement('div');
  list.className = 'notification-centre-list';
  list.setAttribute('role', 'listbox');
  /* Announced as it changes, because it does so on its own: a notification
     arriving while the centre is open adds a row nobody asked for, and a
     reader that is not told about it goes on reading a list that has moved
     under it. `polite` and not `assertive` — the popup is the interruption,
     this is the record of one. */
  list.setAttribute('aria-live', 'polite');

  if (notificationHistory.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'notification-centre-empty';
    /* Two reasons for an empty list — nothing has notified, or the record is
       turned off by `notifications.history: 0` — and the shell is not told
       which, so it says the one that is true either way. */
    empty.textContent = 'No notifications.';
    list.append(empty);
  }

  for (const entry of notificationHistory) {
    list.append(notificationRow(entry));
  }
  dialog.append(list);

  const footer = document.createElement('div');
  footer.className = 'notification-centre-footer';
  const clear = document.createElement('button');
  clear.className = 'notification-centre-clear';
  clear.textContent = 'Clear all';
  keyNavRowEl(clear);
  clear.addEventListener('click', (e) => {
    e.stopPropagation?.();
    /* No id is "forget everything", the same shape `clipboard.forget` has.
       Nothing is closed by it: the senders were told when their popups went,
       and being tidied out of a list they cannot see is not their business. */
    send({ type: 'notification.forget' });
  });
  footer.append(clear);
  dialog.append(footer);

  notificationCentreEl.append(dialog);

  /* Arrows to choose, Enter for the row's default action where it has one,
     Delete to forget the row, Escape to go. A row's own action buttons are
     not stops of their own: they are a second axis, and a list where Down
     sometimes moves to the next notification and sometimes to the second
     button of this one is a list nobody can predict. Enter takes the default
     action, which is what clicking the row does. */
  bindKeyNav('notifications-centre', dialog, {
    dismiss: closeNotificationCentre,
    remove: (row) => row._keyRemove?.(),
  });

  /* Tell the compositor where the dialog is, so it draws that piece of the
     shell above the windows — see setOverlay's own comment in state.js. The
     dialog alone, not the docking box that spans the output. */
  setOverlay('notifications-centre', dialog);
}

function notificationRow(entry) {
  const row = document.createElement('div');
  row.className = 'notification-centre-row urgency-' + (entry.urgency ?? 1);
  /* Read out as one thing rather than as four fragments. The visible row is
     an application name, an age, a summary and a body in that order, which is
     the order somebody scanning wants; read aloud, the summary is the part
     that says whether the rest is worth hearing, so it comes first. */
  keyNavRowEl(row, [entry.summary, entry.body, entry.app_name,
    notificationAge(entry.at)].filter(Boolean).join(', '));
  /* What Delete does here, written on the element for the reason the
     clipboard's rows give: the list is rebuilt on every message. */
  row._keyRemove = () => send({ type: 'notification.forget', id: entry.id });

  const head = document.createElement('div');
  head.className = 'notification-centre-head';

  const app = document.createElement('span');
  app.className = 'notification-centre-app';
  app.textContent = entry.app_name || 'notification';
  head.append(app);

  const age = document.createElement('span');
  age.className = 'notification-centre-age';
  age.textContent = notificationAge(entry.at);
  head.append(age);

  const forget = document.createElement('button');
  forget.className = 'notification-centre-forget';
  forget.textContent = '✕';
  forget.addEventListener('click', (e) => {
    e.stopPropagation?.();
    send({ type: 'notification.forget', id: entry.id });
  });
  head.append(forget);
  row.append(head);

  if (entry.summary) {
    const summary = document.createElement('div');
    summary.className = 'notification-centre-summary';
    summary.textContent = entry.summary;
    row.append(summary);
  }
  if (entry.body) {
    const body = document.createElement('div');
    body.className = 'notification-centre-body';
    /* textContent, not innerHTML, for the reason showNotification gives: a
       body is text from an arbitrary program and this is a web page. The
       centre is the worse place to get this wrong, because what is in it was
       written by something that has already stopped running. */
    body.textContent = entry.body;
    row.append(body);
  }

  const actions = (entry.actions ?? []).filter((a) => a.key !== 'default');
  if (actions.length > 0) {
    const buttons = document.createElement('div');
    buttons.className = 'notification-centre-actions';
    for (const action of actions) {
      const button = document.createElement('button');
      button.textContent = action.label || action.key;
      button.addEventListener('click', (e) => {
        e.stopPropagation?.();
        /* The same message the popup sends. The application is told its
           action was invoked whether the button pressed was on a popup or on
           a row here — from the sender's side there is no difference, and the
           compositor drops an acted-on notification from the history, so the
           row goes with the next `notification.history`. */
        send({ type: 'notification.action', id: entry.id, action: action.key });
      });
      buttons.append(button);
    }
    row.append(buttons);
  }

  /* Clicking the row invokes the default action where there is one, which is
     what clicking the popup does. Where there is none the row does nothing:
     a click that silently threw the message away would be a way to lose the
     thing the centre exists to keep. */
  const fallback = (entry.actions ?? []).find((a) => a.key === 'default');
  if (fallback) {
    row.addEventListener('click', () => {
      send({ type: 'notification.action', id: entry.id, action: 'default' });
    });
    row.classList.add('notification-centre-clickable');
  }

  return row;
}
