/* SPDX-License-Identifier: MIT
 *
 * The on-screen keyboard.
 *
 * Every other picker in this shell draws information the compositor read
 * from somewhere else — a network, a device, a clipboard entry. This one
 * draws a keyboard, and what it sends is not a fact about the desktop but an
 * instruction: press this key, right now, on whatever client has the
 * keyboard. `osk.key` carries an X11/XKB keysym and whether it went down or
 * came back up — the compositor hands it straight to `inject_keysym`, the
 * same call a remote-desktop session's keystrokes go through, because both
 * are the same problem: something without a physical keyboard needs to press
 * one anyway. See the long comment on `Request::OskKey` in
 * `crates/viewport-ipc/src/request.rs` for why that path was chosen over
 * committing text through `zwp_input_method_v2`, which this compositor also
 * has wired up and does not use here.
 *
 * Two things are worth understanding before changing this file.
 *
 * **A tap holds the key down for exactly as long as the finger does, and
 * nothing here repeats a character.** `pressed: true` on touch/pointer-down,
 * `pressed: false` on up — the same two messages a hardware key sends — and
 * the seat's own keyboard repeat, already running at the same delay and rate
 * a physical key gets (`seat.add_keyboard(Default::default(), 200, 25)` in
 * `state.rs`), does the rest. A timer in this file that also repeated the
 * character would be a second repeat engine racing the first.
 *
 * **Every key is sent in its base, unshifted form; Shift is a real key.**
 * This compositor cannot press a keysym, only a key — see `inject_keysym`'s
 * own doc comment — so there is no way to ask it for a capital `A` directly
 * that is not, underneath, "press the key that types `a`, while Shift is
 * held." So that is exactly what this file does: the letter and punctuation
 * keys always carry their own base keysym (`a`, not `A`; `1`, not `!`), and
 * whenever the keyboard's own Shift state says the glyph drawn on a key is
 * the shifted one, the tap is wrapped in a real `Shift_L` press of its own —
 * mechanically the same thing a hand holding physical Shift while pressing
 * physical `1` does, which is why it produces `!` correctly on whatever
 * keymap the focused client's session is actually running, rather than this
 * file having to know that `!` lives on the `1` key in the first place.
 * Caps Lock is the same trick played once instead of every tap: tapping the
 * key sends a real `Caps_Lock` keysym down-and-up, which toggles the seat's
 * own lock modifier exactly as a hardware Caps Lock key would, and every
 * letter after that needs no Shift wrapping at all because the modifier is
 * already doing the work.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* The X11/XKB keysyms this file needs by name rather than by number.
 * Everything else it sends is a single ASCII character, whose keysym is
 * simply its own code point — X11 defined the printable Latin-1 keysyms to
 * equal Unicode there on purpose, which is what lets a row of key data below
 * be a plain string instead of a table of names. */
const OSK_KEYSYM = {
  BackSpace: 0xff08,
  Return: 0xff0d,
  Left: 0xff51,
  Up: 0xff52,
  Right: 0xff53,
  Down: 0xff54,
  Shift_L: 0xffe1,
  Caps_Lock: 0xffe5,
  space: 0x0020,
};

/* The three letter rows of a US QWERTY keyboard, lower case — the base form
 * every letter key is sent in, whatever the keyboard's own Shift state is
 * drawing on its face. */
const OSK_LETTER_ROWS = ['qwertyuiop', 'asdfghjkl', 'zxcvbnm'];

/* The symbol page's two rows, as (base, shifted) pairs at matching positions
 * — the same relationship the number row of a real keyboard has to the row
 * of symbols above it, because that is exactly what this is standing in
 * for. `base` is what gets sent, wrapped in a real Shift when the drawn glyph
 * is the `shifted` one; see the file banner for why sending `shifted`'s own
 * keysym directly would not work. */
const OSK_SYMBOL_ROWS = [
  { base: '1234567890', shifted: '!@#$%^&*()' },
  { base: "-=[]\\;',./", shifted: '_+{}|:"<>?' },
];

