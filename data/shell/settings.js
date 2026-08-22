/* SPDX-License-Identifier: MIT
 *
 * The settings panel: the desktop's own way to change how it looks.
 *
 * `docs/configuration.md` opens by justifying two tiers of configuration with
 * "a settings UI cannot run on a display that is not working", and then names
 * a settings panel twice more as the thing the runtime setters exist for.
 * Nobody had written it. This is it.
 *
 * Three things are worth knowing before changing anything here.
 *
 * **Every change applies immediately and nothing is written to disk until
 * Save.** That is the runtime tier working as designed — `config.gaps`,
 * `config.border`, `config.wallpaper` and `config.dark_mode` change the
 * compositor's copy of the config and re-announce it, without touching the
 * file — and it is what makes this a panel rather than a form. You drag a
 * number and the desktop is already that shape; you decide you preferred it
 * before and you close the panel without saving. `config.save` writes the ones
 * you kept into `settings.json` beside the config file, which is applied over
 * the config file at the next start. See `crates/viewport/src/settings.rs` for
 * why that file and not the config file itself.
 *
 * **The panel draws no state of its own.** Every value on it is read out of
 * the last `config` event and the last `output.layout`, and a change is drawn
 * only once the compositor has echoed it back. A panel that drew what it had
 * just sent would show a value the compositor refused — a negative gap, a
 * wallpaper that is not there — as though it had been applied.
 *
 * **A display change is provisional.** A mode the panel will drive but the
 * display will not, a scale that puts this very dialog off the edge, a
 * rotation on the wrong screen: each of those ends with somebody looking at a
 * rectangle they can no longer read the undo button on. So the compositor
 * arms a revert on every display change and takes it back unless it is told
 * somebody can see it — see `arm_output_revert` in `state.rs`. This draws the
 * asking half: a bar across the dialog with Keep and Revert on it, which send
 * `output.confirm` and `output.revert`. Doing nothing is the same as Revert,
 * which is the property the whole arrangement is for.
 *
 * It is a dialog rather than a dropdown anchored to a bar item, unlike the
 * network picker and the power menu. Those two are lists of one kind of thing
 * and fit under the widget that opens them; this is five sections, one of
 * which is a row per monitor with a mode list in it, and a dropdown that tall
 * is a dialog wearing a hat. The box-and-dialog shape is the launcher's, for
 * the same reason the launcher uses it — the box is sized to the output being
 * looked at so the dialog lands on that monitor rather than in the middle of
 * a multi-monitor canvas.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* The scales worth offering as buttons.
 *
 * A fixed list rather than a free number: fractional scaling is a thing people
 * want at four or five values and a text field for it is a way to type 0.03 by
 * accident. Anything else is still reachable — `viewport msg -t
 * output.configure --name DP-1 --scale 1.1` — which is the right home for the
 * value nobody but you wants. */
const SETTINGS_SCALES = [1, 1.25, 1.5, 1.75, 2];

/* The rotations, spelled exactly as `Transform`'s wire names in
 * `crates/viewport-ipc/src/geometry.rs`. That comment says these strings are
 * load-bearing and that the shell's monitor settings compare against them
 * literally; this is the code it means. The four flipped ones are left out of
 * the panel — a mirrored monitor is a thing two people a year want and a thing
 * everybody else clicks by accident — and stay reachable over the socket. */
const SETTINGS_TRANSFORMS = [
  ['normal', 'Normal'],
  ['90', '90°'],
  ['180', '180°'],
  ['270', '270°'],
];

function toggleSettings() {
  if (settingsOpen) {
    closeSettings();
    return;
  }
  settingsOpen = true;
  /* The other dialogs go, the way a menu does: two dialogs over the same
     windows is two answers to one question, and this one is drawn where the
     launcher and the notification centre are drawn. */
  closeClipboard();
  closeLauncher();
  closeNotificationCentre();
  closeNetworkPicker();
  closeBluetoothPicker();
  closePowerPicker();
  /* Remembered before the keyboard is taken, because taking it is what loses
     it: the compositor answers shell.focus with a view.focused naming nothing,
     which sets focusedId to null on the way through. The wallpaper field is a
     real text box and the number fields are real number boxes, and none of
     them can be typed into while a window holds the keyboard. */
  settingsRestoreId = focusedId;
  send({ type: 'shell.focus' });
  /* The monitors as they are now, rather than as they were when the last
     window moved. `output.layout` carries the mode list, the scale and the
     rotation, and the shell keeps whatever arrived last — but a panel opened
     an hour into a session should not be describing an hour-old display. */
  send({ type: 'output.query' });
  renderSettings();
}

