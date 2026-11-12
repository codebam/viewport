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
 *     rule here can reach it. See runMotion.
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

/* The easing every tween runs on, read from the cascade the way the durations
   are. It used to be a constant duplicating `--ease` in shell.css; the config
   file's `motion.ease` now lands on that property, so a tween left on the old
   constant would move on a different curve from every CSS transition beside
   it. GSAP accepts a `cubic-bezier(...)` string directly — the old constant
   was one. Cached like the durations, and cleared by applyMotion. */
let motionEaseValue = null;

/* The motion tree, resolved: every leaf with the pace, curve and spring it
   actually runs on. A leaf names what it wants and inherits the rest — first
   from `windows`, then from the block's own pace and curve, then from the
   stylesheet — which is Hyprland's `animation` block over an inherited tree,
   in this file's shape. Rebuilt by applyMotion; read through motionLeaf. */
let motionLeaves = new Map();

function motionEase() {
  if (motionEaseValue !== null) return motionEaseValue;
  let ease = 'cubic-bezier(0.2, 0.8, 0.2, 1)';
  if (typeof getComputedStyle === 'function') {
    const raw = getComputedStyle(document.documentElement)
      .getPropertyValue('--ease').trim();
    if (raw) ease = raw;
  }
  motionEaseValue = ease;
  return ease;
}

/* The config file's `motion` block, landed on the properties the stylesheet
   already animates on. The shell is a page, so its motion has always been CSS
   — this is the key that reaches it, and the reason a desk can tune its pace
   without shipping a stylesheet.

   `enabled: false` is the system's reduced-motion setting said in another
   place: the class zeroes both durations for the CSS transitions, and
   reducedMotion() reads the flag for the tweens. The system setting wins over
   a configured duration either way — an inline `--anim` would otherwise
   override the media query that is there to protect it. */
function applyMotion(motion) {
  if (motion === undefined || motion === null) return;
  const root = document.documentElement;
  const systemReduced = typeof matchMedia === 'function' &&
    matchMedia('(prefers-reduced-motion: reduce)').matches;
  if (typeof motion.enabled === 'boolean') motionEnabled = motion.enabled;

  if (motion.enabled === false || systemReduced) {
    root.classList.add('motion-off');
    root.style.removeProperty('--anim');
    root.style.removeProperty('--anim-slow');
  } else {
    root.classList.remove('motion-off');
    if (Number.isFinite(motion.duration) && motion.duration >= 0) {
      root.style.setProperty('--anim', motion.duration + 'ms');
    }
    if (Number.isFinite(motion.slow) && motion.slow >= 0) {
      root.style.setProperty('--anim-slow', motion.slow + 'ms');
    }
  }
  /* The read-once caches above have to go, or a reload that changed the pace
     would keep the numbers the first frame read. The ease is cached here too
     so a tween does not read the cascade again, and so the value is the one
     just applied rather than whatever the engine reports a frame later. */
  motionTimings.clear();
  motionEaseValue = null;
  if (typeof motion.ease === 'string' && motion.ease.trim() !== '') {
    const ease = motion.ease.trim();
    root.style.setProperty('--ease', ease);
    motionEaseValue = ease;
  }

  /* The tree itself, resolved once so a tween does not re-derive inheritance
     on every frame. `windows` is the umbrella the other three inherit from;
     the block's own `duration` and `ease` are the floor under that. */
  const treeRoot = {
    duration: finiteNumber(motion.duration),
    ease: nonEmptyString(motion.ease),
    bezier: bezierOf(motion.bezier),
    spring: null,
    style: null,
    enabled: null,
  };
  const umbrella = resolveMotionLeaf('windows', motion.windows, null, treeRoot);
  motionLeaves = new Map([['windows', umbrella]]);
  for (const [name, raw] of [
    ['windows_in', motion.windows_in],
    ['windows_move', motion.windows_move],
    ['workspaces', motion.workspaces],
  ]) {
    motionLeaves.set(name, resolveMotionLeaf(name, raw, umbrella, treeRoot));
  }

  /* The move category reaches the stylesheet through its own variables: the
     window's transform transition is the one CSS animation a motion leaf
     owns — see `.window` in shell.css. Everything else that springs or
     styles is driven from JavaScript. */
  const move = motionLeaves.get('windows_move');
  if (motion.enabled === false || systemReduced) {
    root.style.setProperty('--anim-move', '0ms');
    root.style.setProperty('--ease-move', 'linear');
  } else {
    root.style.setProperty('--anim-move', `${leafDurationMs(move, '--anim')}ms`);
    root.style.setProperty('--ease-move', leafCurve(move));
  }
}

