/* SPDX-License-Identifier: MIT
 *
 * The lock screen: the clock, the password box, and the way back in.
 *
 * Every other modal surface here is a convenience — the launcher, the power
 * menu, the notification centre. This one is the only thing standing between
 * somebody walking past the desk and everything on it, and that changes two
 * rules about how it is written.
 *
 * The first is that it must cover *everything*, opaquely, across every
 * monitor. While the session is locked the compositor draws this page's buffer
 * and nothing else — no windows, no layer surfaces, no wallpaper — so whatever
 * `#lock` does not cover is the desktop underneath showing through: the bar,
 * the taskbar with every window's title in it, whatever the notification
 * centre last had. `#lock` is `position: fixed`, spans the whole layout, and
 * its background is a solid colour with nothing translucent anywhere in it.
 * See shell.css, which says the same thing next to the rule.
 *
 * The second is that the compositor does not trust it, and this file is
 * written to be worth the little trust it does get. Nothing of this page
 * reaches a locked screen until `session.lock.drawn` has been sent *and* a
 * frame has been painted after it — see `lock_screen_is_drawing` in
 * `crates/viewport/src/handlers/session_lock.rs` for why both halves are
 * needed. That is what `lockAnnounceDrawn` below is for, and why it waits two
 * animation frames rather than sending from the handler: the second frame
 * callback runs after the frame the lock screen was rendered into has been
 * submitted, so every buffer the compositor sees after the message has this
 * screen in it and none of them has the desktop.
 *
 * Nothing here decides whether the session is locked, whether a password was
 * right, or when it is over. All three are the compositor's, because a page
 * that could decide any of them would be a lock screen that a page bug could
 * open.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* Which lock is on screen, or null when the session is not locked. It comes
   from the compositor and goes back out on every message, so a page that
   restarted mid-lock cannot answer for the lock before it. */
let lockGeneration = null;
/* Whether the compositor can check a password at all. False is a machine
   whose libpam would not load: the box is still drawn, because a lock screen
   that vanishes when it cannot authenticate is not a lock screen, but it says
   what has happened rather than swallowing every attempt in silence. */
let lockCanAuthenticate = true;
/* What is in the box. Kept here rather than read back off the input because
   the input is rebuilt on every render — a monitor coming or going redraws
   the whole screen — and a half-typed password must survive that. */
let lockPassword = '';
/* The last refusal, in the compositor's words (which are PAM's). */
let lockError = '';
/* An attempt is with PAM. The box is disabled while it is: the answer takes
   as long as the stack takes, and a second Enter is dropped by the compositor
   anyway, so saying so is better than looking broken. */
let lockBusy = false;
/* The password input on the output being looked at, for focus and for the
   clock tick to leave alone. */
let lockInputEl = null;
/* The clock elements, one per output, so the tick can write them without
   rebuilding the screen once a second. */
let lockClockEls = [];

/* Whether the lock screen is up. Read by bar.js's tick and by commands.js. */
function lockIsUp() {
  return lockGeneration !== null;
}

/* The compositor says the session is locked and this page draws the screen. */
function applySessionLock(generation, canAuthenticate) {
  const fresh = lockGeneration !== generation;
  lockGeneration = generation;
  lockCanAuthenticate = canAuthenticate !== false;
  if (fresh) {
    /* A new lock starts empty. Carrying a half-typed password across one
       would mean a password typed at a screen somebody else was looking at. */
    lockPassword = '';
    lockError = '';
    lockBusy = false;
  }
  /* The keyboard has to be able to come up *over* this, and it normally sits
     below the pickers for reasons that stop applying the moment there are no
     pickers. See the `:root.locked` rule in shell.css. */
  document.documentElement?.classList?.add('locked');
  renderLockScreen();
  lockAnnounceDrawn(generation);
}

