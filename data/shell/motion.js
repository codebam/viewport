/* SPDX-License-Identifier: MIT
 *
 * Motion: the animations that cannot be written as CSS, in one place.
 *
 * Most of this desktop's movement is a stylesheet — a window sliding between
 * two layouts is `transition: flex-grow`, and it should stay that way. What is
 * gathered here is the remainder, the cases the cascade genuinely cannot
 * express:
 *
 *   - an element leaving. CSS can animate an entrance because the element is
 *     there to be styled; it cannot animate a notification being dismissed,
 *     because `remove()` takes the element away before any frame of the
 *     animation is drawn.
 *   - a stagger. The bar's modules and the chooser's sources arrive together
 *     and should not; per-child delays in CSS mean per-child rules, written
 *     against a child count the markup is free to change.
 *   - a value that is not a style at all. A window's contents are a Wayland
 *     surface the compositor draws, so its opacity is an IPC message and no
 *     rule here can reach it. See surfaceOpacityTween.
 *
 * GSAP drives them, vendored at data/shell/vendor/gsap.min.js and loaded
 * before every other script. It is used as a tween engine only: no
 * ScrollTrigger, no Flip, no plugins. The FLIP in geometry.js stays hand
 * written because it is a measurement pass the compositor has to be told
 * about, not an effect.
 *
 * Two rules hold everywhere in this file, and breaking either is a compositor
 * bug rather than an ugly animation:
 *
 * **Nothing measured may be transformed.** A window frame, a `.viewport` hole,
 * an overview thumbnail, the strip, and any element handed to setOverlay are
 * read back with getBoundingClientRect and sent to the compositor. A transform
 * on one of those — or on any of their ancestors — is not a visual effect, it
 * is a stream of new geometry for every client on the screen, for as long as
 * the animation runs. Where an entrance is wanted on something measured, it
 * fades: opacity moves no rectangle. The same reasoning is written out at
 * `overview-in` and `.screencast-dialog` in shell.css.
 *
 * **Nothing repeats forever.** Every frame the shell paints is a composited
 * frame for the whole desktop, so an idling loop — a pulsing dot, a drifting
 * logo — is a permanent cost paid by a machine that is doing nothing. Every
 * tween here is finite and started by something that happened.
 *
 * One of the ordered scripts that make up the shell; see index.html for the
 * load order and shell.md for what the whole is meant to do.
 */

/* Whether the engine actually loaded. A shell whose vendor directory did not
   get installed should be a desktop without animations, not a desktop that
   throws on the first notification, so every helper below falls through to the
   end state when this is false. */
const motionReady = typeof gsap === 'object' && gsap !== null;

if (motionReady) {
  gsap.config({
    /* How long the engine keeps its frame callback scheduled after the last
       tween ends, in frames. The stock 120 is two seconds of a page asking to
       be woken up per frame with nothing to draw — cheap in a browser tab,
       not cheap here, where this page is the desktop and the machine it is on
       may be doing nothing at all. Twenty is a third of a second, which costs
       nothing to keep and is long enough that a burst of notifications does
       not restart the ticker between each one. It wakes itself on the next
       tween either way. */
    autoSleep: 20,
    /* No translateZ. GSAP's default promotes an animating element to its own
       layer for the duration, which is a reasonable trade in a document and a
       poor one in a page whose every frame is composited into a desktop
       alongside real client surfaces. Nothing animated here needs it: these
       are short fades over a few elements. */
    force3D: false,
    /* A target that has gone is not news. The shell animates things that are
       being taken off the screen, and a notification dismissed twice in a
       frame is ordinary. */
    nullTargetWarn: false,
  });
}

/* Durations and the curve come from shell.css.
 *
 * --anim and --anim-slow are what every transition in the stylesheet is
 * written against, and a tween running beside one of them at a different speed
 * does not read as a second animation — it reads as the compositor lagging. So
 * the numbers are read out of the cascade rather than restated here, and there
 * is one place to change the pace of the desktop.
 *
 * Read once, on first use, and only ever while motion is enabled: the
 * reduced-motion query zeroes both properties, and a value captured under it
 * would outlive the setting. Every caller checks reducedMotion() first, so the
 * cache can only be filled with the real numbers. */
