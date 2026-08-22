/* SPDX-License-Identifier: MIT
 *
 * The two radios: the wireless network picker and the Bluetooth picker.
 *
 * NetworkManager and BlueZ are both on the system bus, which the page cannot
 * reach. The compositor reads them and sends `network.update` and
 * `bluetooth.update`; this file draws the two lists and sends the verbs back.
 * Nothing here knows what an access point object path is, or that BlueZ has
 * one — a row names a network by its name and a device by its address, which
 * is all the compositor will accept.
 *
 * Both pickers are in one file because they are one thing twice: the same
 * overlay, the same rows, the same "say when you are open so the radio is only
 * running while somebody is looking" contract, and the same handful of
 * helpers underneath. They are two elements rather than two tabs of one
 * because they answer different questions and are opened by different keys.
 *
 * Two things are worth knowing before changing either.
 *
 * **These are clicked, not steered.** The screen-share chooser is drawn here
 * and driven from the compositor, because it appears in answer to something a
 * client asked and has to take the keyboard away from whatever was focused.
 * These are the other kind — the clipboard picker's kind: they are opened
 * deliberately, the pointer over the shell already reaches the DOM, and the
 * compositor routes nothing on their behalf.
 *
 * **The passphrase box is real typed text.** The out-of-process shell is a
 * Wayland client, so keys reach it the way they reach any client — but only
 * once something gives it the keyboard, and on a desktop with a window open
 * that window has it. `shell.focus` is the request that moves it, so the box
 * asks for focus when it appears and hands it back to the window that had it
 * when it goes away. Without that the field can be clicked and not typed into,
 * which is exactly how it behaved before the request existed.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* Signal strength as four bars, because that is what a strength is for: the
 * difference between 71% and 68% is not a decision anybody makes, and the
 * difference between two bars and four is. Indexed by strength / 25, capped —
 * a strength of exactly 100 would fall off the end. */
const WIFI_BARS = ['󰤯', '󰤟', '󰤢', '󰤥', '󰤨'];

/* What to draw for a device, by the icon name BlueZ gave it. The property is a
 * freedesktop icon name and the page has no icon theme, so it picks a glyph
 * instead; anything unrecognised gets the Bluetooth mark itself, which is
 * always true of a Bluetooth device. */
const BLUETOOTH_GLYPHS = {
  'audio-card': '󰓃',
  'audio-headphones': '󰋋',
  'audio-headset': '󰋎',
  'camera-photo': '󰄀',
  'camera-video': '󰕧',
  computer: '󰟀',
  'input-gaming': '󰊴',
  'input-keyboard': '󰌌',
  'input-mouse': '󰍽',
  'input-tablet': '󰓶',
  phone: '󰄜',
  printer: '󰐪',
};

/* ------------------------------------------------------------------------
 * The wireless network picker
 * --------------------------------------------------------------------- */

function toggleNetworkPicker() {
  if (networkOpen) {
    closeNetworkPicker();
    return;
  }
  networkOpen = true;
  /* The keyboard, so the arrow keys reach the rows. The passphrase box used
     to be the only thing here that asked, which meant a list of networks
     could be read and not steered — see keys.js. */
  keyNavOpen('network');
  /* Asked for on open and stopped on close, which is the whole of why this is
     a message rather than something the compositor decides: a scan is the
     radio transmitting, and the only thing that knows whether anybody is
     looking at the list is the list. */
  send({ type: 'network.scan' });
  renderNetworkPicker();
}

function closeNetworkPicker() {
  if (!networkOpen) return;
  networkOpen = false;
  /* Before the element is emptied: the box may be up, and a picker that
     vanished around it would leave a field on screen with nothing behind
     it. */
  endPassphrase();
  keyNavClose('network');
  send({ type: 'network.scan', enabled: false });
  networkEl.replaceChildren();
  networkEl.hidden = true;
  setOverlay('network', null);
}

/* What the compositor last read off NetworkManager. Kept whether or not the
 * picker is open — the bar's network module reads it for its tooltip — but
 * only drawn while it is, because a snapshot arrives every time an access
 * point's strength moves and redrawing a hidden element would be a composited
 * frame of the whole desktop per twitch of a radio. */