/* The compositor says it is over. */
function applySessionUnlock() {
  lockGeneration = null;
  lockPassword = '';
  lockError = '';
  lockBusy = false;
  lockInputEl = null;
  lockClockEls = [];
  lockEl.replaceChildren();
  lockEl.hidden = true;
  document.documentElement?.classList?.remove('locked');
  /* The keyboard came up for the password box and has no reason to stay. A
     desk with a hardware keyboard never saw it; a desk without one is looking
     at half its screen taken by a keyboard over a desktop it can now use. */
  if (typeof oskPinned !== 'undefined' && oskPinned) {
    oskPinned = false;
    syncOskVisibility();
  }
}

/* PAM said no. */
function applySessionLockError(generation, message) {
  if (generation !== lockGeneration) return;
  lockBusy = false;
  lockError = typeof message === 'string' && message ? message : 'Wrong password.';
  /* Cleared, not kept: retyping over a wrong password one character at a time
     is how a second wrong password happens. */
  lockPassword = '';
  renderLockScreen();
}

/* Tell the compositor the lock screen has been painted.
 *
 * Two frames, not one, and not a `setTimeout`. The first callback runs before
 * the frame this render belongs to has been composited; the second runs after
 * it has been submitted, which is the earliest moment at which "there is a
 * buffer with the lock screen in it" is true. The compositor requires a frame
 * to land after this message before it will draw any of this page — so sending
 * it early does not open anything, it only means the message is ignored until
 * the *next* frame, and sending it late is a black screen for as long as it
 * takes. Two frames is the smallest wait that is honest.
 *
 * Guarded on the generation because the answer travels: a lock that ended
 * while these two frames were pending must not be answered for. */
function lockAnnounceDrawn(generation) {
  const announce = () => {
    if (lockGeneration !== generation) return;
    send({ type: 'session.lock.drawn', generation });
  };
  if (typeof requestAnimationFrame === 'function') {
    requestAnimationFrame(() => requestAnimationFrame(announce));
  } else {
    /* No animation clock — the test harness, and any engine that does not
       give the page one. The message is still sent, because the alternative
       is a lock screen that never appears at all. */
    announce();
  }
}

/* Try the password that is in the box. */
function lockSubmit() {
  if (!lockIsUp() || lockBusy) return;
  if (!lockPassword) return;
  lockBusy = true;
  lockError = '';
  send({ type: 'session.unlock', generation: lockGeneration, password: lockPassword });
  renderLockScreen();
}

/* The clock, in the two lines a lock screen shows it in: the time large, the
 * date under it. Deliberately not bar.js's `clockText`, which is one line with
 * a glyph in front of it for a 12-pixel-high bar module. */
function lockTimeText() {
  const now = new Date();
  return `${String(now.getHours()).padStart(2, '0')}:${String(now.getMinutes()).padStart(2, '0')}`;
}

function lockDateText() {
  return new Date().toLocaleDateString('en-US', {
    weekday: 'long',
    month: 'long',
    day: 'numeric',
  });
}

/* The one-second tick, called from bar.js's own so an idle machine has one
 * timer rather than two — see the note on `renderClocks` there, and the rule
 * in shell.md that nothing here repeats for ever. Writes text and nothing
 * else: rebuilding the screen once a second would take the focus off the
 * password box every second, which is unusable. */
function renderLockClocks() {
  if (!lockIsUp()) return;
  const time = lockTimeText();
  const date = lockDateText();
  for (const { timeEl, dateEl } of lockClockEls) {
    if (timeEl && timeEl.textContent !== time) timeEl.textContent = time;
    if (dateEl && dateEl.textContent !== date) dateEl.textContent = date;
  }
}

/* Build the whole thing: one pane per monitor, the password box on the one
 * being looked at.
 *
 * A pane on every output rather than one dialog on the active one, which is
 * what every other picker here does. Two reasons, and they are both about what
 * a locked screen is: a monitor showing nothing at all reads as a monitor that
 * has died, and — more to the point — the compositor draws this page's buffer
 * across the whole layout while locked, so a pane that is not there is not a
 * dark screen, it is the desktop. `#lock` covers that on its own, but the pane
 * is what makes each screen say what has happened. */