/* Whether the keyboard is on screen at all is two independent facts, not one
 * flag, because they arrive from different places and have to be able to
 * disagree without one silently overriding the other.
 *
 * `oskWanted` is the compositor's own opinion — the focused client just
 * enabled a text-input, or just disabled one — and arrives unasked, as
 * `osk.wanted`. `oskPinned` is somebody's own decision to have the keyboard
 * up, from Mod4+Shift+k or the keyboard's own hide button. Either one being
 * true is enough to show it; both have to be false to hide it, which is what
 * lets a manual open survive the field it was opened for losing focus, and
 * lets `osk.wanted` bring the keyboard up for a field nobody asked about by
 * hand.
 *
 * `oskDismissed` is what makes the hide button work while `oskWanted` is
 * still true: closing the keyboard by hand does not change anything the
 * compositor is tracking, so without this the very next check would see the
 * same `wanted: true` it always saw and the keyboard would not have moved.
 * Set on a manual close, cleared the moment `osk.wanted` reports a *new*
 * want — a field losing focus and a different one gaining it — because a
 * dismissal was about the field that was open when it happened, not about
 * every field that will ever ask again. */
let oskWanted = false;
let oskPinned = false;
let oskDismissed = false;

/* Whether the keyboard is actually on screen right now, tracked here rather
 * than read back off `oskEl.hidden` — the same choice every other picker in
 * this shell makes (`networkOpen`, `clipboardOpen`) and for the same reason:
 * a `hidden` attribute starts life however the markup or the harness wrote
 * it, and a state machine built on reading a DOM property back can only ever
 * be as right as whatever last wrote that property was, for reasons that have
 * nothing to do with this file. */
let oskVisible = false;

function oskShouldBeVisible() {
  return oskPinned || (oskWanted && !oskDismissed);
}

/* 'lower', 'upper' (one key, then back to 'lower' — the one-shot Shift a
 * phone keyboard gives you) or 'caps' (stays until tapped again, entered by
 * a double tap the way phone keyboards enter it too, since there is no
 * second key here to dedicate to it). Shared between the letters page and
 * the symbols page, the way a physical Shift key is: switching pages and
 * back leaves it where it was. */
let oskShiftState = 'lower';
/* 'letters' or 'symbols' — which page is drawn. */
let oskPage = 'letters';
/* When Shift was last tapped, purely to recognise a second tap as a double
 * tap rather than as two separate presses. */
let oskShiftTappedAt = 0;

/* Every key currently held down, as its own release function. A `Set` rather
 * than a single slot because a touchscreen is multi-touch and two fingers can
 * each be holding a different key at once; every entry is called and the set
 * emptied when the keyboard is hidden out from under them — see hideOsk. */
const oskActiveReleases = new Set();

function oskSendKey(keysym, pressed) {
  send({ type: 'osk.key', keysym, pressed });
}

/* What the keyboard's current Shift state means for one key: whether the
 * glyph drawn on it right now is the shifted one, and whether sending it
 * needs a real Shift_L wrapped around the tap. The two are not the same
 * question, and the gap between them is exactly Caps Lock: it draws capitals
 * (`drawn: true`) but needs no Shift press to get them (`press: false`),
 * because the real Caps Lock modifier the seat is already holding is doing
 * that on its own — wrapping the tap in a *second* Shift would be pressing
 * Shift while Caps Lock is on, which on a real keyboard types the lower-case
 * letter back, exactly the opposite of what the key is showing.
 *
 * `caseSensitive` is false for the symbols page: Caps Lock is a modifier for
 * cased letters and nothing else on a real keyboard, so a locked Caps Lock
 * changes nothing about which glyph a digit or a punctuation key shows —
 * only the one-shot Shift does, the same as it would on hardware. */
function oskShiftFor(caseSensitive) {
  if (oskShiftState === 'upper') return { drawn: true, press: true };
  if (oskShiftState === 'caps' && caseSensitive) return { drawn: true, press: false };
  return { drawn: false, press: false };
}