function closeSettings() {
  if (!settingsOpen) return;
  settingsOpen = false;
  /* An unanswered display change is left unanswered rather than confirmed:
     the compositor's deadline is still running and will put the monitors
     back, which is the safe half of the two — closing the panel is not
     somebody saying they can see the screen.
     The question goes with the panel all the same, because the panel cannot
     tell whether the deadline has since lapsed, and a Keep button that
     confirms nothing is worse than no button. Reopening inside the window and
     wanting to keep the change means setting it again, which is twelve
     seconds of inconvenience against a screen nobody can read. */
  settingsConfirming = false;
  settingsSaved = null;
  settingsEl.replaceChildren();
  settingsEl.hidden = true;
  setOverlay('settings', null);
  /* Give the keyboard back to whatever had it, for the reason the launcher's
     close does: a dialog that quietly kept the keyboard would leave the next
     keystroke going nowhere, and the window that was being worked in looks
     focused while receiving nothing. A window that closed in the meantime is
     not chased — the compositor refuses a view.focus for an id that is gone. */
  if (settingsRestoreId !== null && views.has(settingsRestoreId)) {
    send({ type: 'view.focus', id: settingsRestoreId });
  }
  settingsRestoreId = null;
}

/* A display change went out, so the panel has to ask whether it worked.
 *
 * Set here rather than inferred from the next `output.layout`, because the
 * layout that comes back from a change that *did* apply and the one that comes
 * back from a change that was refused look the same from here — the refusal is
 * an `error`, and by then the question is already the right one to ask. */
function settingsPending() {
  settingsConfirming = true;
  renderSettings();
}

/* The compositor said something changed. Redraw if anybody is looking.
 *
 * Called for both `config` and `output.layout`, because the panel is drawn
 * entirely from those two and neither of them is the panel's own doing: a
 * config file reloaded from an editor, a monitor unplugged, another client
 * changing the wallpaper — all of them have to reach the switches.
 *
 * The whole dialog is rebuilt rather than diffed, which is what every other
 * picker in this shell does and is the right trade at this size. The one cost
 * worth knowing: a message arriving while the caret is in the wallpaper field
 * replaces that field, so half a typed path is lost. These messages arrive
 * when something changes rather than on a timer, and the only thing that
 * changes while somebody is typing into this panel is something else on the
 * socket — a wallpaper cycler, an editor saving the config file — so the trade
 * is a lost half-line in a case that is rare against a diffing pass in a case
 * that is not. */
function settingsChanged() {
  if (settingsOpen) renderSettings();
}

/* The compositor wrote the settings down, and said where. */
function settingsWasSaved(path) {
  settingsSaved = typeof path === 'string' ? path : '';
  if (settingsOpen) renderSettings();
}

/* ------------------------------------------------------------------------
 * Drawing it
 * --------------------------------------------------------------------- */

function renderSettings() {
  settingsEl.replaceChildren();
  settingsEl.hidden = false;

  /* Over the output being looked at, rather than centred across all of them —
     see renderClipboard's own comment for why 50%/50% is the wrong centre on a
     multi-monitor desk. `#settings` is only the docking box; the dialog inside
     it is what the box's flex centring puts in the middle of that box. */
  const output = outputs.get(activeOutputName());
  if (output?.rect) {
    Object.assign(settingsEl.style, {
      left: `${output.rect.x}px`,
      top: `${output.rect.y}px`,
      width: `${output.rect.width}px`,
      height: `${output.rect.height}px`,
    });
  }

  const dialog = document.createElement('div');
  dialog.className = 'settings-dialog';
  /* A click inside the dialog must not reach the document listener that closes
     every picker: the fields are clicked for the caret and the buttons are
     clicked to act, and neither is a click that missed. */
  dialog.addEventListener('click', (e) => e.stopPropagation?.());

  dialog.append(settingsHeader());

  const body = document.createElement('div');
  body.className = 'settings-body';
  body.append(
    settingsAppearance(),
    settingsWallpaper(),
    settingsGaps(),
    settingsBorder(),
    settingsDisplays(),
  );
  dialog.append(body);

  if (settingsConfirming) dialog.append(settingsConfirmBar());
  dialog.append(settingsFooter());

  settingsEl.append(dialog);

  /* Tell the compositor where the dialog is, so it draws that piece of the
     shell above the windows — see setOverlay's own comment in state.js. The
     dialog alone, not the docking box that spans the whole output. */
  setOverlay('settings', dialog);
}

