/* SPDX-License-Identifier: MIT
 *
 * Battery widget picker: the power profile list.
 *
 * UPower and the profiles daemon live on the bus, which the page cannot
 * reach. The compositor reads both and sends `power.update`; this file
 * draws the list and sends `power.profile` back. The percentage itself
 * is painted by bar.js.
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
  renderPowerPicker();
}

function closePowerPicker() {
  if (!powerOpen) return;
  powerOpen = false;
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

  const list = document.createElement('div');
  list.className = 'power-list';

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
    row.addEventListener('click', (e) => {
      e.stopPropagation?.();
      send({ type: 'power.profile', profile: name });
      closePowerPicker();
    });
    list.append(row);
  }
  dialog.append(list);
  powerEl.append(dialog);

  /* Tell the compositor where the dialog is, so it draws that piece of the
     shell above the windows — see setOverlay's own comment in state.js. The
     dialog alone, not the docking box that spans the whole output. */
  setOverlay('power', dialog);
}