/* A number the config can prove: anything else is absent, not zero. */
function finiteNumber(value) {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function nonEmptyString(value) {
  return typeof value === 'string' && value.trim() !== '' ? value.trim() : null;
}

/* A curve written as four numbers, in the spelling CSS wants. */
function bezierOf(value) {
  if (!Array.isArray(value) || value.length !== 4) return null;
  if (!value.every((n) => typeof n === 'number' && Number.isFinite(n))) return null;
  return `cubic-bezier(${value.map((n) => String(Number(n.toFixed(4)))).join(', ')})`;
}

/* A spring as the physics wants it: mass, stiffness and damping, defaulted the
   way the compositor defaults them. */
function springOf(spring) {
  if (!spring || typeof spring !== 'object') return null;
  const mass = finiteNumber(spring.mass) ?? 1;
  const stiffness = finiteNumber(spring.stiffness) ?? 400;
  const damping = finiteNumber(spring.damping) ?? 40;
  if (!(mass > 0 && stiffness > 0 && damping >= 0)) return null;
  return { mass, stiffness, damping };
}

/* One leaf, resolved. `umbrella` is the `windows` leaf it may inherit from;
   `root` is the block itself. */
function resolveMotionLeaf(name, raw, umbrella, root) {
  const leaf = raw && typeof raw === 'object' ? raw : {};
  const parent = umbrella ?? {};
  /* The style is the leaf's own: the styles a category may name are different
     ones — `popin` is an arrival, not a workspace switch — so inheriting one
     from `windows` would apply a style the category never offered. Pace,
     curve, spring and the switch still inherit. */
  const style = nonEmptyString(leaf.style);
  const duration = finiteNumber(leaf.duration) ?? finiteNumber(parent.duration)
    ?? root.duration;
  const ease = nonEmptyString(leaf.ease) ?? nonEmptyString(parent.ease)
    ?? root.ease;
  const bezier = bezierOf(leaf.bezier) ?? bezierOf(parent.bezier) ?? root.bezier;
  const spring = springOf(leaf.spring) ?? springOf(parent.spring);
  const enabled = typeof leaf.enabled === 'boolean' ? leaf.enabled
    : typeof parent.enabled === 'boolean' ? parent.enabled : null;
  return { name, style, duration, ease, bezier, spring, enabled };
}

/* The leaf named `name`, or null when no tree has been applied. */
function motionLeaf(name) {
  return motionLeaves.get(name) ?? null;
}

/* A leaf's pace in milliseconds, falling back to the stylesheet property it
   replaces so an unconfigured tree keeps the old timing exactly. */
function leafDurationMs(leaf, fallbackProperty = '--anim') {
  if (leaf && leaf.enabled === false) return 0;
  if (leaf && typeof leaf.duration === 'number' && Number.isFinite(leaf.duration)) {
    return Math.max(0, leaf.duration);
  }
  return Math.max(0, motionSeconds(fallbackProperty) * 1000);
}

/* A leaf's curve as a CSS timing function: its own numbers first, then its
   own string, then the stylesheet's. */
function leafCurve(leaf) {
  return (leaf && (leaf.bezier ?? leaf.ease)) || motionEase();
}

/* The same curve evaluated at `t`, for the hand-driven animations. */
function easeValue(ease, t) {
  if (!(t > 0)) return 0;
  if (t >= 1) return 1;
  const [x1, y1, x2, y2] = parseCurve(ease);
  /* Solve x(u) = t, then read y(u): a cubic-bezier names x as well as y, and
     the timing function is y at the u whose x is the elapsed fraction. */
  let low = 0;
  let high = 1;
  let u = t;
  for (let i = 0; i < 24; i++) {
    u = (low + high) / 2;
    if (bezierAxis(u, x1, x2) < t) low = u;
    else high = u;
  }
  return bezierAxis(u, y1, y2);
}

const CURVE_KEYWORDS = {
  linear: [0, 0, 1, 1],
  ease: [0.25, 0.1, 0.25, 1],
  'ease-in': [0.42, 0, 1, 1],
  'ease-out': [0, 0, 0.58, 1],
  'ease-in-out': [0.42, 0, 0.58, 1],
};

function parseCurve(ease) {
  if (typeof ease !== 'string') return CURVE_KEYWORDS.ease;
  const text = ease.trim();
  if (CURVE_KEYWORDS[text]) return CURVE_KEYWORDS[text];
  const match = /^cubic-bezier\(\s*([^)]+)\)$/.exec(text);
  if (!match) return CURVE_KEYWORDS.ease;
  const numbers = match[1].split(',').map((part) => Number(part.trim()));
  if (numbers.length !== 4 || numbers.some((n) => !Number.isFinite(n))) {
    return CURVE_KEYWORDS.ease;
  }
  return numbers;
}

/* One axis of a cubic bezier with endpoints 0 and 1, at parameter `u`. */
function bezierAxis(u, a, b) {
  const inverse = 1 - u;
  return 3 * inverse * inverse * u * a + 3 * inverse * u * u * b + u * u * u;
}