/* One tap consumed the one-shot Shift. Caps Lock is not this — it stays until
 * a second tap on the Shift key turns it off again, the same as the real
 * modifier it is standing in for. */
function oskConsumeOneShotShift() {
  if (oskShiftState !== 'upper') return;
  oskShiftState = 'lower';
  renderOsk();
}

/* A key that behaves like a physical one: held for exactly as long as the
 * pointer is, released the instant it lifts, and left to the seat's own
 * keyboard repeat for anything past that — see the file banner. Pointer
 * capture rather than `pointerleave`: a finger typing quickly slides a
 * little between going down and coming up, and capturing keeps this button
 * the one hearing about it rather than whatever the finger happened to end
 * up over.
 *
 * `shift` wraps the tap in a real Shift_L press, for a key whose base keysym
 * is not what the label on it says — see the file banner for why that is the
 * only way to type the shifted form at all. `consumesShift` is true for the
 * keys a one-shot Shift is *for* — letters and symbols — and false for
 * everything else, so tapping Backspace or Space in the middle of typing a
 * capital letter does not cancel the capital that was armed for it. */
function oskTypeKey(label, keysym, { shift = false, wide = false, consumesShift = false, className = '' } = {}) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = ['osk-key', wide ? 'osk-key-wide' : '', className].filter(Boolean).join(' ');
  button.textContent = label;

  let down = false;
  const release = () => {
    if (!down) return;
    down = false;
    oskActiveReleases.delete(release);
    button.classList.remove('osk-key-down');
    oskSendKey(keysym, false);
    if (shift) oskSendKey(OSK_KEYSYM.Shift_L, false);
    if (consumesShift) oskConsumeOneShotShift();
  };
  button.addEventListener('pointerdown', (e) => {
    e.preventDefault();
    if (down) return;
    down = true;
    button.setPointerCapture?.(e.pointerId);
    button.classList.add('osk-key-down');
    if (shift) oskSendKey(OSK_KEYSYM.Shift_L, true);
    oskSendKey(keysym, true);
    oskActiveReleases.add(release);
  });
  button.addEventListener('pointerup', release);
  button.addEventListener('pointercancel', release);
  button.addEventListener('lostpointercapture', release);
  return button;
}

/* A row element holding whatever keys are passed to it — the assembly step
 * every row in renderOsk goes through, so a row that mixes generated keys
 * (oskCharKeys below) with one-off ones (Shift, Backspace) is exactly as easy
 * to build as one that does not. */
function oskRow(keys, { className = '' } = {}) {
  const row = document.createElement('div');
  row.className = ['osk-row', className].filter(Boolean).join(' ');
  row.append(...keys);
  return row;
}

/* One row's worth of typing keys, from a string of base characters. Every
 * character is its own keysym — see the file banner — so this is nothing
 * more than one `oskTypeKey` per character, wrapped in Shift whenever
 * `oskShiftFor` says this row's keys need it right now. `shiftedLabels`, when
 * given, is the string to read the drawn glyph from instead of `chars`
 * itself — the symbol rows' shifted punctuation, which is not letters and so
 * cannot be had by upper-casing. `caseSensitive` is passed straight through
 * to `oskShiftFor`: true for the letters page, false for the symbols page —
 * see that function's own comment for why they answer differently. */
function oskCharKeys(chars, { shiftedLabels = null, consumesShift = true, caseSensitive = false } = {}) {
  const { drawn, press } = consumesShift
    ? oskShiftFor(caseSensitive)
    : { drawn: false, press: false };
  const keys = [];
  for (let i = 0; i < chars.length; i += 1) {
    const base = chars[i];
    const label = drawn
      ? (shiftedLabels ? shiftedLabels[i] : base.toUpperCase())
      : base;
    keys.push(oskTypeKey(label, base.codePointAt(0), {
      shift: press,
      consumesShift,
    }));
  }
  return keys;
}