function applyNetwork(snapshot) {
  networkState = snapshot && typeof snapshot === 'object' ? snapshot : null;
  renderBarsModules();
  /* Not while the passphrase box is up. A redraw rebuilds the picker from
     nothing — that is what makes a list that changes under a scan cheap to
     draw — and rebuilding the box would throw away what is being typed into
     it, halfway through a passphrase, on a message that arrives several times
     a second while the radio is scanning. Whatever the snapshot said is kept
     and drawn the moment the box closes, which is one keystroke away. */
  if (networkOpen && networkAsking === null) renderNetworkPicker();
}

function renderNetworkPicker() {
  networkEl.replaceChildren();
  networkEl.hidden = false;

  /* Over the output being looked at, rather than centred across all of them:
     the shell is one page spanning the whole layout, so a dialog centred in
     it lands in the middle of the desk — between two monitors, on the usual
     two-monitor desk. `#network-picker` is only the docking box; the dialog
     inside it is what `.radio-dialog`'s flex centring puts in the middle of
     that box, the same way osk.js docks the keyboard over the output being
     typed into — see renderOsk's own comment there. */
  const output = outputs.get(activeOutputName());
  if (output?.rect) {
    Object.assign(networkEl.style, {
      left: `${output.rect.x}px`,
      top: `${output.rect.y}px`,
      width: `${output.rect.width}px`,
      height: `${output.rect.height}px`,
    });
  }

  const dialog = document.createElement('div');
  dialog.className = 'radio-dialog';
  dialog.setAttribute('role', 'dialog');
  dialog.setAttribute('aria-modal', 'true');
  dialog.setAttribute('aria-label', 'Wi-Fi');

  const state = networkState;
  dialog.append(pickerHeader('Wi-Fi', state?.available && state?.wireless
    ? {
      label: state.enabled ? 'On' : 'Off',
      on: state.enabled === true,
      /* Absent `enabled` toggles, which is what this has to send: the
         snapshot it drew from may already be out of date, and a picker that
         computed the opposite of a stale value would turn the radio back on
         a moment after somebody turned it off. */
      click: () => send({ type: 'network.radio' }),
    }
    : null));

  const list = document.createElement('div');
  list.className = 'radio-list';

  /* Three reasons for a list with nothing in it, and they are not the same
     thing: nobody to ask, no radio, and a radio that is switched off. Saying
     which is the difference between a picker somebody can act on and one that
     looks broken. */
  if (!state?.available) {
    list.append(radioNote('NetworkManager is not running.'));
  } else if (!state.wireless) {
    list.append(radioNote(state.wired
      ? 'No wireless device. The wired connection is up.'
      : 'No wireless device.'));
  } else if (!state.enabled) {
    list.append(radioNote('Wi-Fi is switched off.'));
  } else if ((state.access_points ?? []).length === 0) {
    list.append(radioNote(state.scanning ? 'Looking…' : 'No networks in range.'));
  }

  for (const point of state?.access_points ?? []) {
    list.append(networkRow(point));
  }
  dialog.append(list);

  if (state?.error) dialog.append(radioError(state.error));

  networkEl.append(dialog);

  /* Arrows to choose, Enter to join or leave, Escape to go. Bound before the
     passphrase box is focused below, because binding puts the keyboard on the
     dialog and the box has to win that. */
  bindKeyNav('network', dialog, { dismiss: closeNetworkPicker });

  /* The passphrase box is focused here rather than where it is built, because
     `focus()` on an element that is not in the document does nothing and the
     picker is assembled bottom-up: the box is inside a row that is inside a
     list that is appended above. Guarded because the harness that runs this
     file without a browser has neither method. */
  if (networkAsking !== null) {
    networkEl.querySelector?.('.radio-input')?.focus?.();
  }

  /* The shell is one buffer under every window, so a picker it draws is behind
     them until the compositor is told where it is. The dialog alone rather
     than the whole screen: what is being chosen between is a network, and
     covering the windows to ask about one would be a strange way to ask. */
  setOverlay('network', dialog);
}