const motionTimings = new Map();

function motionSeconds(property) {
  if (motionTimings.has(property)) return motionTimings.get(property);

  let seconds = 0.18;
  if (typeof getComputedStyle === 'function') {
    const raw = getComputedStyle(document.documentElement)
      .getPropertyValue(property).trim();
    const value = parseFloat(raw);
    if (Number.isFinite(value) && value > 0) {
      seconds = raw.endsWith('ms') ? value / 1000 : value;
    }
  }
  motionTimings.set(property, seconds);
  return seconds;
}

/* The stylesheet's cubic-bezier, spelled the way GSAP wants it. Written out
   rather than parsed from --ease: the property is a CSS function call, and a
   parser for one value that has never changed is more to go wrong than the
   duplication it removes. The comment beside it in shell.css says so too. */
const MOTION_EASE = 'cubic-bezier(0.2, 0.8, 0.2, 1)';

/* The single question every helper asks before doing anything. reducedMotion()
   is geometry.js's, checked at the point of use so that changing the setting
   takes effect without a reload — see the note there. */
function motionAllowed() {
  return motionReady && !reducedMotion();
}

/* ------------------------------------------------------------------------
 * The bar
 * --------------------------------------------------------------------- */

/* The bar arriving.
 *
 * Under `auto` this runs every time Mod4 is held, so it is short and it is
 * cheap. The split between what fades and what moves is not a taste: under
 * `auto` the bar's own rect is measured and handed to the compositor as an
 * overlay — see setOverlay in geometry.js — so a bar that slid would be drawn
 * cropped to a rectangle it had already left. The strip fades; its contents,
 * which nothing measures, are what move.
 *
 * The stagger is the reason this is here rather than in the stylesheet. The
 * modules are six spans that come and go with what the compositor has to say
 * about the network, and dealing them out one after another in CSS would mean
 * a nth-child delay per module, written against a count the markup owns. */
function animateBarIn(output) {
  /* A desktop with no bar in it is not an error — a theme may have taken it
     out, and the shell is a page anyone may replace. There is simply nothing
     to reveal. */
  if (!motionAllowed() || !output.barEl) return;

  const slow = motionSeconds('--anim-slow');
  const halves = [output.barEl.querySelector('.bar-left'),
    output.barEl.querySelector('.bar-right')].filter(Boolean);
  /* Only what is on screen. `.module:empty` collapses a status the compositor
     has not sent yet, and staggering an invisible span spends a slot in the
     sequence on nothing — the visible modules would arrive with a gap in the
     middle for no reason anyone could see. */
  const items = halves.flatMap((half) => [...half.children]
    .filter((el) => !el.hidden && el.textContent !== ''));

  const timeline = gsap.timeline();
  timeline.fromTo(output.barEl, { opacity: 0 }, {
    opacity: 1, duration: slow, ease: MOTION_EASE, clearProps: 'opacity',
  });
  if (items.length > 0) {
    /* Four pixels, and not more. Under 'auto' the compositor draws the bar
       cropped to the strip's own rect, so the part of a module that is still
       above its resting place is the part that is outside the crop — it is
       drawn at all only because it is a few pixels at the top of a fade. A
       longer throw would be a visibly clipped animation. */
    timeline.fromTo(items, { opacity: 0, y: -4 }, {
      opacity: 1,
      y: 0,
      duration: slow,
      ease: MOTION_EASE,
      /* `amount`, not `each`: the whole sequence takes this long however many
         modules there are, so a bar with the network module showing does not
         take visibly longer to arrive than one without it. */
      stagger: { amount: slow * 0.6, from: 'edges' },
      /* The elements are retained — the bar is built once per output and
         updated in place — so anything left behind on them is left behind for
         good. Clearing returns them to the stylesheet's idea of themselves. */
      clearProps: 'opacity,transform',
    }, 0);
  }
}