/* The Shift/Caps Lock key. Not built with `oskTypeKey`: it does not type a
 * character itself, so there is no held keysym to send and no repeat to let
 * the seat take over — it is a plain toggle, on the same footing as the
 * radio pickers' own on/off switch, and like those this is answered with a
 * click rather than a pointer-capture press. */
function oskShiftKey() {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'osk-key osk-key-shift';
  if (oskShiftState !== 'lower') button.classList.add('osk-key-active');
  button.textContent = oskShiftState === 'caps' ? '⇪' : '⇧';
  button.addEventListener('click', (e) => {
    e.stopPropagation?.();
    const now = performance.now();
    const doubleTap = now - oskShiftTappedAt < 350;
    oskShiftTappedAt = now;

    if (oskShiftState === 'caps') {
      /* Leaving Caps Lock. The real modifier is what has been doing the
         casing, so turning it off is the whole of what this key has to do —
         the same pair of messages as entering it, just read backwards. */
      oskSendKey(OSK_KEYSYM.Caps_Lock, true);
      oskSendKey(OSK_KEYSYM.Caps_Lock, false);
      oskShiftState = 'lower';
    } else if (doubleTap) {
      oskSendKey(OSK_KEYSYM.Caps_Lock, true);
      oskSendKey(OSK_KEYSYM.Caps_Lock, false);
      oskShiftState = 'caps';
    } else {
      oskShiftState = oskShiftState === 'upper' ? 'lower' : 'upper';
    }
    renderOsk();
  });
  return button;
}

/* The ?123 / ABC key that switches between the letters page and the symbols
 * page. Also a plain toggle, for the same reason Shift is. */
function oskPageKey() {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'osk-key osk-key-page';
  button.textContent = oskPage === 'letters' ? '?123' : 'ABC';
  button.addEventListener('click', (e) => {
    e.stopPropagation?.();
    oskPage = oskPage === 'letters' ? 'symbols' : 'letters';
    renderOsk();
  });
  return button;
}

/* The keyboard's own hide button — the only way a touch-only desk has to
 * dismiss it, since there is no chord to press without a keyboard already up.
 * See toggleOsk for what "hide" means while `osk.wanted` is still true. */
function oskHideKey() {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'osk-key osk-key-hide';
  button.textContent = '⌄';
  button.addEventListener('click', (e) => {
    e.stopPropagation?.();
    toggleOsk();
  });
  return button;
}

function oskArrowKey(label, direction) {
  return oskTypeKey(label, OSK_KEYSYM[direction], { className: 'osk-key-arrow' });
}

/* Build the whole keyboard from `oskPage` and `oskShiftState`, the same way
 * every other picker in this shell rebuilds itself whole on every change
 * rather than patching a live DOM — see renderNetworkPicker and
 * renderScreencastPicker for the same choice made for the same reason: the
 * state is small, redrawing it is cheap, and a full rebuild cannot drift out
 * of step with what it is meant to show. */