/* One network. The row says what it is and what would happen if it were
 * clicked; the passphrase box, when it is for this network, hangs underneath
 * it rather than in a dialog of its own, so that the thing being typed into
 * stays next to the name it is for. */
function networkRow(point) {
  const wrap = document.createElement('div');
  wrap.className = 'radio-item';

  const row = document.createElement('button');
  row.className = 'radio-row';
  if (point.active) row.classList.add('active');

  const icon = document.createElement('span');
  icon.className = 'radio-icon';
  icon.textContent = WIFI_BARS[Math.min(4, Math.floor((point.strength ?? 0) / 25))];
  row.append(icon);

  const label = document.createElement('span');
  label.className = 'radio-label';
  label.textContent = point.ssid;
  row.append(label);

  const detail = document.createElement('span');
  detail.className = 'radio-detail';
  /* One word, and the most useful one available. What somebody reads off a
     row is whether they are on it, whether it will just work, and whether it
     is the open network they should not be putting a password into. */
  detail.textContent = point.active ? 'connected'
    : point.known ? 'saved'
      : point.security === 'enterprise' ? 'enterprise'
        : point.security ? '' : 'open';
  row.append(detail);

  if (point.security) {
    const lock = document.createElement('span');
    lock.className = 'radio-lock';
    lock.textContent = '󰌾';
    row.append(lock);
  }

  /* Named rather than left to the row's own text. Strength is four bars in a
     private-use codepoint and the lock is another, so a reader given the text
     would say two nonsense syllables around the part that matters. */
  keyNavRowEl(row, [point.ssid,
    point.active ? 'connected' : point.known ? 'saved' : null,
    point.security ? 'secured' : 'open',
    `signal ${Math.min(4, Math.floor((point.strength ?? 0) / 25))} of 4`,
  ].filter(Boolean).join(', '));

  row.addEventListener('click', (e) => {
    e.stopPropagation?.();
    if (point.active) {
      send({ type: 'network.disconnect' });
      return;
    }
    /* A network with a saved connection, and an open one, are joined outright:
       there is nothing to ask. Everything else needs a passphrase — except an
       enterprise network, which needs a certificate and an identity that a
       one-line box cannot express, so the picker says so rather than offering
       a field that cannot work. */
    if (point.known || !point.security) {
      send({ type: 'network.connect', ssid: point.ssid });
      return;
    }
    if (point.security === 'enterprise') {
      networkAsking = null;
      networkState = { ...(networkState ?? {}), error: 'That network needs an enterprise profile; set it up with nmcli.' };
      renderNetworkPicker();
      return;
    }
    beginPassphrase(point.ssid);
  });
  wrap.append(row);

  if (networkAsking === point.ssid) wrap.append(passphraseBox(point.ssid));
  return wrap;
}

/* The passphrase field, and the two ways out of it.
 *
 * A real `<input type="password">` rather than anything the compositor
 * assembles from keysyms: the shell is a browser engine and this is what a
 * browser engine is for — an insertion point, a selection, a paste, a
 * composed character from an input method. What the compositor has to do is
 * make sure the keys arrive, which is `shell.focus` and nothing more. */
function passphraseBox(ssid) {
  const box = document.createElement('div');
  box.className = 'radio-passphrase';

  const input = document.createElement('input');
  input.type = 'password';
  input.className = 'radio-input';
  input.placeholder = `Passphrase for ${ssid}`;

  const submit = () => {
    const passphrase = String(input.value ?? '');
    if (passphrase.length === 0) return;
    send({ type: 'network.connect', ssid, passphrase });
    /* The picker stays up. Joining a network takes several seconds and fails
       often enough to matter — a mistyped passphrase is the usual reason —
       and a picker that closed on submit would take the error message with
       it. The next snapshot either shows the row connected or says why not. */
    endPassphrase();
    renderNetworkPicker();
  };

  input.addEventListener('keydown', (e) => {
    /* Every press, not only the two handled below: the picker's own arrow
       navigation is bound on the dialog this box is inside, and a Down
       pressed while typing a passphrase must move the caret rather than the
       highlight three rows away. */
    e.stopPropagation?.();
    if (e.key === 'Enter') {
      e.preventDefault?.();
      submit();
    } else if (e.key === 'Escape') {
      e.preventDefault?.();
      endPassphrase();
      renderNetworkPicker();
    }
  });
  /* A click inside the box must not reach the document listener that closes
     pickers, and must not re-trigger the row above it. */
  box.addEventListener('click', (e) => e.stopPropagation?.());
  box.append(input);

  const join = document.createElement('button');
  join.className = 'radio-join';
  join.textContent = 'Join';
  join.addEventListener('click', (e) => {
    e.stopPropagation?.();
    submit();
  });
  box.append(join);
  return box;
}