function settingsHeader() {
  const header = document.createElement('div');
  header.className = 'settings-header';

  const title = document.createElement('span');
  title.className = 'settings-title';
  title.textContent = 'Settings';
  header.append(title);

  const save = document.createElement('button');
  save.className = 'settings-save';
  save.textContent = 'Save';
  save.title = 'Write these settings down so they survive a restart';
  save.addEventListener('click', (e) => {
    e.stopPropagation?.();
    /* Cleared here rather than left showing the last save's path: pressing
       Save twice should look like two saves, and a message that never changes
       is one nobody can tell arrived. */
    settingsSaved = null;
    send({ type: 'config.save' });
    renderSettings();
  });
  header.append(save);

  return header;
}

/* The line along the bottom. Either what the last save wrote, or the standing
 * explanation of why there is a Save button at all — which is the one thing
 * about this panel that is not obvious from looking at it. */
function settingsFooter() {
  const footer = document.createElement('div');
  footer.className = 'settings-hint';
  footer.textContent = settingsSaved === null
    ? 'Changes apply now. Save keeps them across a restart.'
    : (settingsSaved === ''
      ? 'Saved.'
      : `Saved to ${settingsSaved}`);
  return footer;
}

function settingsConfirmBar() {
  const bar = document.createElement('div');
  bar.className = 'settings-confirm';

  const text = document.createElement('span');
  text.className = 'settings-confirm-text';
  /* No countdown drawn. The number of seconds lives in the compositor
     (`OUTPUT_REVERT_AFTER`) and a copy of it here would be a second place for
     it to be wrong; what matters to somebody reading this is not how long they
     have but that doing nothing is safe. */
  text.textContent = 'Keep this display setting? It goes back on its own '
    + 'if you do nothing.';
  bar.append(text);

  const keep = document.createElement('button');
  keep.className = 'settings-button';
  keep.textContent = 'Keep';
  keep.addEventListener('click', (e) => {
    e.stopPropagation?.();
    send({ type: 'output.confirm' });
    settingsConfirming = false;
    renderSettings();
  });
  bar.append(keep);

  const revert = document.createElement('button');
  revert.className = 'settings-button danger';
  revert.textContent = 'Revert';
  revert.addEventListener('click', (e) => {
    e.stopPropagation?.();
    send({ type: 'output.revert' });
    settingsConfirming = false;
    renderSettings();
  });
  bar.append(revert);

  return bar;
}

/* ------------------------------------------------------------------------
 * The pieces every section is built out of
 * --------------------------------------------------------------------- */

function settingsSection(name) {
  const section = document.createElement('div');
  section.className = 'settings-section';
  const heading = document.createElement('div');
  heading.className = 'settings-heading';
  heading.textContent = name;
  section.append(heading);
  return section;
}

/* One labelled row. The label is a plain element rather than a `<label>` with
 * a `for`: the shell builds these without ids, and a `<label>` pointing at
 * nothing is worse than no label at all for anything reading the page. */
function settingsRow(label, ...controls) {
  const row = document.createElement('div');
  row.className = 'settings-row';
  const name = document.createElement('span');
  name.className = 'settings-label';
  name.textContent = label;
  row.append(name);
  const value = document.createElement('span');
  value.className = 'settings-value';
  value.append(...controls);
  row.append(value);
  return row;
}

/* A switch that says what the setting *is* rather than what it would become —
 * the reading the network picker's own toggle uses, and the only one that
 * survives the state changing under it while somebody is looking. */
function settingsToggle(on, onChange) {
  const button = document.createElement('button');
  button.className = 'settings-toggle' + (on ? ' on' : '');
  button.textContent = on ? 'On' : 'Off';
  button.addEventListener('click', (e) => {
    e.stopPropagation?.();
    onChange(!on);
  });
  return button;
}

/* A row of choices, one of them current. Used for the wallpaper fitting, the
 * scales and the rotations: all three are a handful of named values where a
 * dropdown would hide the options behind a click. */
