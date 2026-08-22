/* SPDX-License-Identifier: MIT
 *
 * The launcher.
 *
 * The list is the compositor's: the shell is a web page, and a web page
 * cannot read XDG_DATA_DIRS any more than it can read /proc. The compositor
 * scans the .desktop directories, filters on the text typed here, and sends
 * back the rows to draw — a name, an icon already resolved to a data: URL,
 * and what the entry says it is for. What this file owns is the list and the
 * filter field, and what it sends back is the filter and the id of the row
 * that was chosen. The command line never crosses the wire: the compositor
 * starts what it scanned, with an activation token minted for the process,
 * so the window that appears opens focused rather than behind whatever the
 * user moved on to.
 *
 * The filter field is real typed text, on the same terms as the network
 * picker's passphrase box: the shell only has the keyboard once something
 * gives it the keyboard, so the picker asks for it when it opens and hands
 * it back to the window that had it when it goes away.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

let launcherOpen = false;
let launcherApps = [];
let launcherFilter = '';
/* The list the rows are drawn from, counted in queries. A launch carries it
   back: a query is sent on every keystroke and not waited for, so the list a
   row is drawn from may be replaced before the Enter that chose it lands —
   and a launch the compositor has moved past is refused rather than started. */
let launcherGeneration = 0;
/* The list container of the open dialog. The dialog is built once when the
 * picker opens and the list rebuilt on every answer, so the field being typed
 * into survives its own keystrokes. */
let launcherListEl = null;

/* Open it, or take it down. Bound to Mod4+d by default, which arrives here as
 * a shell command — the compositor routes the keys, so this is never a key
 * handler. */
function toggleLauncher() {
  if (launcherOpen) {
    closeLauncher();
    return;
  }
  launcherOpen = true;
  /* The other pickers go, the way a menu does: two dialogs over the same
     windows is two answers to one question, and the clipboard picker's rows
     and the centre's are drawn in the same place this list is. */
  closeClipboard();
  closeNotificationCentre();
  /* The last list and the last filter are kept, not reset: the picker opens
     on what it last showed, and the query it sends — with the filter it still
     holds — answers with the same list a moment later. Resetting would be a
     blank flash on the way to the same rows, or a filtered list under an
     emptied field until the answer came back. */
  /* Take the keyboard, remembering who had it. Both halves are keys.js's
     now, on the same terms every other surface gets them — this file had the
     only copy of that dance and the network picker had the second. */
  keyNavOpen('launcher');
  send({ type: 'launcher.query', filter: launcherFilter || undefined });
  renderLauncher();
}

function closeLauncher() {
  if (!launcherOpen) return;
  launcherOpen = false;
  /* Gives the keyboard back to whatever had it — see keyNavClose. */
  keyNavClose('launcher');
  launcherListEl = null;
  launcherEl.replaceChildren();
  launcherEl.hidden = true;
  setOverlay('launcher', null);
}

/* What the compositor last answered. The list container alone is rebuilt —
 * the dialog around it, and the filter field inside it, must not be: a
 * rebuild on every keystroke would throw away the field that is being typed
 * into, halfway through a word. */
function applyLauncher(apps, generation) {
  launcherApps = Array.isArray(apps) ? apps : [];
  if (typeof generation === 'number') launcherGeneration = generation;
  if (!launcherOpen) return;
  renderLauncherList();
}