/* How long a hand-driven animation may run before it is cut off. Generous for
   a slow spring, short enough that a mistake cannot leave a frame loop
   running for a second. */
const MOTION_FRAME_CAP = 240;

/* Run one hand-driven animation, sampling progress from 0 to 1.

   The same loop drives both kinds: a spring is integrated from the clock with
   the reference formula, and a paced animation is the elapsed fraction put
   through its curve. Returns whether anything was started — a reduced-motion
   session and a zero-duration category sample 1 at once and return false. */
function runMotion(leaf, sample, done) {
  if (!motionAllowed() || !leaf || leaf.enabled === false
      || typeof requestAnimationFrame !== 'function') {
    sample(1);
    if (done) done();
    return false;
  }
  const ms = leafDurationMs(leaf);
  if (ms <= 0 && !leaf.spring) {
    sample(1);
    if (done) done();
    return false;
  }
  if (leaf.spring) {
    runSpring(leaf.spring, sample, done);
    return true;
  }
  const duration = ms / 1000;
  const curve = leafCurve(leaf);
  let started = null;
  let frames = 0;
  const step = (now) => {
    if (started === null) started = now;
    const t = Math.min(1, Math.max(0, (now - started) / 1000 / duration));
    sample(easeValue(curve, t));
    if (t < 1 && ++frames < MOTION_FRAME_CAP) requestAnimationFrame(step);
    else if (done) done();
  };
  requestAnimationFrame(step);
  return true;
}

/* The reference spring, advanced closed-form over one frame's elapsed time:
   the result does not depend on how the frames happened to fall. */
function runSpring(spring, sample, done) {
  const state = { value: 0, velocity: 0 };
  let last = null;
  let frames = 0;
  const step = (now) => {
    if (last === null) last = now;
    const dt = Math.min(0.064, Math.max(0, (now - last) / 1000));
    last = now;
    springAdvance(state, spring, dt);
    sample(Math.max(0, Math.min(1.05, state.value)));
    const settled = Math.abs(state.value - 1) < 0.001
      && Math.abs(state.velocity) < 0.001;
    if (!settled && ++frames < MOTION_FRAME_CAP) requestAnimationFrame(step);
    else {
      sample(1);
      if (done) done();
    }
  };
  requestAnimationFrame(step);
}

function springAdvance(state, spring, dt) {
  if (!(dt > 0)) return state;
  const mass = Math.max(spring.mass, 0.0001);
  const stiffness = Math.max(spring.stiffness, 0.0001);
  const damping = Math.max(spring.damping, 0);
  const displacement = state.value - 1;
  const omega0 = Math.sqrt(stiffness / mass);
  const gamma = damping / (2 * mass);
  let value;
  let velocity;
  if (gamma < omega0) {
    const omegad = Math.sqrt(omega0 * omega0 - gamma * gamma);
    const decay = Math.exp(-gamma * dt);
    const sin = Math.sin(omegad * dt);
    const cos = Math.cos(omegad * dt);
    value = 1 + decay * (displacement * cos
      + ((state.velocity + gamma * displacement) / omegad) * sin);
    velocity = decay * (state.velocity * cos
      - ((gamma * state.velocity + omega0 * omega0 * displacement) / omegad) * sin);
  } else if (Math.abs(gamma - omega0) <= Math.max(omega0, 1) * 0.0001) {
    const decay = Math.exp(-gamma * dt);
    const b = state.velocity + gamma * displacement;
    value = 1 + decay * (displacement + b * dt);
    velocity = decay * (state.velocity - gamma * b * dt);
  } else {
    const root = Math.sqrt(gamma * gamma - omega0 * omega0);
    const r1 = -gamma + root;
    const r2 = -gamma - root;
    const a = (state.velocity - r2 * displacement) / (r1 - r2);
    const b = displacement - a;
    const e1 = Math.exp(r1 * dt);
    const e2 = Math.exp(r2 * dt);
    value = 1 + a * e1 + b * e2;
    velocity = a * r1 * e1 + b * r2 * e2;
  }
  state.value = value;
  state.velocity = velocity;
  return state;
}

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
    opacity: 1, duration: slow, ease: motionEase(), clearProps: 'opacity',
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
      ease: motionEase(),
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
    ease: motionEase(),
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
    ease: motionEase(),
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
    opacity: 1, duration: slow, ease: motionEase(), clearProps: 'opacity',
  });
  if (rows.length > 0) {
    timeline.fromTo(rows, { opacity: 0, y: 8 }, {
      opacity: 1,
      y: 0,
      duration: slow,
      ease: motionEase(),
      stagger: { amount: slow * 0.8 },
      clearProps: 'opacity,transform',
    }, slow * 0.25);
  }
}