function settingsChoice(options, current, onChange) {
  const group = document.createElement('span');
  group.className = 'settings-choice';
  for (const [value, label] of options) {
    const button = document.createElement('button');
    button.className = 'settings-option'
      + (String(value) === String(current) ? ' active' : '');
    button.textContent = label;
    button.addEventListener('click', (e) => {
      e.stopPropagation?.();
      onChange(value);
    });
    group.append(button);
  }
  return group;
}

/* A whole number, committed on Enter or on losing the box.
 *
 * Not on every keystroke: typing `12` over `8` passes through `1`, and a gap
 * of one pixel applied for as long as it takes to type the second digit is a
 * desktop that jumps while being configured. Escape puts the box back to what
 * the compositor last said, which is the way out of a half-typed number. */
function settingsNumber(value, onChange) {
  const input = document.createElement('input');
  input.type = 'number';
  input.className = 'settings-number';
  input.min = '0';
  input.value = String(value);

  const commit = () => {
    const n = Number.parseInt(String(input.value ?? ''), 10);
    /* A negative one is refused by the compositor, which would come back as an
       error over a value the panel had already drawn. Refusing it here as well
       means the box simply does not accept it. */
    if (!Number.isFinite(n) || n < 0) {
      input.value = String(value);
      return;
    }
    if (n !== value) onChange(n);
  };

  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault?.();
      commit();
    } else if (e.key === 'Escape') {
      e.preventDefault?.();
      /* Kept out of the document's own Escape, which closes the panel: the
         first press puts a half-typed number back, and only a second one — in
         a box with nothing to put back — reaches the dialog. */
      e.stopPropagation?.();
      input.value = String(value);
    }
  });
  input.addEventListener('change', commit);
  input.addEventListener('blur', commit);
  return input;
}

/* ------------------------------------------------------------------------
 * The sections
 * --------------------------------------------------------------------- */

function settingsAppearance() {
  const section = settingsSection('Appearance');
  /* What this switch actually does, said on the row, because it is the one
     setting here that changes nothing the shell draws: the colour scheme goes
     out on the bus as `org.freedesktop.appearance`'s `color-scheme` and the
     GNOME theme name beside it, and what follows it is every toolkit
     application on the desk. The shell's own palette is dark, always. */
  section.append(settingsRow(
    'Dark applications',
    settingsToggle(shellConfig.dark_mode !== false, (on) => {
      send({ type: 'config.dark_mode', enabled: on });
    }),
  ));
  return section;
}

function settingsWallpaper() {
  const section = settingsSection('Wallpaper');

  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'settings-text';
  input.placeholder = '~/Pictures/wall.png, #1a1b26, or empty for none';
  /* What the compositor resolved, which for a picture is a `file://` URL
     rather than the path that was typed. Shown as it is rather than turned
     back into a path: it is what the desktop is actually loading, and a panel
     that pretty-printed it would be showing something the compositor never
     said. A CSS value — a colour, a gradient — comes back as written. */
  input.value = typeof shellConfig.wallpaper === 'string'
    ? shellConfig.wallpaper : '';

  const commit = () => {
    /* The empty string is a real value here and means "take it away", which is
       the only way back to the shell's own gradient. */
    send({ type: 'config.wallpaper', path: String(input.value ?? '').trim() });
  };
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault?.();
      commit();
    } else if (e.key === 'Escape') {
      e.preventDefault?.();
      /* The field's own Escape, not the dialog's — see settingsNumber. */
      e.stopPropagation?.();
      input.value = typeof shellConfig.wallpaper === 'string'
        ? shellConfig.wallpaper : '';
    }
  });
  section.append(settingsRow('Picture', input));

  const modes = WALLPAPER_MODES.map((mode) => [mode, mode]);
  section.append(settingsRow(
    'Fitting',
    settingsChoice(modes, shellConfig.wallpaper_mode ?? 'fill', (mode) => {
      send({ type: 'config.wallpaper', mode });
    }),
  ));
  return section;
}

function settingsGaps() {
  const section = settingsSection('Gaps');
  const gaps = shellConfig.gaps ?? {};
  /* The shell's own defaults, which is what an absent field means — the same
     numbers `applyGaps` falls back to. Drawing a blank box for "not set" would
     be a panel that cannot say what the desktop is currently doing. */
  section.append(settingsRow('Between windows', settingsNumber(
    gaps.inner ?? 8, (inner) => send({ type: 'config.gaps', inner }),
  )));
  section.append(settingsRow('Around the edge', settingsNumber(
    gaps.outer ?? 0, (outer) => send({ type: 'config.gaps', outer }),
  )));
  section.append(settingsRow('Drop for a lone window', settingsToggle(
    gaps.smart === true, (smart) => send({ type: 'config.gaps', smart }),
  )));
  return section;
}