/* Open the box for one network.
 *
 * No `shell.focus` of its own any more: the picker took the keyboard when it
 * opened, because the list of networks has to be steerable and not only the
 * field under one row of it. This used to be the only place that asked, which
 * is exactly why the rows could be read and not chosen between. */
function beginPassphrase(ssid) {
  networkAsking = ssid;
  renderNetworkPicker();
}

/* Close the box.
 *
 * The keyboard stays on the picker rather than going back to the window: the
 * picker is still up, and abandoning a passphrase should leave the arrow keys
 * able to pick a different network rather than dropping the whole surface out
 * of reach of the keyboard that was steering it. It goes back when the picker
 * closes, which is keyNavClose's job — see closeNetworkPicker. */
function endPassphrase() {
  if (networkAsking === null) return;
  networkAsking = null;
}

/* ------------------------------------------------------------------------
 * The Bluetooth picker
 * --------------------------------------------------------------------- */

function toggleBluetoothPicker() {
  if (bluetoothOpen) {
    closeBluetoothPicker();
    return;
  }
  bluetoothOpen = true;
  keyNavOpen('bluetooth');
  /* Discovery starts with the picker and stops with it. Nothing else in this
     shell turns a radio on, and it is why the close below is not optional. */
  send({ type: 'bluetooth.scan' });
  renderBluetoothPicker();
}

function closeBluetoothPicker() {
  if (!bluetoothOpen) return;
  bluetoothOpen = false;
  keyNavClose('bluetooth');
  send({ type: 'bluetooth.scan', enabled: false });
  bluetoothEl.replaceChildren();
  bluetoothEl.hidden = true;
  setOverlay('bluetooth', null);
}

function applyBluetooth(snapshot) {
  bluetoothState = snapshot && typeof snapshot === 'object' ? snapshot : null;
  if (bluetoothOpen) renderBluetoothPicker();
}

function renderBluetoothPicker() {
  bluetoothEl.replaceChildren();
  bluetoothEl.hidden = false;

  /* Over the output being looked at — see renderNetworkPicker's own comment
     for why 50%/50% is the wrong centre on a multi-monitor desk. */
  const output = outputs.get(activeOutputName());
  if (output?.rect) {
    Object.assign(bluetoothEl.style, {
      left: `${output.rect.x}px`,
      top: `${output.rect.y}px`,
      width: `${output.rect.width}px`,
      height: `${output.rect.height}px`,
    });
  }

  const dialog = document.createElement('div');
  dialog.className = 'radio-dialog';
  dialog.setAttribute('role', 'dialog');
  dialog.setAttribute('aria-modal', 'true');
  dialog.setAttribute('aria-label', 'Bluetooth');

  const state = bluetoothState;
  dialog.append(pickerHeader('Bluetooth', state?.available
    ? {
      label: state.powered ? 'On' : 'Off',
      on: state.powered === true,
      click: () => send({ type: 'bluetooth.power' }),
    }
    : null));

  const list = document.createElement('div');
  list.className = 'radio-list';

  if (!state?.available) {
    list.append(radioNote('No Bluetooth adapter.'));
  } else if (!state.powered) {
    list.append(radioNote('Bluetooth is switched off.'));
  } else if ((state.devices ?? []).length === 0) {
    list.append(radioNote(state.discovering ? 'Looking…' : 'Nothing found yet.'));
  }

  for (const device of state?.devices ?? []) {
    list.append(bluetoothRow(device));
  }
  dialog.append(list);

  if (state?.error) dialog.append(radioError(state.error));

  bluetoothEl.append(dialog);

  bindKeyNav('bluetooth', dialog, { dismiss: closeBluetoothPicker });

  /* Named to the compositor for the same reason the network picker is: the
     shell is one buffer under the windows, and a list nobody can see is not a
     picker. The dialog alone, not the docking box that spans the whole
     output. */
  setOverlay('bluetooth', dialog);
}

