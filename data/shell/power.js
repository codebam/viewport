/* SPDX-License-Identifier: MIT
 *
 * Battery widget picker: the power profile list and the power rows.
 *
 * UPower and the profiles daemon live on the bus, which the page cannot
 * reach. The compositor reads both and sends `power.update`; this file
 * draws the list, then the rows that are always on — suspend, power off,
 * reboot, quit — and sends `power.profile`, `power.action` or `quit` back.
 * The percentage itself is painted by bar.js.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

function togglePowerPicker() {
  if (powerOpen) {
    closePowerPicker();
    return;
  }
  powerOpen = true;
  /* The keyboard, so the arrow keys reach the rows rather than the window
     under the dialog. The battery widget opens this with the pointer, which
     is exactly why it needed asking for: a surface opened by a click has no
     reason to have the keyboard and every reason to need it. See keys.js. */
  keyNavOpen('power');
  renderPowerPicker();
}

function closePowerPicker() {
  if (!powerOpen) return;
  powerOpen = false;
  keyNavClose('power');
  powerEl.replaceChildren();
  powerEl.hidden = true;
  setOverlay('power', null);
}

function applyPower(snapshot) {
  powerState = snapshot && typeof snapshot === 'object' ? snapshot : null;
  renderBarsWidgets();
  if (powerOpen) renderPowerPicker();
}

function renderPowerPicker() {
  powerEl.replaceChildren();
  powerEl.hidden = false;

  /* Over the output being looked at, rather than centred across all of them —
     see renderClipboard's own comment for why 50%/50% is the wrong centre on
     a multi-monitor desk. `#power-picker` is only the docking box; the dialog
     inside it is what `.power-dialog`'s flex centring puts in the middle of
     that box. */
  const output = outputs.get(activeOutputName());
  if (output?.rect) {
    Object.assign(powerEl.style, {
      left: `${output.rect.x}px`,
      top: `${output.rect.y}px`,
      width: `${output.rect.width}px`,
      height: `${output.rect.height}px`,
    });
  }

  const dialog = document.createElement('div');
  dialog.className = 'power-dialog';
  dialog.setAttribute('role', 'dialog');
  dialog.setAttribute('aria-modal', 'true');
  dialog.setAttribute('aria-label', 'Power');
  /* A click inside the dialog must not reach the document listener that
     closes pickers. The rows already stop their own, but the dialog now has
     gaps between them that a click can land in — a separator, the padding
     around the list — and closing on those would be a surface that dismissed
     itself when somebody clicked next to the row they were aiming at. */
  dialog.addEventListener('click', (e) => e.stopPropagation?.());

  const list = document.createElement('div');
  list.className = 'power-list';
  list.setAttribute('role', 'listbox');
  list.setAttribute('aria-label', 'Power profiles');

  const profiles = powerState?.profiles ?? [];
  if (profiles.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'power-empty';
    empty.textContent = 'No power profiles.';
    list.append(empty);
  }

  for (const name of profiles) {
    const row = document.createElement('button');
    row.className = 'power-row';
    if (name === powerState?.profile) row.classList.add('active');
    row.textContent = name;
    keyNavRowEl(row);
    /* Which profile is in force, said rather than only coloured: `.active`
       paints a row and nothing in the accessibility tree carries a class. */
    row.setAttribute('aria-checked', name === powerState?.profile ? 'true' : 'false');
    row.addEventListener('click', (e) => {
      e.stopPropagation?.();
      send({ type: 'power.profile', profile: name });
      closePowerPicker();
    });
    list.append(row);
  }
  dialog.append(list);

  /* The rows that are on whether or not the daemon offered a profile: a
     machine with a lid set to off and no battery daemon is still a machine,
     and there is still one thing that asks it to lie down, to go away, and to
     come back. Suspend is the same call the lid policy makes on a hinge close;
     power off and reboot are its siblings; and quitting is this compositor's
     own move, which goes out as its own message rather than a fourth verb —
     two ways to leave would mean two things, and only one of them is true. */
  const sep = document.createElement('div');
  sep.className = 'power-sep';
  dialog.append(sep);

  const verbs = [
    /* Lock first, because it is the only one of these that is not a way of
       ending the session, and it is the one somebody reaches for most. It
       sends `session.lock` rather than a `power.action` verb: the other three
       go out to logind, and locking is this compositor's own move — what it
       means is `idle.lock_command` if the config names one and the shell's own
       lock screen if it does not, and that decision lives in one place on the
       compositor's side (`lock_session`) rather than being made here. */
    ['Lock', 'lock', false],
    ['Suspend', 'suspend', false],
    ['Power Off', 'poweroff', true],
    ['Reboot', 'reboot', true],
    ['Quit', 'quit', true],
  ];
  for (const [label, verb, danger] of verbs) {
    const row = document.createElement('button');
    row.className = 'power-row' + (danger ? ' danger' : '');
    row.textContent = label;
    keyNavRowEl(row);
    row.addEventListener('click', (e) => {
      e?.stopPropagation?.();
      if (verb === 'lock') {
        send({ type: 'session.lock' });
      } else if (verb === 'quit') {
        send({ type: 'quit' });
      } else {
        send({ type: 'power.action', action: verb });
      }
      closePowerPicker();
    });
    dialog.append(row);
  }

  powerEl.append(dialog);

  /* Arrows to choose, Enter to do it, Escape to change your mind. No Delete
     verb: nothing in this list can be forgotten, and three of the four rows
     end the session. */
  bindKeyNav('power', dialog, { dismiss: closePowerPicker });

  /* Tell the compositor where the dialog is, so it draws that piece of the
     shell above the windows — see setOverlay's own comment in state.js. The
     dialog alone, not the docking box that spans the whole output. */
  setOverlay('power', dialog);
}