function renderLauncher() {
  launcherEl.replaceChildren();
  launcherEl.hidden = false;

  /* Over the output being looked at, rather than centred across all of them:
     the shell is one page spanning the whole layout, so a dialog centred in
     it lands in the middle of the desk — between two monitors, on the usual
     two-monitor desk. `#launcher` is only the docking box; the dialog inside
     it is what the flex rules put where a launcher is looked for. */
  const output = outputs.get(activeOutputName());
  if (output?.rect) {
    Object.assign(launcherEl.style, {
      left: `${output.rect.x}px`,
      top: `${output.rect.y}px`,
      width: `${output.rect.width}px`,
      height: `${output.rect.height}px`,
    });
  }

  const dialog = document.createElement('div');
  dialog.className = 'launcher-dialog';
  dialog.setAttribute('role', 'dialog');
  dialog.setAttribute('aria-modal', 'true');
  dialog.setAttribute('aria-label', 'Launcher');

  /* A click inside the dialog must not reach the document listener that
     closes pickers — including a click on the field itself, which is how
     somebody puts the caret back where they left it. */
  dialog.addEventListener('click', (e) => e.stopPropagation?.());

  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'launcher-input';
  input.placeholder = 'Type to filter';
  input.value = launcherFilter;
  input.setAttribute('role', 'combobox');
  input.setAttribute('aria-label', 'Filter applications');
  input.setAttribute('aria-expanded', 'true');
  input.addEventListener('input', () => {
    const filter = String(input.value ?? '');
    if (filter === launcherFilter) return;
    launcherFilter = filter;
    /* Back to the top: the list about to arrive is a different list, and
       leaving the highlight on row four of the old one would start whatever
       happened to land there. */
    keyNavSelect('launcher', 0);
    send({ type: 'launcher.query', filter });
  });
  dialog.append(input);

  const list = document.createElement('div');
  list.className = 'launcher-list';
  list.setAttribute('role', 'listbox');
  list.setAttribute('aria-label', 'Applications');
  launcherListEl = list;
  dialog.append(list);

  const hint = document.createElement('div');
  hint.className = 'launcher-hint';
  hint.textContent = '↑↓ choose · Enter launch · Esc close';
  dialog.append(hint);

  launcherEl.append(dialog);

  /* The keys go on the field rather than on the dialog, and the rows are
     named rather than found under it: this is the one surface whose dialog
     outlives its own list. The field is where the keyboard is — it is real
     typed text — and a second listener on the dialog would see every press
     again as it bubbled up out of the field. */
  bindKeyNav('launcher', dialog, {
    keysOn: input,
    focus: input,
    rows: () => keyNavRows(launcherListEl),
    dismiss: closeLauncher,
    activate: () => launchSelected(),
  });

  renderLauncherList();
  /* Tell the compositor where the dialog is, so it draws that piece of the
     shell above the windows — see setOverlay's own comment in state.js. The
     dialog alone, not the docking box that spans the whole output. */
  setOverlay('launcher', dialog);
  input.focus?.();
}

function renderLauncherList() {
  const list = launcherListEl;
  if (!list) return;
  list.replaceChildren();

  if (launcherApps.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'launcher-empty';
    /* Two reasons for an empty list and the shell cannot tell them apart, so
       it says the one that is true either way. */
    empty.textContent = launcherFilter
      ? `Nothing matches “${launcherFilter}”.`
      : 'No applications found.';
    list.append(empty);
    return;
  }

  launcherApps.forEach((app) => {
    const row = document.createElement('button');
    row.className = 'launcher-row';
    /* Named for the reader rather than left to the row's own text: the icon
       is a letter where the entry had no picture, and "F Firefox web,
       browser" is what a reader would otherwise say. */
    keyNavRowEl(row, app.detail ? `${app.name}, ${app.detail}` : app.name);

    const icon = document.createElement('span');
    icon.className = 'launcher-icon';
    if (app.icon) {
      const img = document.createElement('img');
      img.src = app.icon;
      img.alt = '';
      icon.append(img);
    } else {
      /* The tray's fallback for an item with no icon: a letter. */
      icon.textContent = (app.name || '?').trim().charAt(0).toUpperCase();
    }
    row.append(icon);

    const text = document.createElement('span');
    text.className = 'launcher-text';
    const name = document.createElement('span');
    name.className = 'launcher-name';
    name.textContent = app.name;
    text.append(name);
    if (app.detail) {
      const detail = document.createElement('span');
      detail.className = 'launcher-detail';
      detail.textContent = app.detail;
      text.append(detail);
    }
    row.append(text);

    row.addEventListener('click', (e) => {
      e.stopPropagation?.();
      launchApp(app.id);
    });
    list.append(row);
  });

  /* The highlight painted back on, and the row it lands on scrolled into
     view. The list was just replaced, so nothing on the old elements survives
     — see keys.js on why the index is kept out here rather than on them. */
  keyNavRefresh('launcher');
}

function launchSelected() {
  if (launcherApps.length === 0) return;
  const app = launcherApps[Math.min(keyNavIndex('launcher'), launcherApps.length - 1)];
  launchApp(app.id);
}

function launchApp(id) {
  /* The generation the row was drawn from, so the compositor can tell a
     launch for the list on screen from one for the list a query that has not
     been answered yet replaced. */
  send({ type: 'launcher.launch', id, generation: launcherGeneration });
  closeLauncher();
}