/* One device. Tapping the row does the obvious thing — connect what is not
 * connected, disconnect what is — and the ✕ forgets a device that is paired,
 * which is the only way back out of a pairing that went wrong. */
function bluetoothRow(device) {
  const row = document.createElement('button');
  row.className = 'radio-row';
  if (device.connected) row.classList.add('active');

  const icon = document.createElement('span');
  icon.className = 'radio-icon';
  icon.textContent = BLUETOOTH_GLYPHS[device.icon] ?? '󰂯';
  row.append(icon);

  const label = document.createElement('span');
  label.className = 'radio-label';
  /* A device that has not said what it is called is still a device, and a row
     with no text in it looks like a bug rather than a choice. The address is
     what the request names it by anyway. */
  label.textContent = device.name || device.address;
  row.append(label);

  const detail = document.createElement('span');
  detail.className = 'radio-detail';
  detail.textContent = device.connected ? 'connected'
    : device.paired ? 'paired'
      /* dBm, negative, closer to zero being nearer. Shown only for a device
         the adapter can hear right now, which is what tells the headset on
         the desk from the one in a drawer three rooms away. */
      : typeof device.rssi === 'number' ? `${device.rssi} dBm` : '';
  row.append(detail);

  let forget = null;
  if (device.paired) {
    forget = document.createElement('span');
    forget.className = 'radio-forget';
    forget.textContent = '✕';
    row.append(forget);
  }

  keyNavRowEl(row, [device.name || device.address,
    device.connected ? 'connected' : device.paired ? 'paired' : null,
  ].filter(Boolean).join(', '));

  row.addEventListener('click', (e) => {
    e.stopPropagation?.();
    /* One listener, because the target says which part was hit — the same
       shape the clipboard picker's rows use, and for the same reason: a second
       element with its own listener is a second thing to keep in step with the
       row it belongs to. */
    const action = e.target === forget && forget !== null ? 'forget'
      : device.connected ? 'disconnect'
        : 'connect';
    send({ type: 'bluetooth.device', address: device.address, action });
  });
  return row;
}

/* ------------------------------------------------------------------------
 * The pieces both pickers are built from
 * --------------------------------------------------------------------- */

/* A title, and the radio's own on/off switch beside it.
 *
 * The switch is absent rather than disabled where there is no radio to switch:
 * a control that cannot do anything is a question about why, and the note in
 * the list below already answers it. */
function pickerHeader(title, toggle) {
  const header = document.createElement('div');
  header.className = 'radio-header';

  const name = document.createElement('span');
  name.className = 'radio-title';
  name.textContent = title;
  header.append(name);

  if (toggle) {
    const button = document.createElement('button');
    button.className = 'radio-toggle';
    if (toggle.on) button.classList.add('on');
    button.textContent = toggle.label;
    /* The switch is a stop of its own. It is the first thing in the dialog and
       the only control on a picker whose radio is off — a keyboard that could
       reach the rows and not the switch could not turn the radio back on. */
    keyNavRowEl(button, `${title}, ${toggle.on ? 'on' : 'off'}`);
    button.setAttribute('role', 'switch');
    button.setAttribute('aria-checked', toggle.on ? 'true' : 'false');
    button.addEventListener('click', (e) => {
      e.stopPropagation?.();
      toggle.click();
    });
    header.append(button);
  }
  return header;
}

/* Why there is nothing to choose from. */
function radioNote(text) {
  const note = document.createElement('div');
  note.className = 'radio-empty';
  note.textContent = text;
  return note;
}

/* What went wrong, in the daemon's own words.
 *
 * On the picker rather than in the console, because it is an answer to
 * something somebody just did: a refused passphrase and a pairing that timed
 * out are both ordinary, and neither is a protocol error worth an `error`
 * event. */
function radioError(text) {
  const line = document.createElement('div');
  line.className = 'radio-error';
  line.textContent = text;
  return line;
}