function renderLockScreen() {
  if (!lockIsUp()) return;
  lockEl.replaceChildren();
  lockEl.hidden = false;
  lockInputEl = null;
  lockClockEls = [];

  const active = activeOutputName();
  /* An output list that has not arrived yet — a page that started while the
     session was already locked — still gets one pane, over the whole layout.
     A lock screen with no clock is worth more than no lock screen. */
  const panes = outputs.size > 0 ? [...outputs.entries()] : [[null, null]];

  for (const [name, output] of panes) {
    const pane = document.createElement('div');
    pane.className = 'lock-pane';
    if (output?.rect) {
      Object.assign(pane.style, {
        left: `${output.rect.x}px`,
        top: `${output.rect.y}px`,
        width: `${output.rect.width}px`,
        height: `${output.rect.height}px`,
      });
    }

    const timeEl = document.createElement('div');
    timeEl.className = 'lock-time';
    timeEl.textContent = lockTimeText();
    const dateEl = document.createElement('div');
    dateEl.className = 'lock-date';
    dateEl.textContent = lockDateText();
    pane.append(timeEl, dateEl);
    lockClockEls.push({ timeEl, dateEl });

    /* The box goes on one screen. Three password fields showing three
       different halves of the same password is worse than one that has to be
       looked at. */
    if (name === active || name === null) {
      pane.append(lockForm());
    }
    lockEl.append(pane);
  }
}

/* The password box and everything that goes with it. */
function lockForm() {
  const form = document.createElement('div');
  form.className = 'lock-form';

  const input = document.createElement('input');
  input.type = 'password';
  input.className = 'lock-input';
  input.autocomplete = 'off';
  input.spellcheck = false;
  input.placeholder = lockBusy ? 'Checking…' : 'Password';
  input.disabled = lockBusy;
  input.value = lockPassword;
  input.addEventListener('input', () => {
    lockPassword = input.value ?? '';
  });
  input.addEventListener('keydown', (e) => {
    e.stopPropagation?.();
    if (e.key === 'Enter') {
      /* Read off the element as well as off the `input` event, because a key
         injected by the on-screen keyboard arrives as a real key press on the
         seat and the two paths have no reason to agree about ordering. */
      lockPassword = input.value ?? lockPassword;
      lockSubmit();
    }
  });
  form.append(input);
  lockInputEl = input;

  const unlock = document.createElement('button');
  unlock.type = 'button';
  unlock.className = 'lock-unlock';
  unlock.textContent = 'Unlock';
  unlock.disabled = lockBusy;
  unlock.addEventListener('click', (e) => {
    e.stopPropagation?.();
    lockSubmit();
  });
  form.append(unlock);

  /* The whole reason this screen is drawn here rather than by swaylock: a desk
     with no keyboard can raise one. Offered whenever the on-screen keyboard is
     not switched off outright — including in `manual` mode, where it does not
     raise itself and this is the only thing that would. */
  if (typeof oskMode === 'undefined' || oskMode !== 'off') {
    const keyboard = document.createElement('button');
    keyboard.type = 'button';
    keyboard.className = 'lock-keyboard';
    keyboard.textContent = '⌨';
    keyboard.title = 'On-screen keyboard';
    keyboard.addEventListener('click', (e) => {
      e.stopPropagation?.();
      oskPinned = !oskPinned;
      syncOskVisibility();
      /* Straight back to the box: the keyboard types into whatever the page
         has focused, and that has to be the password field and not the button
         that just raised it. */
      lockInputEl?.focus?.();
    });
    form.append(keyboard);
  }

  const message = document.createElement('div');
  message.className = 'lock-message';
  if (!lockCanAuthenticate) {
    message.classList.add('lock-message-error');
    message.textContent =
      'This session cannot check a password. Switch to another VT to get back in.';
  } else if (lockError) {
    message.classList.add('lock-message-error');
    message.textContent = lockError;
  }
  form.append(message);

  /* Focused on every render, because the keyboard is on this surface and the
     page decides where in it the keys land. Without it a lock screen that was
     redrawn — a wrong password, a monitor plugged in — silently stops
     accepting typing. */
  input.focus?.();
  return form;
}