function settingsBorder() {
  const section = settingsSection('Window border');
  const border = shellConfig.border ?? {};
  section.append(settingsRow('Corner radius', settingsNumber(
    border.radius ?? 6, (radius) => send({ type: 'config.border', radius }),
  )));
  section.append(settingsRow('Thickness', settingsNumber(
    border.width ?? 2, (width) => send({ type: 'config.border', width }),
  )));
  section.append(settingsRow('Square a lone window', settingsToggle(
    border.smart === true, (smart) => send({ type: 'config.border', smart }),
  )));
  return section;
}

/* How a mode is written on a button: the size, and the rate to one decimal.
 *
 * One decimal rather than none because 143.998 and 144 are two different
 * modes on plenty of panels and a list showing both as "144" is a list with
 * two identical rows in it. Not three decimals, which is the kernel's own
 * precision and is noise on a button. */
function settingsModeLabel(mode) {
  const hz = (mode.refresh ?? 0) / 1000;
  return `${mode.width}×${mode.height} @ ${hz.toFixed(1)} Hz`;
}

function settingsDisplays() {
  const section = settingsSection('Displays');

  if (outputs.size === 0) {
    const empty = document.createElement('div');
    empty.className = 'settings-empty';
    empty.textContent = 'No displays.';
    section.append(empty);
    return section;
  }

  for (const [name, output] of outputs) {
    const heading = document.createElement('div');
    heading.className = 'settings-display';
    /* The connector name is what everything else calls this monitor — the
       config file's `outputs` block, `viewport msg`, the log — so it is the
       name on the panel too, with the model beside it for the person who has
       two of the same connector and one of them is the television. */
    heading.textContent = output.info?.model
      ? `${name} — ${output.info.model}`
      : name;
    section.append(heading);

    section.append(settingsRow('Mode', settingsModeSelect(name, output)));

    section.append(settingsRow('Scale', settingsChoice(
      SETTINGS_SCALES.map((scale) => [scale, `${scale}×`]),
      output.info?.scale ?? 1,
      (scale) => {
        send({ type: 'output.configure', name, scale });
        settingsPending();
      },
    )));

    section.append(settingsRow('Rotation', settingsChoice(
      SETTINGS_TRANSFORMS,
      output.info?.transform ?? 'normal',
      (transform) => {
        send({ type: 'output.configure', name, transform });
        settingsPending();
      },
    )));
  }

  return section;
}

function settingsModeSelect(name, output) {
  const modes = Array.isArray(output.info?.modes) ? output.info.modes : [];
  if (modes.length === 0) {
    const none = document.createElement('span');
    none.className = 'settings-muted';
    /* A backend with no mode list is the nested and headless ones, where the
       size is whatever the window or the flag said. Saying so is better than
       an empty dropdown, which reads as a bug. */
    none.textContent = 'fixed by the backend';
    return none;
  }

  const select = document.createElement('select');
  select.className = 'settings-select';
  for (const mode of modes) {
    const option = document.createElement('option');
    /* The value carries the three numbers rather than an index into the list,
       because the list is rebuilt from every `output.layout` and an index into
       yesterday's list is a mode nobody asked for. */
    option.value = `${mode.width}x${mode.height}@${mode.refresh}`;
    option.textContent = settingsModeLabel(mode)
      + (mode.preferred ? ' (preferred)' : '');
    if (mode.current) option.selected = true;
    select.append(option);
  }
  select.value = currentModeValue(modes);
  select.addEventListener('change', (e) => {
    e.stopPropagation?.();
    const [size, refresh] = String(select.value ?? '').split('@');
    const [width, height] = size.split('x');
    send({
      type: 'output.configure',
      name,
      mode: {
        width: Number(width),
        height: Number(height),
        refresh: Number(refresh),
      },
    });
    settingsPending();
  });
  return select;
}

/* Which option is the one on screen now, as the value the `<option>`s carry.
 *
 * Empty when the compositor named no current mode, which leaves the browser on
 * the first option — the same thing a `<select>` with nothing selected does
 * anyway, and better than selecting a mode the display is not in. */
function currentModeValue(modes) {
  const current = modes.find((mode) => mode.current);
  return current
    ? `${current.width}x${current.height}@${current.refresh}`
    : '';
}