/* The binding-mode badge appearing.
 *
 * It shows because a key was pressed and the keyboard now means something
 * else, which is the one state change on this bar worth a beat of motion. A
 * back-eased pop overshoots slightly and settles, which reads as a thing
 * arriving rather than a thing that was always there.
 *
 * `display` cannot be transitioned, which is why this was a keyframed entrance
 * before and is a tween now: both restart from nothing when the element stops
 * being hidden. It is called on the edge only — see renderBarModules — so a
 * status sample every two seconds does not re-pop a badge that has been
 * sitting there since the key was pressed. */
function animateModeIn(el) {
  if (!motionAllowed()) return;

  gsap.fromTo(el, { opacity: 0, scale: 0.88 }, {
    opacity: 1,
    scale: 1,
    duration: motionSeconds('--anim'),
    /* A small overshoot. The badge sits inside the bar, and under 'auto' the
       bar is drawn cropped to its own rect — so the ease is one that settles
       just past its resting size rather than one that bounces out of the strip
       it lives in. */
    ease: 'back.out(1.4)',
    clearProps: 'opacity,transform',
  });
}

/* ------------------------------------------------------------------------
 * Notifications
 * --------------------------------------------------------------------- */

/* The strip's rectangle is handed to the compositor so it can be drawn in
 * front of the windows underneath it, and it stays until the last child is
 * removed: a notification that is animating away is still a child of its
 * container until removal, so it still counts as being there. See
 * reportNotificationRect. */

/* The properties both tweens move, and the only ones they may.
 *
 * A notification is drawn over a window by the compositor redrawing this strip
 * of the shell on top of it, cropped to the rectangle the page reported. What
 * is *behind* the notification inside that rectangle is not the window — it is
 * the page, which is to say the wallpaper. So anything that leaves part of the
 * reported rectangle uncovered puts the desktop background over the window for
 * as long as it lasts:
 *
 *   * Opacity does it everywhere at once. A notification fading out is a
 *     rectangle of wallpaper fading *in* over whatever it was covering.
 *   * A transform does it at the edges. `scale` shrinks what is painted and
 *     leaves the layout box — which is what the rectangle is measured from —
 *     exactly where it was.
 *
 * Both were how these two animated, and both are why closing a notification
 * showed the background colour behind it. What is left is the box itself:
 * collapsing and growing the height, and the padding and borders with it, so
 * the shrinking rectangle and the shrinking picture are the same shrinking
 * thing. `.notification` is `overflow: hidden` so the text inside is clipped
 * by that box rather than spilling out of it. */
const NOTIFICATION_BOX = [
  'height',
  'marginTop',
  'marginBottom',
  'paddingTop',
  'paddingBottom',
  'borderTopWidth',
  'borderBottomWidth',
];

/* The collapsed state, as tween values: every box property at zero. */
function notificationCollapsed() {
  return Object.fromEntries(NOTIFICATION_BOX.map((key) => [key, 0]));
}

/* One arriving.
 *
 * It grows into place rather than sliding in from the edge of the screen,
 * which is what a notification usually does and what this cannot: the strip's
 * rect is measured off the container and handed to the compositor, which draws
 * this piece of the shell cropped to exactly that, and a child translated past
 * the container's edge is drawn outside the crop — which is to say not drawn.
 *
 * `from` rather than `fromTo`, because the values it grows *to* are whatever
 * the stylesheet and the text made them: the box is measured, never assumed.
 * See NOTIFICATION_BOX for why the growth is the box and not a fade. */
function animateNotificationIn(el) {
  if (!motionAllowed()) return;

  gsap.from(el, {
    ...notificationCollapsed(),
    duration: motionSeconds('--anim-slow'),
    ease: MOTION_EASE,
    clearProps: NOTIFICATION_BOX.join(','),
    /* The strip grew, and it is growing for the length of the tween. The
       overlay rect is measured off the container, so it has to be re-measured
       while the thing inside it is still settling or the compositor spends the
       entrance drawing a rectangle the size of the stack before this one
       arrived. */
    onUpdate: reportNotificationRect,
  });
}

/* One leaving.
 *
 * This is the case CSS cannot do at all. The entrance is a rule on an element
 * that exists; the exit has to happen to an element that is being taken away,
 * and `remove()` lands before any frame of an animation on it is drawn. So the
 * removal is what this function is given, and it runs when the tween ends.
 *
 * The height collapse is the half that matters. Dismissing the top of three
 * notifications leaves the other two to close the gap, and without animating
 * the box away they jump — a fade would take the pixels and leave the space
 * they were in. */