function renderOsk() {
  const panel = document.createElement('div');
  panel.className = 'osk-panel';

  const backspace = oskTypeKey('⌫', OSK_KEYSYM.BackSpace, { wide: true, className: 'osk-key-backspace' });

  if (oskPage === 'letters') {
    const caseSensitive = { caseSensitive: true };
    panel.append(oskRow(oskCharKeys(OSK_LETTER_ROWS[0], caseSensitive)));
    panel.append(oskRow(oskCharKeys(OSK_LETTER_ROWS[1], caseSensitive)));
    panel.append(oskRow([oskShiftKey(), ...oskCharKeys(OSK_LETTER_ROWS[2], caseSensitive), backspace]));
  } else {
    panel.append(oskRow(oskCharKeys(OSK_SYMBOL_ROWS[0].base, { shiftedLabels: OSK_SYMBOL_ROWS[0].shifted })));
    panel.append(oskRow(oskCharKeys(OSK_SYMBOL_ROWS[1].base, { shiftedLabels: OSK_SYMBOL_ROWS[1].shifted })));
    panel.append(oskRow([oskShiftKey(), backspace]));
  }

  panel.append(oskRow([
    oskPageKey(),
    oskTypeKey(',', ','.codePointAt(0)),
    oskTypeKey(' ', OSK_KEYSYM.space, { wide: true, className: 'osk-key-space' }),
    oskTypeKey('.', '.'.codePointAt(0)),
    oskTypeKey('⏎', OSK_KEYSYM.Return, { wide: true, className: 'osk-key-enter' }),
  ]));

  panel.append(oskRow([
    oskHideKey(),
    oskArrowKey('←', 'Left'),
    oskArrowKey('↑', 'Up'),
    oskArrowKey('↓', 'Down'),
    oskArrowKey('→', 'Right'),
  ], { className: 'osk-row-nav' }));

  /* A click on the panel must not reach the document listener that closes
     the radio pickers and the tray menu — the keyboard is not one of them,
     and closing on a stray click through it would be a keyboard that
     vanishes while it is being used. */
  panel.addEventListener('click', (e) => e.stopPropagation?.());

  oskEl.replaceChildren(panel);

  /* Docked to the bottom of the output being typed on, the same way the
     screen-share dialog is centred over it — see reportScreencastRect —
     rather than to the bottom of the whole multi-monitor page, which on a
     two-monitor desk would put the keyboard under the seam between them
     more often than not. */
  const output = outputs.get(activeOutputName());
  if (output?.rect) {
    Object.assign(oskEl.style, {
      left: `${output.rect.x}px`,
      top: `${output.rect.y}px`,
      width: `${output.rect.width}px`,
      height: `${output.rect.height}px`,
    });
  }

  reportOskRect(panel);
}

/* Tell the compositor where the keyboard is, so it draws that piece of the
 * shell above the windows — see setOverlay's own comment in state.js. The
 * panel alone, not the docking box that spans the whole output: what the
 * compositor needs is the rectangle somebody can actually touch. */
function reportOskRect(panel) {
  setOverlay('osk', panel);
}

function showOsk() {
  oskVisible = true;
  oskEl.hidden = false;
  renderOsk();
}

/* Release anything still physically held before the buttons themselves are
 * removed from the document. Without this, a keyboard hidden mid-press — the
 * compositor saying `osk.wanted: false` while a finger is still on a key,
 * which a focus change elsewhere can cause at any moment — would leave that
 * key logically down on the focused client forever: its `pointerup` can
 * never fire on a button that is no longer in the document to receive it,
 * and nothing else ever sends the matching release. */
function oskReleaseHeld() {
  for (const release of [...oskActiveReleases]) release();
}

function hideOsk() {
  oskVisible = false;
  oskReleaseHeld();
  oskEl.hidden = true;
  oskEl.replaceChildren();
  setOverlay('osk', null);
}

function syncOskVisibility() {
  const should = oskShouldBeVisible();
  if (should === oskVisible) return;
  if (should) showOsk(); else hideOsk();
}

/* The compositor's own opinion of whether the focused client wants text
 * typed into it — see `osk.wanted`'s doc comment in event.rs for exactly
 * when this is sent. A *change* to `true` always gets to ask again, even if
 * this exact keyboard was dismissed a moment ago for a different field: see
 * `oskDismissed`'s own comment above for why. */
function applyOskWanted(wanted) {
  oskWanted = wanted;
  if (wanted) oskDismissed = false;
  syncOskVisibility();
}

/* Mod4+Shift+k, and the keyboard's own hide button. Opening always pins it
 * open regardless of what the compositor currently thinks the focused client
 * wants — a keyboard raised by hand is for typing into whatever is focused,
 * chord-bound or touch-tapped is the same request either way. Closing while
 * `osk.wanted` is still true has nothing else to change, so it marks the
 * want dismissed instead — see oskShouldBeVisible. */
function toggleOsk() {
  if (oskShouldBeVisible()) {
    oskPinned = false;
    oskDismissed = true;
  } else {
    oskPinned = true;
  }
  syncOskVisibility();
}