function animateNotificationOut(el, remove) {
  if (!motionAllowed()) {
    remove();
    return;
  }

  const done = () => {
    remove();
  };

  /* One tween, not two. This used to fade and shrink first and collapse the
     box afterwards, and the fade was the bug the user saw: for the length of
     it the compositor went on drawing a strip-sized rectangle of shell over
     the window while the notification inside it dissolved, so what appeared
     underneath was the wallpaper rather than the window that was really
     there. See NOTIFICATION_BOX.

     Measured, not assumed: the box is whatever the text made it, and the
     stack above has to travel exactly that far. GSAP reads the current
     computed values itself, which is why only the destination is named. */
  gsap.to(el, {
    ...notificationCollapsed(),
    duration: motionSeconds('--anim'),
    ease: MOTION_EASE,
    onUpdate: reportNotificationRect,
    onComplete: done,
  });
}

/* ------------------------------------------------------------------------
 * The screen-share chooser
 * --------------------------------------------------------------------- */

/* The dialog arriving, and its sources dealt out.
 *
 * Opacity on the dialog and nothing else, for the reason written beside
 * `.screencast-dialog` in shell.css: the rect handed to the compositor is
 * measured off that element, and a dialog that scaled or slid would be drawn
 * against a stale rectangle for the length of the animation.
 *
 * The rows inside it are a different matter. They are laid out within the
 * dialog's padding and nothing measures them, so they may move — and they
 * start below their resting place, which keeps every frame of the movement
 * inside the box the compositor is cropping to. */
function animateScreencastIn(dialog, rows) {
  if (!motionAllowed()) return;

  const slow = motionSeconds('--anim-slow');
  const timeline = gsap.timeline();
  timeline.fromTo(dialog, { opacity: 0 }, {
    opacity: 1, duration: slow, ease: MOTION_EASE, clearProps: 'opacity',
  });
  if (rows.length > 0) {
    timeline.fromTo(rows, { opacity: 0, y: 8 }, {
      opacity: 1,
      y: 0,
      duration: slow,
      ease: MOTION_EASE,
      stagger: { amount: slow * 0.8 },
      clearProps: 'opacity,transform',
    }, slow * 0.25);
  }
}

/* ------------------------------------------------------------------------
 * Window surfaces
 * --------------------------------------------------------------------- */

/* Fade a value that is not a style.
 *
 * A window's frame is the shell's and its contents are a surface the
 * compositor draws, so no rule in shell.css can touch the opacity of what is
 * actually on screen. The compositor holds whatever value it was last told and
 * interpolates nothing, which means the samples *are* the fade: this tweens a
 * plain number and hands each sample to the caller, who sends it over IPC.
 *
 * Sampled every other frame. Each view.opacity makes the compositor walk the
 * window's whole surface tree with wlr_scene_node_for_each_buffer(), so the
 * price of the tween is one tree walk per sample per window opened, and over a
 * tenth of a second a 30Hz ramp is indistinguishable from a 60Hz one. The last
 * sample is never skipped: it is the one that leaves the window at its resting
 * opacity, and dropping it would leave a window permanently dimmer than it
 * should be.
 *
 * Returns whether a tween was started, which is what lets a caller with its
 * own opinion about this window's opacity — solar, whose cold tiers rest well
 * below 1 — leave one in progress alone rather than talking over it. */
function surfaceOpacityTween(resting, sample) {
  if (!motionAllowed()) return false;

  const state = { value: 0 };
  let frame = 0;

  gsap.to(state, {
    value: resting,
    /* Shorter than the layout's own animations on purpose: a window opening is
       already accompanied by everything beside it moving over to make room,
       and a fade that outlasted that movement would still be brightening after
       the desktop had settled. */
    duration: motionSeconds('--anim'),
    ease: MOTION_EASE,
    onUpdate() {
      if (++frame % 2 === 0) sample(state.value);
    },
    onComplete() {
      sample(resting);
    },
  });
  return true;
}
