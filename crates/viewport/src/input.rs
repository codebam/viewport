// SPDX-License-Identifier: GPL-3.0-or-later
//
// Input routing. Ports the parts of src/input.c that step 2 needs.
//
// Adapted from smithay's MIT-licensed `smallvil` example.
//
// Hit-testing falls out of the layering rather than being computed: whatever
// `surface_under` returns is the client under the pointer, and nothing there
// means the pointer is over the shell. That is why "the click went to the
// titlebar" versus "the click went to the app" needs no geometry bookkeeping
// and cannot go stale mid-animation.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, GestureBeginEvent,
    GestureEndEvent, GesturePinchUpdateEvent as _, GestureSwipeUpdateEvent as _, InputBackend,
    InputEvent, InputTime, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    PointerMotionEvent, TouchEvent,
};
use smithay::input::keyboard::{keysyms, FilterResult, Keysym, ModifiersState};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, MotionEvent,
};
use smithay::input::tablet::{TabletDescriptor, TabletSeatTrait as _};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Serial, SERIAL_COUNTER};

/// The two buttons a drag can start with, as libinput numbers them.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

const SWIPE_THRESHOLD: f64 = 120.0;
const PINCH_OUT_THRESHOLD: f64 = 1.2;
const PINCH_IN_THRESHOLD: f64 = 0.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureKind {
    Swipe,
    Pinch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureDirection {
    Left,
    Right,
    Up,
    Down,
    In,
    Out,
}

/// One configured discrete touchpad gesture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GestureBinding {
    kind: GestureKind,
    fingers: u32,
    direction: GestureDirection,
    action: crate::binding::Action,
}

/// A gesture captured at begin. Once captured, no part reaches a client.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GestureState {
    Swipe { fingers: u32, dx: f64, dy: f64 },
    Pinch { fingers: u32, scale: f64 },
}

/// Parse `swipe:3:left` or `pinch:2:out` with an ordinary binding action.
pub fn parse_gesture(spec: &str, action: &str) -> Option<GestureBinding> {
    let mut parts = spec.split(':');
    let kind = match parts.next()? {
        "swipe" => GestureKind::Swipe,
        "pinch" => GestureKind::Pinch,
        _ => return None,
    };
    let fingers = parts.next()?.parse().ok()?;
    if fingers == 0 {
        return None;
    }
    let direction = match (kind, parts.next()?) {
        (GestureKind::Swipe, "left") => GestureDirection::Left,
        (GestureKind::Swipe, "right") => GestureDirection::Right,
        (GestureKind::Swipe, "up") => GestureDirection::Up,
        (GestureKind::Swipe, "down") => GestureDirection::Down,
        (GestureKind::Pinch, "in") => GestureDirection::In,
        (GestureKind::Pinch, "out") => GestureDirection::Out,
        _ => return None,
    };
    if parts.next().is_some() || action.trim().is_empty() {
        return None;
    }
    Some(GestureBinding {
        kind,
        fingers,
        direction,
        action: crate::binding::parse_action(action.trim()),
    })
}

fn captures_gesture(bindings: &[GestureBinding], kind: GestureKind, fingers: u32) -> bool {
    bindings
        .iter()
        .any(|binding| binding.kind == kind && binding.fingers == fingers)
}

fn finish_gesture(
    state: GestureState,
    bindings: &[GestureBinding],
    cancelled: bool,
) -> Option<crate::binding::Action> {
    if cancelled {
        return None;
    }
    let (kind, fingers, direction) = match state {
        GestureState::Swipe {
            fingers, dx, dy, ..
        } => {
            let direction = if dx.abs() >= SWIPE_THRESHOLD && dx.abs() > dy.abs() {
                if dx < 0.0 {
                    GestureDirection::Left
                } else {
                    GestureDirection::Right
                }
            } else if dy.abs() >= SWIPE_THRESHOLD && dy.abs() > dx.abs() {
                if dy < 0.0 {
                    GestureDirection::Up
                } else {
                    GestureDirection::Down
                }
            } else {
                return None;
            };
            (GestureKind::Swipe, fingers, direction)
        }
        GestureState::Pinch { fingers, scale, .. } => {
            let direction = if scale >= PINCH_OUT_THRESHOLD {
                GestureDirection::Out
            } else if scale <= PINCH_IN_THRESHOLD {
                GestureDirection::In
            } else {
                return None;
            };
            (GestureKind::Pinch, fingers, direction)
        }
    };
    bindings
        .iter()
        .find(|binding| {
            binding.kind == kind && binding.fingers == fingers && binding.direction == direction
        })
        .map(|binding| binding.action.clone())
}

use crate::state::ViewportState;
use crate::views::NO_VIEW;

/// The protocol slot a scripted touch names.
///
/// libinput reports an `Option<u32>`; a script names a finger as `0`, `1`.
/// `None` is the unnamed single-touch slot, which a script has no reason to
/// ask for.
fn inject_touch_slot(slot: u32) -> smithay::backend::input::TouchSlot {
    smithay::backend::input::TouchSlot::from(Some(slot))
}

/// Run a command, detached.
///
/// Double-forked through a shell so the compositor does not accumulate
/// zombies and a launched application outlives the key that started it.
pub fn spawn(command: &str) {
    spawn_with_env(command, &[])
}

/// The same, with variables added to the environment the command is run in.
///
/// The launcher's use of it: an xdg-activation token minted for the process,
/// which the application presents when its window appears and the compositor
/// honours as "focus this, a token says it was asked for".
pub fn spawn_with_env(command: &str, extra: &[(String, String)]) {
    use std::process::{Command, Stdio};

    tracing::info!("exec: {command}");
    let mut child = Command::new("/bin/sh");
    // Which engine *this* compositor draws its shell with is not a preference
    // to hand down.
    //
    // An installed Viewport is wrapped with `VIEWPORT_SHELL_BACKEND` set to
    // the engine it ships, and every process the session starts inherits it —
    // so a second Viewport run from a terminal inside the first picks up the
    // first one's engine whatever package it came from, and says nothing about
    // why. That surfaced as `--background-terminal` refusing to start under a
    // compositor whose whole package exists to be the backend that supports
    // it: the cef session's variable had followed the terminal, the `nix run`
    // typed into it, and the webkitgtk build it started.
    //
    // The variable is how someone *asks* for an engine, and that ask belongs
    // to the shell they typed it in, not to a compositor three processes up.
    child.env_remove("VIEWPORT_SHELL_BACKEND");
    for (key, value) in extra {
        child.env(key, value);
    }
    let result = child
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Kept rather than discarded. A binding that failed used to leave the
        // log saying only that it had been started: a screenshot script that
        // died on a missing tool, a bad argument or a `set -e` was
        // indistinguishable from one that worked, from anywhere.
        .stderr(Stdio::piped())
        .spawn();
    match result {
        Ok(mut child) => {
            let stderr = child.stderr.take();
            let what = command.to_owned();
            // Reaped on a thread of its own: sh execs the command, so this
            // waits for the application itself, which the compositor cannot.
            std::thread::spawn(move || {
                if let Some(stderr) = stderr {
                    log_stderr(&what, stderr);
                }
                match child.wait() {
                    // A command that failed says so, once, with its status.
                    // Nothing else in the session would ever mention it.
                    Ok(status) if !status.success() => {
                        tracing::warn!("{what} exited {status}");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("could not wait for {what}: {e}"),
                }
            });
        }
        Err(e) => tracing::error!("could not run {command}: {e}"),
    }
}

/// How many lines of a child's stderr reach the log.
///
/// A failing script says what is wrong in its first few lines. A browser left
/// running for a day writes tens of thousands, and a compositor log that is
/// mostly one client's chatter is no more use than one that says nothing.
const STDERR_LINES: usize = 20;

/// Copy a child's stderr into the log, tagged with the command.
///
/// Reading continues past the cap even though logging stops: a pipe nobody
/// drains fills up, and the next write blocks the child in the middle of
/// whatever it was doing — which would turn a chatty application into a hung
/// one.
fn log_stderr(command: &str, stderr: std::process::ChildStderr) {
    use std::io::BufRead as _;

    let mut lines = 0usize;
    let mut suppressed = 0usize;
    for line in std::io::BufReader::new(stderr).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if lines < STDERR_LINES {
            lines += 1;
            tracing::warn!("{command}: {line}");
            if lines == STDERR_LINES {
                tracing::warn!("{command}: further output is not logged");
            }
        } else {
            suppressed += 1;
        }
    }
    if suppressed > 0 {
        tracing::debug!("{command}: {suppressed} more lines went unlogged");
    }
}

/// A chord the compositor keeps for itself.
///
/// Deliberately tiny. Everything else a compositor binds belongs to the shell,
/// which sends `bind.add` — but these two cannot, because they have to work
/// when the shell is broken or absent. Without them a compositor on a real
/// TTY is inescapable: VT switching needs the compositor to act on the chord,
/// and there is otherwise no way to stop it from the machine it is running on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Ctrl+Alt+F1 through F12.
    SwitchVt(i32),
    /// Ctrl+Alt+Backspace.
    Quit,
    /// Ctrl+Alt+G, the same as a virtual machine's ungrab.
    ToggleCapture,
    /// The release half of an intercepted chord.
    Swallow,
    /// A binding fired.
    Bound(crate::binding::Action),
    /// A key aimed at the screen-share chooser, which is drawn by the shell
    /// and steered from here.
    Pick(Pick),
    /// A key for the shell's page: nothing else holds the keyboard.
    Web(WebKey),
}

/// A key on its way to the shell, in the terms WPE wants it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebKey {
    /// An X11-style keycode — evdev's, offset by eight — which is what WPE
    /// expects and what the C build sent (`src/input.c:963`).
    pub keycode: u32,
    pub keysym: u32,
    pub pressed: bool,
    pub modifiers: u32,
    pub time: u32,
}

/// What a key does while the chooser is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// Move the highlight by one, up or down the list.
    Step(i8),
    Confirm,
    Cancel,
}

/// Match a key against the compositor's own chords.
///
/// xkb turns Ctrl+Alt+Fn into an `XF86Switch_VT_n` keysym on its own, so the
/// modifiers do not have to be checked for that one — the keysym only exists
/// because they were held.
fn shortcut(modifiers: &ModifiersState, keysym: Keysym) -> Option<Action> {
    let raw = keysym.raw();

    if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&raw) {
        return Some(Action::SwitchVt(
            (raw - keysyms::KEY_XF86Switch_VT_1 + 1) as i32,
        ));
    }

    if modifiers.ctrl && modifiers.alt && raw == keysyms::KEY_BackSpace {
        return Some(Action::Quit);
    }

    // The ungrab chord, the one QEMU and virt-manager use. Nested, this hands
    // the host back its own shortcuts and takes them again; on real hardware
    // there is no host and nothing to hand anything to.
    if modifiers.ctrl && modifiers.alt && (raw == keysyms::KEY_g || raw == keysyms::KEY_G) {
        return Some(Action::ToggleCapture);
    }

    None
}

/// What a button event means to a window drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragEffect {
    /// No drag is running: the event is the button handler's own, and a press
    /// may start one.
    Free,
    /// The button that started the drag came up. The drag is over.
    End,
    /// A button belonging to a drag already running. It goes no further —
    /// neither to the client under the pointer, which is being moved rather
    /// than clicked, nor back into the drag-start path, which would turn a
    /// move into a resize under the hand that was moving it.
    Swallow,
}

/// Which of those a button event is.
///
/// `held` is the button the running drag was started with, if there is one.
/// Split out from the handler because it is the whole of the state machine and
/// the handler around it needs a compositor to run.
fn drag_effect(held: Option<u32>, button: u32, pressed: bool) -> DragEffect {
    match held {
        None => DragEffect::Free,
        Some(held) if !pressed && held == button => DragEffect::End,
        Some(_) => DragEffect::Swallow,
    }
}

// Buttons whose press a binding took, so the matching release can be taken
// too — `suppressed_keys` for the mouse.
//
// A press that fires a binding is not forwarded, and a release that is
// forwarded on its own is a client told a button came up that it never saw go
// down: a browser that starts a text selection on the release, a canvas that
// ends a stroke it never began. Matching the release against the bindings
// again would not do, because the modifier is usually released before the
// button and the chord no longer matches by then.
//
// A thread local rather than a field on the state: input is dispatched on the
// compositor's own thread and nowhere else, and this is the button handler's
// private bookkeeping.
thread_local! {
    static SUPPRESSED_BUTTONS: std::cell::RefCell<Vec<u32>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Remember that this button's press was consumed by a binding.
fn suppress_button(button: u32) {
    SUPPRESSED_BUTTONS.with(|buttons| {
        let mut buttons = buttons.borrow_mut();
        if !buttons.contains(&button) {
            buttons.push(button);
        }
    });
}

/// Whether this release belongs to a press a binding consumed, forgetting it if
/// so — one release per press, as a second one is a button that went down again
/// somewhere this did not see.
fn release_suppressed(button: u32) -> bool {
    SUPPRESSED_BUTTONS.with(|buttons| {
        let mut buttons = buttons.borrow_mut();
        match buttons.iter().position(|b| *b == button) {
            Some(at) => {
                buttons.remove(at);
                true
            }
            None => false,
        }
    })
}

/// Whether a press starts one of the compositor's own pointer gestures —
/// Mod4 and a button to move, resize or pan.
///
/// `on_overlay` is the part worth naming. Everything the shell draws in front
/// of the windows and asks to be clicked — the bar it floats under `auto`, a
/// notification, the screen-share chooser — is a thing to click and not a
/// handle for a gesture, and the gesture has to be declined *before* the
/// question of what is underneath. The old guard only declined the drag of a
/// window found under the pointer, so a press over the bar with no window
/// beneath fell through to the pan, was swallowed by it, and never reached the
/// page. Under `auto` that is every click the bar can ever get, because the
/// bar is on screen only while Mod4 is held.
///
/// Split out from the handler for the same reason `drag_effect` is: the rule
/// is small and the handler around it needs a compositor to run.
fn starts_gesture(pressed: bool, grabbed: bool, on_overlay: bool, logo: bool, button: u32) -> bool {
    pressed && !grabbed && !on_overlay && logo && matches!(button, BTN_LEFT | BTN_RIGHT)
}

/// Which edges of a window a resize drag takes hold of, from where the press
/// landed inside it.
///
/// By half rather than by a border strip: the whole window is the handle —
/// Mod4 and the right button, with no edge to aim at — so every point in it
/// has to name a corner, and the nearest one is the only answer that does not
/// leave a dead zone in the middle. Grabbing in the bottom right quarter is
/// what the drag has always done, and still does; the other three quarters are
/// what this adds.
///
/// The returned pair is `(west, north)`: whether the drag moves the left edge
/// and whether it moves the top one. The shell turns that into which sibling
/// gives up the space, or which way a floating window's corner is pinned.
fn resize_edges(pos: Point<f64, Logical>, geometry: Rectangle<i32, Logical>) -> (bool, bool) {
    let centre = (
        geometry.loc.x as f64 + geometry.size.w as f64 / 2.0,
        geometry.loc.y as f64 + geometry.size.h as f64 / 2.0,
    );
    (pos.x < centre.0, pos.y < centre.1)
}

/// How `(west, north)` is spelled on the wire.
///
/// A word rather than a pair of signs: the shell reads it, and a command line
/// that says `top-left` is one somebody can also type by hand while working
/// out why a drag went the wrong way.
fn edge_name(edges: (bool, bool)) -> &'static str {
    match edges {
        (true, true) => "top-left",
        (false, true) => "top-right",
        (true, false) => "bottom-left",
        (false, false) => "bottom-right",
    }
}

/// Whether a button event is the shell's.
///
/// `grabbed` is the implicit grab a press over the shell takes: it holds until
/// the release, so a drag that starts on the shell and crosses onto a window
/// stays the shell's, exactly as it would for a client.
///
/// The other way round, it does not. A press over a window is that window's,
/// and where the pointer has wandered to by the time it comes up does not
/// change that — so `over_shell` only counts for a press. Without that, a
/// press on a window released over empty space arrived at the page as a button
/// up with no button down before it.
fn shell_gets_button(grabbed: bool, over_shell: bool, pressed: bool) -> bool {
    grabbed || (pressed && over_shell)
}

/// Whether this event is a key or button coming back up.
///
/// The one thing a blank has to tell apart: `blank` is bound to a chord, and
/// the hand that pressed it has to come off it. Everything else — a press, a
/// moved mouse, a finger — is somebody asking for the screens back, and is
/// answered on the spot rather than after a clock the C build ran
/// (`crate::idle::Idle::activity`).
fn activity_kind<I: InputBackend>(event: &InputEvent<I>) -> crate::idle::Activity {
    let up = match event {
        InputEvent::Keyboard { event, .. } => {
            event.state() == smithay::backend::input::KeyState::Released
        }
        InputEvent::PointerButton { event, .. } => event.state() == ButtonState::Released,
        _ => false,
    };
    if up {
        crate::idle::Activity::Release
    } else {
        crate::idle::Activity::Deliberate
    }
}

/// Whether this event is someone using the pointer.
///
/// What `cursor.hide_after_ms` measures, and the reason it is not simply every
/// input event: typing is what someone does while the mouse sits still, and
/// counting a keystroke would keep the cursor up for exactly the person who
/// asked for it to go away. Touch is out for the other reason — a finger on a
/// touchscreen moves no cursor, so waking one would put an arrow on a screen
/// that had none.
///
/// A tablet tool counts: it draws the cursor like a mouse does, through
/// `tablet_cursor_status`.
fn uses_the_pointer<I: InputBackend>(event: &InputEvent<I>) -> bool {
    matches!(
        event,
        InputEvent::PointerMotion { .. }
            | InputEvent::PointerMotionAbsolute { .. }
            | InputEvent::PointerButton { .. }
            | InputEvent::PointerAxis { .. }
            | InputEvent::GestureSwipeBegin { .. }
            | InputEvent::GestureSwipeUpdate { .. }
            | InputEvent::GestureSwipeEnd { .. }
            | InputEvent::GesturePinchBegin { .. }
            | InputEvent::GesturePinchUpdate { .. }
            | InputEvent::GesturePinchEnd { .. }
            | InputEvent::GestureHoldBegin { .. }
            | InputEvent::GestureHoldEnd { .. }
            | InputEvent::TabletToolAxis { .. }
            | InputEvent::TabletToolProximity { .. }
            | InputEvent::TabletToolTip { .. }
            | InputEvent::TabletToolButton { .. }
    )
}

impl ViewportState {
    /// A key from the control socket rather than from libinput.
    ///
    /// Through `KeyboardHandle::input` and `shortcut` like the real path, so a
    /// scripted chord is filtered by the code a typed one is filtered by — a
    /// test that took its own route would be testing its own route. What it
    /// deliberately does not carry is the rest of `process_input_event`: idle
    /// activity, the screen coming back on, the shell's own key forwarding.
    /// This is for driving the compositor's chords, not for pretending to be a
    /// keyboard.
    pub fn inject_key(&mut self, keycode: u32, pressed: bool) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        let state_bit = if pressed {
            smithay::backend::input::KeyState::Pressed
        } else {
            smithay::backend::input::KeyState::Released
        };
        // evdev codes are offset by 8 from xkb's, which is the difference
        // between what libinput reports and what a keymap is written against.
        let code = smithay::input::keyboard::Keycode::new(keycode + 8);
        let action = keyboard.input::<Option<Action>, _>(
            self,
            code,
            state_bit,
            serial,
            time,
            |_, modifiers, handle| {
                if !pressed {
                    return FilterResult::Forward;
                }
                match shortcut(modifiers, handle.modified_sym()) {
                    Some(action) => FilterResult::Intercept(Some(action)),
                    None => FilterResult::Forward,
                }
            },
        );
        if let Some(action) = action.flatten() {
            self.handle_action(action);
        }
        // A key injected here changes the seat's modifiers exactly as a typed
        // one does — this is the path the on-screen keyboard's Shift takes —
        // and a remote client watching the same seat has to hear about it.
        self.sync_eis_modifiers();
    }

    /// Let go of a key that something else was holding down when it vanished.
    ///
    /// For one caller: a libei client that disconnects mid-chord, see
    /// `crate::libei`. Its presses went through `process_input_event`, so a key
    /// the compositor took for itself is on `suppressed_keys` and a key the
    /// shell was given is held down in the page. Releasing such a key with
    /// `inject_key` would do neither thing — it would forward a release to
    /// whatever has focus now, which never saw the press, and leave the
    /// suppression behind to swallow somebody's real release later.
    ///
    /// So this is the release half of that arm and nothing else: the same
    /// bookkeeping in the same order, with the press half — bindings, the
    /// chooser, the shortcut table — left out because a release cannot start
    /// any of them. What it deliberately does not do is the rest of
    /// `process_input_event`: a client that died is not activity, and waking
    /// the screens for it would light up a desk nobody is at.
    pub fn release_injected_key(&mut self, code: smithay::input::keyboard::Keycode) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        let to_shell = keyboard.current_focus().is_none() && self.shell_is_up();
        // The modifiers are read only by a page, so they are only worth
        // computing when there is one; without the web engine they go
        // nowhere, and the placeholder only keeps the key's shape.
        #[cfg(feature = "wpe")]
        let modifiers_now = self.shell_modifiers();
        #[cfg(not(feature = "wpe"))]
        let modifiers_now = 0;
        let mods_before = keyboard.modifier_state();

        let action = keyboard.input::<Option<Action>, _>(
            self,
            code,
            smithay::backend::input::KeyState::Released,
            serial,
            time,
            |state, _modifiers, handle| {
                let keysym = handle.modified_sym();
                let Some(at) = state.suppressed_keys.iter().position(|k| *k == keysym) else {
                    // Nobody took the press, so the client that has it now is
                    // the client that saw it: an ordinary release.
                    return FilterResult::Forward;
                };
                state.suppressed_keys.remove(at);
                // A key the page was given has to be released to it as well,
                // or the page has one held down for ever.
                if to_shell {
                    FilterResult::Intercept(Some(Action::Web(WebKey {
                        keycode: handle.raw_code().raw() + 8,
                        keysym: keysym.raw(),
                        pressed: false,
                        modifiers: modifiers_now,
                        time: time.millis(),
                    })))
                } else {
                    FilterResult::Intercept(Some(Action::Swallow))
                }
            },
        );

        let intercepted = action.is_some();
        if let Some(action) = action.flatten() {
            self.handle_action(action);
        }
        // For the reason the same two lines in `process_input_event` give: an
        // intercepted key is never forwarded, so a modifier that has just been
        // let go of would otherwise stay depressed as far as the focused client
        // is concerned — and this is the case where the key was let go of by
        // nobody, which is exactly when nothing else will correct it.
        if intercepted && keyboard.modifier_state() != mods_before {
            keyboard.advertise_modifier_state(self);
        }
    }

    /// A pointer motion from the control socket rather than from libinput.
    ///
    /// The same three calls the libinput path makes, in the same order, so a
    /// scripted click and a real one are the same event by the time anything
    /// sees it.
    pub fn inject_pointer(&mut self, x: f64, y: f64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let location = (x, y).into();
        let under = self.surface_under(location);
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location,
                serial,
                time,
            },
        );
        pointer.frame(self);
        self.needs_render = true;
        self.cursor_activity();
    }

    /// A pointer button from the control socket rather than from libinput.
    pub fn inject_button(&mut self, button: u32, pressed: bool) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        if tracing::enabled!(tracing::Level::DEBUG) {
            let focus = pointer.current_focus();
            let shell = self.shell_client_surface().cloned();
            tracing::debug!(
                "button {button} {} -> focus {:?}, shell surface {:?}, same: {}",
                if pressed { "press" } else { "release" },
                focus
                    .as_ref()
                    .map(smithay::reexports::wayland_server::Resource::id),
                shell
                    .as_ref()
                    .map(smithay::reexports::wayland_server::Resource::id),
                focus
                    .as_ref()
                    .map(|f| Some(f) == shell.as_ref())
                    .unwrap_or(false),
            );
        }
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        pointer.button(
            self,
            &smithay::input::pointer::ButtonEvent {
                button,
                state: if pressed {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                },
                serial,
                time,
            },
        );
        pointer.frame(self);
        self.cursor_activity();
    }

    /// A touch from the control socket rather than from libinput.
    ///
    /// Same path as a real finger: hit-test, focus the window under it, then
    /// `wl_touch`. Slot is the finger index the protocol uses, 0 for the
    /// first. Coordinates are the layout's, not a fraction of the output —
    /// a script already thinks in those.
    pub fn inject_touch_down(&mut self, slot: u32, x: f64, y: f64) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let position = (x, y).into();
        let serial = SERIAL_COUNTER.next_serial();
        let under = self.surface_under(position);
        self.focus_touch_target(under.as_ref(), serial);
        touch.down(
            self,
            under,
            &smithay::input::touch::DownEvent {
                slot: inject_touch_slot(slot),
                location: position,
                serial,
                time: InputTime::now(),
            },
        );
        self.needs_render = true;
    }

    pub fn inject_touch_motion(&mut self, slot: u32, x: f64, y: f64) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let position = (x, y).into();
        let under = self.surface_under(position);
        touch.motion(
            self,
            under,
            &smithay::input::touch::MotionEvent {
                slot: inject_touch_slot(slot),
                location: position,
                time: InputTime::now(),
            },
        );
    }

    pub fn inject_touch_up(&mut self, slot: u32) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        touch.up(
            self,
            &smithay::input::touch::UpEvent {
                slot: inject_touch_slot(slot),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
    }

    pub fn inject_touch_frame(&mut self) {
        if let Some(touch) = self.seat.get_touch() {
            touch.frame(self);
        }
    }

    pub fn inject_touch_cancel(&mut self) {
        if let Some(touch) = self.seat.get_touch() {
            touch.cancel(self);
        }
    }

    /// A mouse that moved by this much, rather than one put somewhere.
    ///
    /// The companion to `inject_pointer`, and not a convenience wrapper around
    /// it: a relative movement is its own event on the wire. A client that has
    /// locked the pointer — a game, a virtual machine — is told nothing about
    /// where the cursor is and reads `zwp_relative_pointer_v1` instead, so a
    /// remote session that only ever sent absolute positions could not turn
    /// such a client's camera at all. That is the same reason the libinput
    /// path sends the relative event first and unconditionally, and this
    /// mirrors it deliberately: what a client sees must not depend on whether
    /// the hand on the mouse is in the room.
    ///
    /// Clamped to the monitors, and held inside a confinement region if the
    /// client under the cursor nominated one, exactly as a real mouse is.
    pub fn inject_pointer_relative(&mut self, dx: f64, dy: f64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let from = pointer.current_location();
        let under = self.surface_under(from);
        let (locked, confine_to) = self.pointer_constraint(&pointer, under.as_ref());

        let delta = (dx, dy).into();
        self.note_pointer_motion(from, delta, locked, confine_to.as_ref(), under.is_some());
        let time = InputTime::now();
        pointer.relative_motion(
            self,
            under.clone(),
            &smithay::input::pointer::RelativeMotionEvent {
                delta,
                // Nothing accelerated it — an injected movement is already the
                // distance that was meant — so the raw delta is also the
                // unaccelerated one.
                delta_unaccel: delta,
                time,
            },
        );
        if locked {
            // The cursor does not move, which is the whole of what a lock
            // asks for. The relative event above is what the client reads.
            pointer.frame(self);
            return;
        }

        self.move_pointer(from, delta, confine_to.as_ref(), time);
        self.cursor_activity();
    }

    /// A scroll, smooth or in wheel notches.
    ///
    /// `v120` is what tells the two apart all the way down to the client: a
    /// wheel reports whole detents of 120 and a touchpad reports a distance,
    /// and a client uses the difference to decide whether to animate. Passing
    /// the notch count rather than deriving it from the distance is what keeps
    /// a remote wheel feeling like a wheel — dividing 15 pixels back into
    /// eighths of a detent is arithmetic that never quite comes out.
    ///
    /// `finish` is the touchpad's other half: two fingers leaving the surface
    /// is what stops a client's kinetic scroll, and without it a remote
    /// gesture coasts forever.
    ///
    /// The shell is offered the scroll first when the cursor is over it, for
    /// the same reason the libinput path does: the bar, the notification list
    /// and a chooser longer than the screen all scroll, and they are not
    /// surfaces anything else would route to.
    pub fn inject_axis(
        &mut self,
        horizontal: f64,
        vertical: f64,
        v120: Option<(i32, i32)>,
        finish: bool,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let time = InputTime::now();
        // A wheel when the caller counted notches, a touchpad-like continuous
        // source otherwise. Not `Finger`, which promises that a real finger is
        // on a real touchpad and that a stop event will follow when it lifts;
        // `Continuous` is the source for a scroll that comes from somewhere
        // else, which is exactly what this is.
        let source = if v120.is_some() {
            AxisSource::Wheel
        } else {
            AxisSource::Continuous
        };
        let mut frame = AxisFrame::new(time).source(source);
        if horizontal != 0.0 {
            frame = frame.value(Axis::Horizontal, horizontal);
        }
        if vertical != 0.0 {
            frame = frame.value(Axis::Vertical, vertical);
        }
        if let Some((horizontal_120, vertical_120)) = v120 {
            if horizontal_120 != 0 {
                frame = frame.v120(Axis::Horizontal, horizontal_120);
            }
            if vertical_120 != 0 {
                frame = frame.v120(Axis::Vertical, vertical_120);
            }
        }
        if finish {
            frame = frame.stop(Axis::Horizontal).stop(Axis::Vertical);
        }

        let at = pointer.current_location();
        if self.surface_under(at).is_none() && self.shell_is_up() {
            // The shell reads this as "precise": a scroll worth animating
            // rather than one to step a page at a time. That is a property of
            // being continuous, not of being a finger — a remote touchpad
            // scroll was reaching the page claiming every tick was a detent.
            self.shell_pointer_axis(
                at,
                horizontal,
                vertical,
                source == AxisSource::Continuous,
                time.millis(),
            );
        }
        pointer.axis(self, frame);
        pointer.frame(self);
        self.cursor_activity();
    }

    /// A key named by what it types rather than by where it is.
    ///
    /// An application that has only a character to send — a remote session
    /// relaying what somebody typed on a different keyboard, an on-screen
    /// keyboard whose buttons are letters — has no keycode to give, and the
    /// keycode is what the protocol carries. So one is looked up in the keymap
    /// the seat is actually using, at any shift level of the active layout,
    /// and the key is pressed as if it were that one.
    ///
    /// That is a real approximation and worth naming. Sending the keycode for
    /// `a` while Shift is held types `A`, because the client applies its own
    /// modifiers to the code it is given — this end cannot press a keysym, only
    /// a key. Doing it properly means swapping the keymap for a synthetic one
    /// per character, which is what a virtual keyboard client does and what an
    /// on-screen keyboard should do when it needs a character the layout does
    /// not have.
    ///
    /// Answers with whether the keymap had one, so a caller can say so rather
    /// than leaving a key that silently did nothing.
    pub fn inject_keysym(&mut self, keysym: u32, pressed: bool) -> bool {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return false;
        };
        let keysym = smithay::input::keyboard::Keysym::new(keysym);
        let Some(keycode) = keyboard.keycode_for_keysym(keysym) else {
            tracing::debug!(
                "no key in this layout produces {}",
                smithay::input::keyboard::xkb::keysym_get_name(keysym)
            );
            return false;
        };
        // Back to evdev's numbering, which is what `inject_key` takes and what
        // libinput reports: xkb's keycodes are offset by eight from it, and
        // this is the one place the offset has to be undone rather than
        // applied.
        self.inject_key(keycode.raw().saturating_sub(8), pressed);
        true
    }

    // Why the on-screen keyboard types by pressing keys rather than by
    // committing text.
    //
    // `zwp_text_input_v3` plus `zwp_input_method_v2` is the protocol built
    // for exactly this — a client asks for text input, something on the
    // compositor's side supplies characters, and `commit_string` hands them
    // over as real text rather than as a simulated keystroke. This
    // compositor even has the escape hatch a fork of smithay added for it:
    // `TextInputHandle::set_compositor_input_method`, switched on
    // permanently in `state.rs`, makes the seat's text-input machinery run
    // with no real IME bound at all.
    //
    // It is not used to type. Two things ruled it out, and both are about
    // coverage rather than difficulty. First, `commit_string` only reaches a
    // client that has bound `zwp_text_input_v3` *and* called `enable` on it
    // — every toolkit does for a real text field, but a terminal emulator
    // does not, and a login prompt or a game's own text box often does not
    // either, so a keyboard that only worked through that path would go
    // silent on exactly the applications a touch-only desk most needs it
    // for. Second, `commit_string` only carries literal text: Backspace,
    // Enter, Tab and the arrows are not characters, and an application
    // reading them as one rather than as the editing command they are would
    // be a keyboard that types "backspace" instead of erasing.
    //
    // `inject_keysym` has neither limit — it presses a key, which is what
    // every client with a keyboard already knows what to do with — and its
    // own doc comment already named this as the intended caller before this
    // keyboard existed. So `osk.key` is handed straight to it in `apply.rs`,
    // and the input-method escape hatch stays switched on for exactly one
    // job: letting `sync_osk_wanted`, below, see a text-input's `enable` at
    // all. See that request's doc comment in `viewport-ipc` for how Shift
    // and Caps Lock are made to work over a path that can only press keys.
    //
    /// Tell the shell whether the client with keyboard focus currently has an
    /// enabled text-input, so an on-screen keyboard can raise and lower
    /// itself without a binding to press.
    ///
    /// `zwp_text_input_v3`'s `enable`/`disable` are the client's own way of
    /// asking, but this smithay fork has no callback for them — a real
    /// `zwp_input_method_v2` client is expected to notice by receiving
    /// `activate`/`deactivate`, and this compositor is not one. What it has
    /// instead is `TextInputHandle::with_active_text_input`, which is
    /// accurate the instant it is asked but says nothing on its own, so this
    /// is called from the two places already in this file that run close to
    /// "the focused client just told the compositor something": every
    /// `wl_surface.commit`, because `enable` only takes effect on the
    /// text-input's own commit and a client asking for a keyboard tends to
    /// repaint soon after — a cursor starting to blink, if nothing else — and
    /// every keyboard focus change, which catches a field that was already
    /// enabled before its window had focus. Between the two, the delay a
    /// person actually experiences is not the kind anybody notices; a
    /// dedicated callback would close the small remaining gap and was not
    /// worth adding to a vendored fork for it.
    ///
    /// Notifies only on the edge — see `osk_wanted`'s own doc comment in
    /// `state.rs` for why sending on every check would be worse than the gap
    /// this leaves.
    ///
    /// What "wanted" means is gated by `osk_mode`, config's `osk` key. Under
    /// `OskMode::Manual` and `OskMode::Off` a text-input enabling is never
    /// enough on its own — the keyboard only comes up if somebody reaches
    /// for the chord, which this function has no part in — so `wanted` is
    /// forced false before it ever reaches `osk_wanted` or a notification.
    /// Under `OskMode::Auto` it is further gated on `osk_touch_seen`: raising
    /// the keyboard for every enabled text-input regardless of hardware is
    /// right for a tablet, where the desk has no other way to type, and
    /// wrong for the desk that has a keyboard and a mouse and nothing
    /// touch-capable at all, where it is only ever in the way. See that
    /// field's own doc comment in `state.rs` for why it is sticky rather than
    /// a live reading of the seat.
    pub fn sync_osk_wanted(&mut self) {
        use smithay::wayland::text_input::TextInputSeat as _;

        let mut wanted = false;
        self.seat
            .text_input()
            .with_active_text_input(|_text_input, _surface| wanted = true);

        wanted = match self.osk_mode {
            crate::config::OskMode::Auto => wanted && self.osk_touch_seen,
            crate::config::OskMode::Manual | crate::config::OskMode::Off => false,
        };

        if wanted == self.osk_wanted {
            return;
        }
        self.osk_wanted = wanted;
        self.notify(&viewport_ipc::Event::OskWanted { wanted });
    }

    /// One input event from a remote-desktop session.
    ///
    /// The dispatcher, and nothing more: every arm is a call to one of the
    /// helpers above, which are the same ones the control socket drives and
    /// the same ones an on-screen keyboard will. Whether this session was
    /// allowed to send the event was settled on the bus thread against the
    /// grant the user gave — see `screencast::remote` — so what is left here
    /// is only where on the desk it lands.
    pub fn inject_remote(&mut self, injection: crate::screencast::remote::Injection) {
        use crate::screencast::remote::Injection;

        match injection {
            Injection::PointerMotion { dx, dy } => self.inject_pointer_relative(dx, dy),
            Injection::PointerMotionAbsolute { stream, x, y } => {
                // Nothing at all when the node names no stream, rather than a
                // click at the origin: the application is pointing at a
                // picture this compositor is not sending, and the top left
                // corner of the desk is not a better guess than none.
                match self.remote_point(stream, x, y) {
                    Some(at) => self.inject_pointer(at.x, at.y),
                    None => tracing::debug!(
                        "remote desktop: a pointer position in stream {stream}, which is not one \
                         of ours"
                    ),
                }
            }
            Injection::PointerButton { button, pressed } => self.inject_button(button, pressed),
            Injection::PointerAxis { dx, dy, finish } => self.inject_axis(dx, dy, None, finish),
            Injection::PointerAxisDiscrete { axis, steps } => {
                let (horizontal, vertical) = crate::screencast::remote::discrete_axis(axis, steps);
                let notches = crate::screencast::remote::discrete_v120(steps);
                let v120 = match (horizontal != 0.0, vertical != 0.0) {
                    (true, _) => (notches, 0),
                    (_, true) => (0, notches),
                    // An axis this end does not know, which `discrete_axis`
                    // has already turned into no movement. Dropped rather than
                    // sent as an empty frame.
                    _ => return,
                };
                self.inject_axis(horizontal, vertical, Some(v120), false);
            }
            Injection::KeyboardKeycode { keycode, pressed } => match u32::try_from(keycode) {
                Ok(keycode) => self.inject_key(keycode, pressed),
                Err(_) => tracing::debug!("remote desktop: ignoring keycode {keycode}"),
            },
            Injection::KeyboardKeysym { keysym, pressed } => match u32::try_from(keysym) {
                Ok(keysym) => {
                    self.inject_keysym(keysym, pressed);
                }
                Err(_) => tracing::debug!("remote desktop: ignoring keysym {keysym}"),
            },
            Injection::TouchDown { stream, slot, x, y } => {
                if let Some(at) = self.remote_point(stream, x, y) {
                    self.inject_touch_down(slot, at.x, at.y);
                    // One finger per event on this interface, so each one is
                    // its own frame. A client that never receives a frame
                    // never acts on the touch at all.
                    self.inject_touch_frame();
                }
            }
            Injection::TouchMotion { stream, slot, x, y } => {
                if let Some(at) = self.remote_point(stream, x, y) {
                    self.inject_touch_motion(slot, at.x, at.y);
                    self.inject_touch_frame();
                }
            }
            Injection::TouchUp { slot } => {
                self.inject_touch_up(slot);
                self.inject_touch_frame();
            }
        }
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // Anything the shell asked for and has not been given yet, before this
        // event is tested against the desktop.
        //
        // `surface_under` picks what a click lands on out of the space, in the
        // space's order, so a stack owed a restack here is a click going
        // through a dialog into the window behind it — which is the fault
        // `restack` exists to prevent, arriving by a different door. The IPC
        // source pays up before it returns, so this cannot actually be owed
        // anything; it is here because "cannot" rests on calloop's ordering and
        // a click landing in the wrong window is not a thing to leave resting
        // on that.
        self.settle();

        // A hotplug is not somebody using the desk. These events still reach
        // the match below for tablet, touch and device-config lifecycle.
        let device_change = matches!(
            &event,
            InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
        );
        if !device_change && self.idle.activity(activity_kind(&event)) {
            // The screens were off. Bring them back through the same path the
            // deadline turned them off by.
            self.set_outputs_enabled(true);
        }
        // And any client that asked to be told when the session goes idle —
        // a chat program marking you away, which is not the compositor's
        // business to decide but is its business to report.
        if !device_change {
            let seat = self.seat.clone();
            self.idle_notifier_state.notify_activity(&seat);
        }

        // And the pointer's own deadline, which counts a narrower set of
        // events than either of those — see `uses_the_pointer`.
        if uses_the_pointer(&event) {
            self.cursor_activity();
        }

        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time(&event);
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };
                let pressed = event.state() == smithay::backend::input::KeyState::Pressed;
                // Worked out before the keyboard's state is borrowed by the
                // filter, which is where it is needed.
                let inhibited = self.shortcuts_inhibited();
                // Nothing holds the keyboard, so the keys are the shell's.
                //
                // By who actually holds focus rather than by whether a window
                // is focused: a layer surface — a launcher, a lock screen —
                // takes focus without being a toplevel, and checking the
                // focused window alone would deliver its keystrokes to the
                // page instead, leaving the launcher unable to type
                // (`src/input.c:1052`).
                let to_shell = keyboard.current_focus().is_none() && self.shell_is_up();
                // The modifiers are read only by a page, so they are only worth
                // computing when there is one; without the web engine they go
                // nowhere, and the placeholder only keeps the key's shape.
                #[cfg(feature = "wpe")]
                let modifiers_now = self.shell_modifiers();
                #[cfg(not(feature = "wpe"))]
                let modifiers_now = 0;

                // What the focused client currently believes is held. Compared
                // against the state after the event, below.
                let mods_before = keyboard.modifier_state();

                // The filter runs with the keyboard's state borrowed, so it
                // decides *what* to do and the action is carried out after.
                let action = keyboard.input::<Option<Action>, _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |state, modifiers, handle| {
                        let keysym = handle.modified_sym();
                        if pressed {
                            // The compositor's own chords first: those have to
                            // work even when a binding table is broken.
                            // A client holding an inhibitor gets everything
                            // except a VT switch. That one is the session's
                            // rather than this seat's — a client cannot be
                            // allowed to take the only way off a frozen
                            // desktop, and no client inside a VT has any use
                            // for the chord that leaves it.
                            if inhibited {
                                match shortcut(modifiers, keysym) {
                                    Some(action @ Action::SwitchVt(_)) => {
                                        state.suppressed_keys.push(keysym);
                                        return FilterResult::Intercept(Some(action));
                                    }
                                    _ => return FilterResult::Forward,
                                }
                            }
                            // The chooser owns the keyboard while it is up.
                            //
                            // Everything is taken, not only the keys it uses: a
                            // keystroke that fell through would go to whatever
                            // was focused before the share was asked for, which
                            // is a password typed into a terminal the user is
                            // not looking at.
                            if state.picker.is_some() {
                                state.suppressed_keys.push(keysym);
                                let pick = match keysym {
                                    Keysym::Escape => Some(Pick::Cancel),
                                    Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                                        Some(Pick::Confirm)
                                    }
                                    Keysym::Up | Keysym::k => Some(Pick::Step(-1)),
                                    Keysym::Down | Keysym::j | Keysym::Tab => Some(Pick::Step(1)),
                                    _ => None,
                                };
                                return FilterResult::Intercept(
                                    pick.map(Action::Pick).or(Some(Action::Swallow)),
                                );
                            }

                            if let Some(action) = shortcut(modifiers, keysym) {
                                // VT switching still works while locked — it
                                // is the session's, not this seat's, and a
                                // locked session on another VT is still
                                // locked. Quitting does not: it would drop the
                                // lock along with the compositor. Forwarded
                                // rather than swallowed, so Ctrl+Alt+Backspace
                                // still reaches the lock screen as keys.
                                if state.locked && !matches!(action, Action::SwitchVt(_)) {
                                    return FilterResult::Forward;
                                }
                                // Remembered so the release is swallowed too;
                                // a client that saw only the release would
                                // think the key was stuck.
                                state.suppressed_keys.push(keysym);
                                return FilterResult::Intercept(Some(action));
                            }
                            // The *unmodified* keysym. A chord is written
                            // "Mod4+Shift+q" — the shift is in the modifiers,
                            // and the key is still q. Matching the modified
                            // symbol would look for Q and never find it, so
                            // every shifted binding would be dead.
                            let unmodified = handle
                                .raw_latin_sym_or_raw_current_sym()
                                .map(|sym| sym.raw())
                                .unwrap_or_else(|| keysym.raw());

                            // No binding fires while locked — one that
                            // spawns a terminal would put it on top of the
                            // lock screen — but the key still goes to the
                            // client, because the client is the lock screen
                            // and the key is the password.
                            if state.locked {
                                return FilterResult::Forward;
                            }

                            // A global shortcut, but only after everything
                            // above has declined the key. An application that
                            // asks for a chord this desktop is already using
                            // gets a grant that never fires, rather than a
                            // terminal that stops opening — the desk's own
                            // keymap is not something a portal call may
                            // change. See `crate::shortcuts`.
                            if let Some(fired) = state.shortcut_for(modifiers, unmodified) {
                                state.suppressed_keys.push(keysym);
                                state.shortcuts_to_announce.push((true, fired.clone()));
                                // Remembered by the key it arrived on, because
                                // the release is matched by keysym and carries
                                // nothing else to identify it — and by then the
                                // modifiers may already be up.
                                state.shortcuts_held.push((keysym.raw(), fired));
                                return FilterResult::Intercept(Some(Action::Swallow));
                            }

                            match crate::binding::match_binding(
                                &state.bindings,
                                modifiers,
                                unmodified,
                                &state.binding_mode,
                            ) {
                                Some(bound) => {
                                    state.suppressed_keys.push(keysym);
                                    FilterResult::Intercept(Some(Action::Bound(bound.clone())))
                                }
                                // To the page, which is the only thing left
                                // that could want it. Intercepted rather than
                                // forwarded because forwarding it goes
                                // nowhere: there is no focused surface, which
                                // is why it is the shell's in the first place.
                                None if to_shell => {
                                    state.suppressed_keys.push(keysym);
                                    FilterResult::Intercept(Some(Action::Web(WebKey {
                                        keycode: handle.raw_code().raw() + 8,
                                        keysym: keysym.raw(),
                                        pressed: true,
                                        modifiers: modifiers_now,
                                        time: time.millis(),
                                    })))
                                }
                                None => FilterResult::Forward,
                            }
                        } else if let Some(at) =
                            state.suppressed_keys.iter().position(|k| *k == keysym)
                        {
                            state.suppressed_keys.remove(at);
                            // The other half of a global shortcut. A
                            // push-to-talk key is the case that makes this
                            // more than tidiness: the application is holding a
                            // microphone open on the strength of the press,
                            // and nothing else will ever tell it the key came
                            // back up.
                            if let Some(at) = state
                                .shortcuts_held
                                .iter()
                                .position(|(code, _)| *code == keysym.raw())
                            {
                                let (_, fired) = state.shortcuts_held.remove(at);
                                state.shortcuts_to_announce.push((false, fired));
                            }
                            // A key the page was given has to be released to
                            // it as well, or the page has one held down for
                            // ever.
                            if to_shell {
                                FilterResult::Intercept(Some(Action::Web(WebKey {
                                    keycode: handle.raw_code().raw() + 8,
                                    keysym: keysym.raw(),
                                    pressed: false,
                                    modifiers: modifiers_now,
                                    time: time.millis(),
                                })))
                            } else {
                                FilterResult::Intercept(Some(Action::Swallow))
                            }
                        } else {
                            FilterResult::Forward
                        }
                    },
                );

                let intercepted = action.is_some();
                if let Some(action) = action.flatten() {
                    self.handle_action(action);
                }

                // Tell the focused client about a modifier it did not see
                // change.
                //
                // Smithay sends the modifier update as part of forwarding a
                // key, and an intercepted key is never forwarded — so a
                // modifier the compositor took for itself changes state
                // silently as far as the client is concerned. Mod4 is one:
                // it goes to the shell so the bar can appear while it is held,
                // which means both its press and its release are intercepted.
                //
                // What that produced: `Mod4+Return` opens a terminal, the
                // terminal takes focus while the key is still physically down,
                // and its `enter` correctly reports Mod4 depressed. Releasing
                // Mod4 was then intercepted and the terminal was never told —
                // so it read every following key as Mod4+key. Arrows did
                // nothing, and letters looked fine because by the time anyone
                // typed one something unrelated had pushed a fresh modifier
                // state out. It cleared as soon as focus moved away and back,
                // which is why opening a second window "fixed" it.
                if intercepted && keyboard.modifier_state() != mods_before {
                    keyboard.advertise_modifier_state(self);
                }

                // Anything the filter noticed a global shortcut on, announced
                // now that the keyboard's own borrow has ended.
                self.flush_shortcuts();

                // And any libei client, for the same reason from further away:
                // a remote session composes its own chords and cannot see the
                // desk. See `sync_eis_modifiers`. Not conditional on
                // `intercepted` — a modifier the compositor forwarded reached
                // the focused client and still never reached the socket.
                self.sync_eis_modifiers();

                // Whether the logo key is held, which is what the bar rides on
                // when it is set to appear only while Mod4 is down.
                //
                // After the key has been processed, or the state read is the
                // one before it. Only on a change, and only while the bar is
                // on "auto": this runs for every Shift and Ctrl of ordinary
                // typing, and a message per keystroke saying Mod4 is still not
                // held would be pure noise (`src/input.c:922`).
                if self.config.bar.as_deref() == Some("auto") {
                    let logo = self
                        .seat
                        .get_keyboard()
                        .map(|keyboard| keyboard.modifier_state().logo)
                        .unwrap_or(false);
                    if logo != self.logo_held {
                        self.logo_held = logo;
                        self.notify(&viewport_ipc::Event::Modifiers { logo });
                    }
                }
            }

            // A mouse sends relative motion; absolute is for tablets and
            // touchscreens. Handling only the latter leaves the pointer
            // pinned at the origin for the whole session.
            InputEvent::PointerMotion { event, .. } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let from = pointer.current_location();
                let under = self.surface_under(from);

                // What the client under the pointer has asked for, if
                // anything. Read before the pointer moves, because a
                // constraint applies to where it is now.
                let (locked, confine_to) = self.pointer_constraint(&pointer, under.as_ref());
                self.note_pointer_motion(
                    from,
                    event.delta(),
                    locked,
                    confine_to.as_ref(),
                    under.is_some(),
                );

                // Relative motion first, and always. It is what a game reads,
                // and a locked pointer has nothing else to go on — an absolute
                // position saturates at the screen edge, which is a game that
                // can only turn so far.
                pointer.relative_motion(
                    self,
                    under.clone(),
                    &smithay::input::pointer::RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        time: event.time(),
                    },
                );

                if locked {
                    // The cursor does not move at all. That is the point: it
                    // neither escapes onto the other monitor mid-fight nor
                    // generates absolute motion the game would misread.
                    pointer.frame(self);
                    return;
                }

                self.move_pointer(from, event.delta(), confine_to.as_ref(), event.time());
            }

            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                // A tablet in absolute mode names a place on the glass, and
                // under the magnifier the glass is showing a blown-up piece of
                // the layout. A relative mouse does not come through here and
                // must not be put through this — see
                // `ViewportState::glass_to_content`.
                let pos = self.glass_to_content(pos);
                self.pointer_absolute_to(pos, event.time());
            }

            InputEvent::PointerButton { event, .. } => {
                let (Some(pointer), Some(keyboard)) =
                    (self.seat.get_pointer(), self.seat.get_keyboard())
                else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let state = event.state();

                // An xdg-shell move/resize replaced the client's implicit
                // click grab with our own. That grab owns every button until
                // the initiating one is released.
                if self
                    .pointer_drag
                    .as_ref()
                    .is_some_and(|drag| drag.client_requested)
                {
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button: event.button_code(),
                            state,
                            serial,
                            time: event.time(),
                        },
                    );
                    pointer.frame(self);
                    return;
                }

                // Mod4 and a button drags the window under the pointer: left
                // moves it, right resizes it. The chord every tiling
                // compositor has, and what makes a floating window usable
                // without a titlebar to grab — this compositor draws none,
                // because the shell draws the frame.
                //
                // The shell has done the arithmetic for both since it was
                // written, and nothing ever sent it: `layout.move.delta` and
                // `layout.resize.delta` had a handler, a comment saying the
                // compositor forwards the drag, and no sender anywhere.
                //
                // While one is running the buttons are its own, so that is
                // settled first.
                match drag_effect(
                    self.pointer_drag.as_ref().map(|drag| drag.button),
                    event.button_code(),
                    state == ButtonState::Pressed,
                ) {
                    DragEffect::End => {
                        self.finish_pointer_drag();
                        // The hand has let go, and the shell has been drawing
                        // without its animations for as long as the deltas
                        // were arriving — a window under the pointer must not
                        // ease toward it. Saying so is what puts the desktop
                        // back under the ordinary rules on the next frame
                        // rather than a timeout later. The shell has that
                        // timeout as well, for the gesture that ends without a
                        // release: a VT switch takes the pointer away and no
                        // button is ever reported up.
                        return;
                    }
                    DragEffect::Swallow => return,
                    DragEffect::Free => {}
                }

                // A configured mouse binding, fired on the press like a key
                // binding is. The modifier prefix is the keyboard's, exactly
                // as in a chord: `Mod4+Mouse4=shell workspace.switch 1` needs
                // Mod4 held. Runs before the drag line so a bound button is
                // the user's gesture, not the window's.
                if state == ButtonState::Pressed {
                    if let Some(bound) = crate::binding::match_button(
                        &self.bindings,
                        &keyboard.modifier_state(),
                        event.button_code(),
                        &self.binding_mode,
                    ) {
                        self.handle_action(Action::Bound(bound.clone()));
                        // Not forwarded: the button was bound, and handing a
                        // press it did not ask for to a client would leave it
                        // thinking the button is still down.
                        suppress_button(event.button_code());
                        return;
                    }
                } else if release_suppressed(event.button_code()) {
                    // The other half of the same chord. Matching again would
                    // answer the wrong question — the modifier is usually let
                    // go of before the button is — so the press records what it
                    // took and the release goes by that, exactly as
                    // `suppressed_keys` does for a key. Without it the client
                    // was handed a release for a press it never saw.
                    return;
                }

                // Something the shell drew in front and asked to be clicked:
                // the bar it floats over the windows, a notification, the
                // screen-share chooser.
                //
                // Nothing here is a handle for a gesture — not for dragging
                // the window underneath, which is what the old guard said, and
                // not for panning the desktop either, which is what it missed.
                // With no window under the pointer a Mod4 press on the bar
                // started a *pan*, was swallowed by it, and never reached the
                // page: in `bar: auto` the bar is on screen only while Mod4 is
                // held, so every click it could ever receive arrived with that
                // modifier down and was taken for a gesture. What that looks
                // like is a bar whose workspace pills and window titles do
                // nothing at all.
                let on_overlay = crate::pointer::over_overlay(
                    &self.shell_overlay_hits,
                    pointer.current_location(),
                );

                // Nothing is dragged, moved, resized or panned behind a lock
                // screen. `window_under` answers by geometry alone — unlike
                // `surface_under`, which refuses while locked — so the guard
                // has to be here, in front of every path that hit-tests.
                if !self.locked
                    && starts_gesture(
                        state == ButtonState::Pressed,
                        pointer.is_grabbed(),
                        on_overlay,
                        keyboard.modifier_state().logo,
                        event.button_code(),
                    )
                {
                    let hit = self.window_under(pointer.current_location());
                    let dragging = hit.as_ref().and_then(|window| {
                        self.views
                            .iter()
                            .find(|v| &v.window == window)
                            .map(|v| v.id)
                    });
                    // Nothing under the pointer is the desktop itself, and a
                    // drag there moves the *view*: the gesture every canvas
                    // has, and the one thing a plane with no edges cannot do
                    // without. The shell decides what that means and a layout
                    // with no view of its own ignores it, so this stays a
                    // question about whether a window was hit rather than
                    // about which layout is running — which the compositor
                    // would be wrong about anyway the moment `layout.model`
                    // switched one at runtime.
                    //
                    // Left only. The right button on the desktop is not a
                    // resize of anything.
                    let kind = match (dragging, event.button_code()) {
                        (Some(_), BTN_RIGHT) => crate::state::DragKind::Resize,
                        (Some(_), _) => crate::state::DragKind::Move,
                        (None, BTN_LEFT) => crate::state::DragKind::Pan,
                        (None, _) => crate::state::DragKind::Move,
                    };
                    if dragging.is_some() || kind == crate::state::DragKind::Pan {
                        // Which corner the resize took hold of, worked out once
                        // at the press: a drag that asked again every frame
                        // would change edges under the hand the moment the
                        // pointer crossed the middle of the window it is
                        // resizing.
                        //
                        // In the window's own coordinates, as `window_under`
                        // tests: a window drawn scaled — a canvas at any zoom
                        // but 1 — is at a different place on screen than the
                        // `Space` holds it.
                        let edges = hit
                            .as_ref()
                            .filter(|_| kind == crate::state::DragKind::Resize)
                            .and_then(|window| {
                                let geometry = self.space.element_geometry(window)?;
                                let pos = self.unscaled(window, pointer.current_location());
                                Some(resize_edges(pos, geometry))
                            })
                            .unwrap_or((false, false));
                        self.pointer_drag = Some(crate::state::PointerDrag {
                            id: dragging.unwrap_or_default(),
                            button: event.button_code(),
                            kind,
                            edges,
                            edge: None,
                            last: pointer.current_location(),
                            pending: (0.0, 0.0),
                            sent: None,
                            client_requested: false,
                        });
                        // Not forwarded. The client did not ask to be dragged
                        // and a button it sees pressed and never released is a
                        // button it thinks is still down.
                        return;
                    }
                }

                // And nothing takes the keyboard behind a lock screen either.
                // The locker's surface is the only thing that may hold it, and
                // a click where a window happens to be mapped raised that
                // window and handed it every keystroke that followed.
                if state == ButtonState::Pressed && !pointer.is_grabbed() && !self.locked {
                    let hit = self.window_under(pointer.current_location());
                    // Clicking a notification must not raise and focus the
                    // window behind it — the click never reached that window.
                    // `on_overlay` is worked out above, where the gesture is
                    // declined for the same reason.
                    match hit.filter(|_| !self.overview && !on_overlay) {
                        Some(window) => {
                            let id = self
                                .views
                                .iter()
                                .find(|v| v.window == window)
                                .map(|v| v.id)
                                .unwrap_or(NO_VIEW);

                            self.space.raise_element(&window, false);
                            // A click on a tiled window must not bury the
                            // floating one that was over it.
                            self.restack();
                            // By window rather than by toplevel: an X11
                            // window has no toplevel, and reaching for one
                            // focused nothing at all.
                            if let Some(focus) =
                                crate::keyboard_focus::KeyboardFocus::for_window(&window)
                            {
                                keyboard.set_focus(self, Some(focus), serial);
                            }
                            self.activate_view(id);
                            if id != self.focused {
                                self.notify_focus(id);
                            }
                        }
                        None => {
                            // The pointer is over the shell, or the overview is
                            // up and every click belongs to it.
                            self.activate_view(NO_VIEW);
                            // To the page that was clicked, where the shell is
                            // a client — a click on a web page is how anyone
                            // expects to start typing into it. With the engine
                            // in this process there is no surface to focus and
                            // the focus has to stay empty, which is what tells
                            // the key path to hand keys to the engine instead.
                            let where_ = pointer.current_location();
                            if !self.focus_shell_at(Some(where_)) {
                                keyboard.set_focus(
                                    self,
                                    Option::<crate::keyboard_focus::KeyboardFocus>::None,
                                    serial,
                                );
                            }
                            if self.focused != NO_VIEW {
                                self.notify_focus(NO_VIEW);
                            }
                        }
                    }
                }

                // To the shell, when that is where the pointer is. A press
                // holds it until release, so a drag that crosses onto a window
                // is still the shell's — the same implicit grab Wayland gives
                // a client.
                //
                // And a release goes by the grab alone. Where the pointer has
                // got to says nothing about who the button belongs to: press
                // on a window, drag off it onto the shell, let go, and the
                // page was handed a button up for a button it never saw go
                // down.
                let pressed = state == ButtonState::Pressed;
                let on_shell = shell_gets_button(
                    self.pointer_grabbed_by_shell,
                    self.surface_under(pointer.current_location()).is_none(),
                    pressed,
                );
                if on_shell && self.shell_is_up() {
                    if pressed {
                        self.pointer_grabbed_by_shell = true;
                    }
                    let at = pointer.current_location();
                    self.shell_pointer_button(
                        at,
                        event.button_code(),
                        pressed,
                        event.time().millis(),
                    );
                }
                if !pressed {
                    self.pointer_grabbed_by_shell = false;
                }

                pointer.button(
                    self,
                    &ButtonEvent {
                        button: event.button_code(),
                        state,
                        serial,
                        time: event.time(),
                    },
                );
                pointer.frame(self);
            }

            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                let horizontal = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0
                });
                let vertical = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0
                });

                // A scroll binding — `Mod4+WheelUp=shell workspace.next`. Only a
                // physical wheel, not a touchpad: two-finger scroll is the
                // page's to answer, and a binding would fire on every tick of a
                // swipe. Fires once per wheel notch (axis batch) while the
                // modifier is held, and consumes the scroll — nothing is passed
                // on to the window under the pointer.
                if source != AxisSource::Finger && vertical != 0.0 {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        let wheel = if vertical > 0.0 {
                            crate::binding::Wheel::Up
                        } else {
                            crate::binding::Wheel::Down
                        };
                        if let Some(bound) = crate::binding::match_wheel(
                            &self.bindings,
                            &keyboard.modifier_state(),
                            wheel,
                            &self.binding_mode,
                        ) {
                            self.handle_action(Action::Bound(bound.clone()));
                            return;
                        }
                    }
                }

                let mut frame = AxisFrame::new(event.time()).source(source);
                if horizontal != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal);
                    if let Some(discrete) = event.amount_v120(Axis::Horizontal) {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical);
                    if let Some(discrete) = event.amount_v120(Axis::Vertical) {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }
                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                // Scrolling the shell: the taskbar, the notification list, and
                // a chooser longer than the screen.
                let at = pointer.current_location();
                if self.surface_under(at).is_none() && self.shell_is_up() {
                    self.shell_pointer_axis(
                        at,
                        horizontal,
                        vertical,
                        source == AxisSource::Finger,
                        event.time().millis(),
                    );
                }
                pointer.axis(self, frame);
                pointer.frame(self);
            }

            // A device appearing. A tablet has to be added to the seat before
            // a client can be told anything about it, and libinput is the only
            // thing that knows one is there.
            InputEvent::DeviceAdded { device } => {
                use smithay::backend::input::Device as _;
                if device.has_capability(smithay::backend::input::DeviceCapability::TabletTool) {
                    let descriptor = TabletDescriptor::from(&device);
                    let dh = self.display_handle.clone();
                    self.seat.tablet_seat().add_wp_tablet(&dh, &descriptor);
                    tracing::info!("tablet: {}", descriptor.name);
                }
                // Sticky rather than toggled back off in DeviceRemoved — see
                // `osk_touch_seen`'s own doc comment in state.rs for why a
                // touchscreen unplugging mid-session should not be the thing
                // that stops the on-screen keyboard raising itself.
                if device.has_capability(smithay::backend::input::DeviceCapability::Touch)
                    && !self.osk_touch_seen
                {
                    self.osk_touch_seen = true;
                    // Recomputed immediately rather than waiting for the next
                    // commit or focus change: a touchscreen appearing after
                    // login (a USB panel plugged in, a Bluetooth one paired)
                    // should be able to raise the keyboard for whatever
                    // already has an enabled text-input, not only for the
                    // next thing focused after it.
                    self.sync_osk_wanted();
                }
            }
            InputEvent::DeviceRemoved { device } => {
                use smithay::backend::input::Device as _;
                if device.has_capability(smithay::backend::input::DeviceCapability::TabletTool) {
                    let seat = self.seat.tablet_seat();
                    seat.remove_tablet(&TabletDescriptor::from(&device));
                    // The tools belong to the tablets. With none left there is
                    // nothing for a tool to be on, and a client holding one
                    // would be told about pressure from a device that is in a
                    // drawer.
                    if seat.count_tablets() == 0 {
                        seat.clear_tools();
                    }
                }
            }

            // A tablet pen moving. It carries what a mouse cannot: pressure,
            // tilt, and which end of the pen is down — which is the whole
            // reason a drawing program wants the protocol rather than the
            // pointer.
            InputEvent::TabletToolAxis { event, .. } => {
                use smithay::backend::input::TabletToolEvent as _;
                let Some(position) = self.touch_position(&event) else {
                    return;
                };
                let under = self.surface_under(position);
                let time = event.time();
                let tool = self.seat.tablet_seat().get_tool(&event.tool());

                // The pointer moves too: a tablet is also how the cursor gets
                // around, and a client with no tablet support still sees it.
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.motion(
                        self,
                        under.clone(),
                        &MotionEvent {
                            location: position,
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                        },
                    );
                    pointer.frame(self);
                }

                if let Some(tool) = tool {
                    tool.axis(self, axis_frame(&event));
                    tool.motion(
                        self,
                        under,
                        &smithay::input::tablet::tool::MotionEvent {
                            location: position,
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                        },
                    );
                    tool.frame(self, time);
                }
                self.needs_render = true;
            }

            // The pen coming within range of the tablet, or leaving it.
            InputEvent::TabletToolProximity { event, .. } => {
                use smithay::backend::input::{
                    TabletToolEvent as _, TabletToolProximityEvent as _,
                };
                let Some(position) = self.touch_position(&event) else {
                    return;
                };
                let under = self.surface_under(position);
                let time = event.time();
                let dh = self.display_handle.clone();
                let seat = self.seat.tablet_seat();
                let tablet = seat.get_tablet(&TabletDescriptor::from(&event.device()));
                let tool = seat.get_tool(&event.tool());
                let tool = match tool {
                    Some(tool) => tool,
                    // First sight of this pen. A tablet reports its tools as
                    // they arrive rather than up front, because a pen is not
                    // plugged in.
                    None => self
                        .seat
                        .tablet_seat()
                        .add_wp_tool(self, &dh, &event.tool()),
                };
                let Some(tablet) = tablet else {
                    return;
                };

                match event.state() {
                    smithay::backend::input::ProximityState::In => tool.proximity_in(
                        self,
                        under,
                        tablet,
                        &smithay::input::tablet::tool::ProximityInEvent {
                            location: position,
                            axis: Some(axis_frame(&event)),
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                        },
                    ),
                    smithay::backend::input::ProximityState::Out => {
                        // The pen has been lifted away, so whatever it asked
                        // the cursor to look like is no longer the answer:
                        // the mouse is the device in use again and has its own
                        // idea. Left set, a crosshair would follow a pointer
                        // that has moved somewhere else entirely.
                        self.tablet_cursor_status = None;
                        self.needs_render = true;
                        tool.proximity_out(
                            self,
                            &smithay::input::tablet::tool::ProximityOutEvent {
                                serial: SERIAL_COUNTER.next_serial(),
                                time,
                            },
                        )
                    }
                }
                tool.frame(self, time);
            }

            // The pen touching the tablet, which is a click that also takes
            // the keyboard: there is nothing else to focus with.
            InputEvent::TabletToolTip { event, .. } => {
                use smithay::backend::input::{TabletToolEvent as _, TabletToolTipEvent as _};
                let Some(tool) = self.seat.tablet_seat().get_tool(&event.tool()) else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let time = event.time();
                match event.tip_state() {
                    smithay::backend::input::TabletToolTipState::Down => {
                        tool.down(
                            self,
                            &smithay::input::tablet::tool::DownEvent { serial, time },
                        );
                        // The tip landing is the click; the keyboard comes
                        // with it, by the same route a finger's does.
                        let at = self
                            .seat
                            .get_pointer()
                            .map(|pointer| pointer.current_location());
                        if let Some(at) = at {
                            let under = self.surface_under(at);
                            self.focus_touch_target(under.as_ref(), serial);
                        }
                    }
                    smithay::backend::input::TabletToolTipState::Up => tool.up(
                        self,
                        &smithay::input::tablet::tool::UpEvent { serial, time },
                    ),
                }
                tool.frame(self, time);
            }

            InputEvent::TabletToolButton { event, .. } => {
                use smithay::backend::input::{TabletToolButtonEvent as _, TabletToolEvent as _};
                let Some(tool) = self.seat.tablet_seat().get_tool(&event.tool()) else {
                    return;
                };
                let time = event.time();
                tool.button(
                    self,
                    &smithay::input::tablet::tool::ButtonEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        button: event.button(),
                        state: event.button_state(),
                        time,
                    },
                );
                tool.frame(self, time);
            }

            // Touchpad gestures forward whole unless this finger count has a
            // configured binding. Capture is decided at begin: waiting for a
            // threshold would leak a partial sequence to the client.
            InputEvent::GestureSwipeBegin { event, .. } => {
                self.gesture = None;
                if !self.locked
                    && captures_gesture(&self.gestures, GestureKind::Swipe, event.fingers())
                {
                    self.gesture = Some(GestureState::Swipe {
                        fingers: event.fingers(),
                        dx: 0.0,
                        dy: 0.0,
                    });
                    return;
                }
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_swipe_begin(
                        self,
                        &GestureSwipeBeginEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time(),
                            fingers: event.fingers(),
                        },
                    );
                }
            }
            InputEvent::GestureSwipeUpdate { event, .. } => {
                if let Some(GestureState::Swipe { dx, dy, .. }) = self.gesture.as_mut() {
                    *dx += event.delta().x;
                    *dy += event.delta().y;
                    return;
                }
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_swipe_update(
                        self,
                        &GestureSwipeUpdateEvent {
                            time: event.time(),
                            delta: event.delta(),
                        },
                    );
                }
            }
            InputEvent::GestureSwipeEnd { event, .. } => {
                if let Some(state @ GestureState::Swipe { .. }) = self.gesture.take() {
                    if !self.locked {
                        if let Some(action) =
                            finish_gesture(state, &self.gestures, event.cancelled())
                        {
                            self.handle_action(Action::Bound(action));
                        }
                    }
                    return;
                }
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_swipe_end(
                        self,
                        &GestureSwipeEndEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time(),
                            // A gesture the touchpad gave up on is not a
                            // gesture that finished, and a client that is told
                            // it finished acts on it.
                            cancelled: event.cancelled(),
                        },
                    );
                }
            }
            InputEvent::GesturePinchBegin { event, .. } => {
                self.gesture = None;
                if !self.locked
                    && captures_gesture(&self.gestures, GestureKind::Pinch, event.fingers())
                {
                    self.gesture = Some(GestureState::Pinch {
                        fingers: event.fingers(),
                        scale: 1.0,
                    });
                    return;
                }
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_pinch_begin(
                        self,
                        &GesturePinchBeginEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time(),
                            fingers: event.fingers(),
                        },
                    );
                }
            }
            InputEvent::GesturePinchUpdate { event, .. } => {
                if let Some(GestureState::Pinch { scale, .. }) = self.gesture.as_mut() {
                    *scale = event.scale();
                    return;
                }
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_pinch_update(
                        self,
                        &GesturePinchUpdateEvent {
                            time: event.time(),
                            delta: event.delta(),
                            scale: event.scale(),
                            rotation: event.rotation(),
                        },
                    );
                }
            }
            InputEvent::GesturePinchEnd { event, .. } => {
                if let Some(state @ GestureState::Pinch { .. }) = self.gesture.take() {
                    if !self.locked {
                        if let Some(action) =
                            finish_gesture(state, &self.gestures, event.cancelled())
                        {
                            self.handle_action(Action::Bound(action));
                        }
                    }
                    return;
                }
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_pinch_end(
                        self,
                        &GesturePinchEndEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time(),
                            cancelled: event.cancelled(),
                        },
                    );
                }
            }
            // A hold is how a touchpad says fingers are resting: what stops
            // kinetic scrolling when a hand comes down on it.
            InputEvent::GestureHoldBegin { event, .. } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_hold_begin(
                        self,
                        &GestureHoldBeginEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time(),
                            fingers: event.fingers(),
                        },
                    );
                }
            }
            InputEvent::GestureHoldEnd { event, .. } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_hold_end(
                        self,
                        &GestureHoldEndEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time(),
                            cancelled: event.cancelled(),
                        },
                    );
                }
            }

            // Touch. A tap is not a click: the pointer is not moved and no
            // button is sent, because a client that supports touch expects
            // touch — and one that does not is better served by nothing than
            // by a pointer that teleports to wherever a finger landed.
            InputEvent::TouchDown { event, .. } => {
                let Some(position) = self.touch_position(&event) else {
                    return;
                };
                self.touch_down_at(event.slot(), position, event.time());
            }

            InputEvent::TouchMotion { event, .. } => {
                let Some(position) = self.touch_position(&event) else {
                    return;
                };
                self.touch_motion_at(event.slot(), position, event.time());
            }

            InputEvent::TouchUp { event, .. } => {
                let Some(touch) = self.seat.get_touch() else {
                    return;
                };
                touch.up(
                    self,
                    &smithay::input::touch::UpEvent {
                        slot: event.slot(),
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time(),
                    },
                );
            }

            // The end of one set of simultaneous touches, which is how a
            // client knows a two-finger gesture was two fingers rather than
            // two taps.
            InputEvent::TouchFrame { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.frame(self);
                }
            }

            // The compositor has taken the sequence over — a gesture, or a
            // device going away mid-touch. A client that is not told this is
            // left with a finger down that never lifts.
            InputEvent::TouchCancel { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.cancel(self);
                }
            }

            _ => {}
        }
    }

    /// Where a touch event landed, in the layout's own coordinates.
    ///
    /// Touch positions arrive as a fraction of the screen, so they mean
    /// nothing without an output to scale them against — and the output has to
    /// be the one the touchscreen is attached to, which for now is the first
    /// one. A tablet with a second monitor plugged in would want the mapping
    /// libinput reports, and that is a device property this does not read yet.
    fn touch_position<E: smithay::backend::input::AbsolutePositionEvent<I>, I: InputBackend>(
        &self,
        event: &E,
    ) -> Option<Point<f64, Logical>> {
        let output = self.space.outputs().next()?;
        let geometry = self.space.output_geometry(output)?;
        let at = event.position_transformed(geometry.size) + geometry.loc.to_f64();
        // Through the magnifier, because a finger is aimed at what is on the
        // screen. This is the one input path that needs the transform at all:
        // a mouse reports a movement, and the compositor's cursor is at a
        // real place whatever the screen is doing with the picture. See
        // `ViewportState::glass_to_content`.
        Some(self.glass_to_content(at))
    }

    /// A finger put down at a place in the layout.
    ///
    /// Split from the `TouchDown` arm for the reason
    /// [`ViewportState::pointer_absolute_to`] is split from its own: the two
    /// kinds of touch device this compositor has disagree about what the
    /// numbers mean and about nothing else. A real touchscreen reports a
    /// fraction of the screen it is glued to, which `touch_position` scales;
    /// a libei client is told the layout's coordinates and sends those.
    ///
    /// Focus rides along in both, through `focus_touch_target`: whichever
    /// hand put the finger down, the window under it took the keyboard.
    pub fn touch_down_at(
        &mut self,
        slot: smithay::backend::input::TouchSlot,
        position: Point<f64, Logical>,
        time: InputTime,
    ) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let under = self.surface_under(position);

        // The window under the finger takes the keyboard too. There is
        // no other way to focus something on a touchscreen: there is no
        // pointer to click with and no way to reach a chord.
        self.focus_touch_target(under.as_ref(), serial);

        touch.down(
            self,
            under,
            &smithay::input::touch::DownEvent {
                slot,
                location: position,
                serial,
                time,
            },
        );
        self.needs_render = true;
    }

    /// A finger that moved to a place in the layout. See
    /// [`ViewportState::touch_down_at`] for why the coordinates arrive already
    /// resolved.
    pub fn touch_motion_at(
        &mut self,
        slot: smithay::backend::input::TouchSlot,
        position: Point<f64, Logical>,
        time: InputTime,
    ) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let under = self.surface_under(position);
        touch.motion(
            self,
            under,
            &smithay::input::touch::MotionEvent {
                slot,
                location: position,
                time,
            },
        );
    }

    /// Put the pointer at a place in the layout, as an absolute device does.
    ///
    /// Split out of the `PointerMotionAbsolute` arm above rather than left
    /// inside it because two kinds of device produce an absolute position and
    /// they disagree about only one thing: what the numbers mean before they
    /// are a place on the desk. A touchscreen or a tablet reports a fraction of
    /// the screen it is attached to, so `touch_position` and the arm above have
    /// to scale it against an output first. A libei client — see
    /// `crate::libei` — is told the layout's own coordinates in
    /// `ei_device.region` and answers in them, so there is nothing to scale and
    /// scaling anyway would put a remote pointer on the wrong monitor. What
    /// happens after the position is known is identical for both, and it is
    /// everything that matters: a pointer lock is honoured, a confinement is
    /// applied, a window being dragged follows, and the shell is told when the
    /// pointer is over it rather than over a client.
    ///
    /// Keep the device timestamp intact until each protocol converts it to the
    /// precision it carries.
    pub fn pointer_absolute_to(&mut self, pos: Point<f64, Logical>, time: InputTime) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };

        // A capture holds whatever device is driving the cursor, not
        // just a mouse. A pen or a touchscreen walking through an
        // active lock would move a cursor the client has been told is
        // standing still, and hand a game absolute positions it reads
        // as an enormous jump.
        let from = pointer.current_location();
        // Once, and reused: a hit test walks every window in the
        // `Space` and clones each one it walks past, and asking the
        // same question of the same point twice costs that twice.
        let under = self.surface_under(from);
        let (locked, confine_to) = self.pointer_constraint(&pointer, under.as_ref());
        // Whether the pointer started over a surface, kept out of the hit
        // test: the relative event below consumes it, and the tail only
        // wants the answer.
        let over_surface = under.is_some();

        // The delta this absolute position implies. It is what a
        // captured client is driven by, and the only thing it gets:
        // the position itself is exactly what a lock withholds.
        let delta = pos - from;
        self.note_pointer_motion(from, delta, locked, confine_to.as_ref(), over_surface);
        pointer.relative_motion(
            self,
            under,
            &smithay::input::pointer::RelativeMotionEvent {
                delta,
                // Nothing accelerated it, so the raw delta is the
                // unaccelerated one.
                delta_unaccel: delta,
                time,
            },
        );
        if locked {
            pointer.frame(self);
            return;
        }
        self.move_pointer(from, delta, confine_to.as_ref(), time);
    }

    /// Carry out one of the compositor's own chords.
    pub fn handle_action(&mut self, action: Action) {
        // Logged at info because a chord that was seen and a chord that never
        // arrived look identical from the outside, and the difference is the
        // whole diagnosis when a compositor will not let go of a TTY.
        if action != Action::Swallow {
            tracing::info!("chord: {action:?}");
        }

        match action {
            Action::Web(key) => self.shell_keyboard_key(key),

            Action::Pick(pick) => match pick {
                Pick::Step(delta) => self.step_screencast_pick(delta as isize),
                Pick::Confirm => self.confirm_screencast_pick(),
                Pick::Cancel => self.cancel_screencast_pick(),
            },

            Action::SwitchVt(vt) => {
                // A kiosk turns this off so the session cannot be left
                // (`src/input.c:1002`). Swallowed rather than forwarded: the
                // chord was intercepted either way, and handing
                // XF86Switch_VT_n to a client would be strange.
                if !self.vt_switching {
                    tracing::debug!("vt switching is disabled by the config");
                    return;
                }
                let Some(udev) = self.udev.as_mut() else {
                    // Nested, where the VT belongs to whatever is hosting us.
                    tracing::debug!("ignoring a VT switch: not on a real session");
                    return;
                };
                use smithay::backend::session::Session as _;
                if let Err(e) = udev.session.change_vt(vt) {
                    tracing::error!("could not switch to VT {vt}: {e}");
                }
            }
            Action::Quit => {
                tracing::info!("quit chord pressed");
                self.shutdown();
            }
            Action::ToggleCapture => match self.capture.as_mut() {
                Some(capture) => {
                    capture.toggle();
                }
                // Not nested, or a host that never agreed to hold anything
                // back. Said once rather than silently doing nothing, because
                // the chord working everywhere except where it matters is the
                // confusing version.
                None => tracing::info!(
                    "nothing to release: this session has no host holding its shortcuts back"
                ),
            },
            Action::Bound(bound) => self.run_binding(bound),
            Action::Swallow => {}
        }
    }

    /// Carry out a binding.
    fn run_binding(&mut self, action: crate::binding::Action) {
        use crate::binding::Action as Bound;

        match action {
            Bound::Exec(command) => spawn(&command),
            Bound::Exit => self.shutdown(),
            Bound::Close => {
                // An X11 window has no xdg toplevel, so `toplevel()` alone
                // drops every XWayland client — close goes to the x11 surface
                // instead, politely when it asked for it, force above that.
                let Some(view) = self.views.get(self.focused) else {
                    return;
                };
                if let Some(toplevel) = view.window.toplevel() {
                    toplevel.send_close();
                } else if let Some(x11) = view.window.x11_surface() {
                    if let Err(e) = x11.close() {
                        tracing::error!("could not close the focused X11 window: {e}");
                    }
                }
            }
            Bound::Background => self.toggle_background_focus(),
            // The same reload `--watch-shell` fires when a file changes, and
            // the only one available when it was not asked for. See
            // [`crate::shell_watch`].
            Bound::Reload => self.reload_shells(),
            Bound::Mode(mode) => {
                tracing::info!(
                    "binding mode: {}",
                    if mode.is_empty() { "default" } else { &mode }
                );
                self.binding_mode = mode;
            }
            Bound::Focus(target) => self.focus_direction(&target),
            Bound::Appearance => {
                self.dark_mode = !self.appearance.is_dark();
                self.appearance.set_dark(self.dark_mode);
                // The shell draws this as a switch on the settings panel, so
                // the chord and the switch have to agree: a panel left open
                // while somebody presses Mod4+Shift+D would otherwise go on
                // showing the scheme the desk was in when it opened.
                self.config.dark_mode = self.dark_mode;
                self.notify_config();
            }
            Bound::Lock => {
                tracing::info!("lock binding");
                self.lock_session();
            }
            Bound::Blank => {
                tracing::info!("blank binding");
                self.blank_screens();
            }
            Bound::Magnify(step) => {
                if self.magnifier.apply(step) {
                    tracing::info!("magnifier at {:.2}x", self.magnifier.zoom());
                    // Nothing else is damage: no surface committed, no window
                    // moved, and the pointer is exactly where it was. What
                    // changed is the transform every element is drawn through,
                    // which only this knows about.
                    self.needs_render = true;
                }
            }
            Bound::Volume {
                source,
                delta,
                mute,
            } => {
                let node = if source {
                    crate::status::SOURCE
                } else {
                    crate::status::SINK
                };
                if self.status.set_audio(node, delta, mute) {
                    self.status_tick_with_osd(Some(if source {
                        viewport_ipc::event::StatusOsd::Microphone
                    } else {
                        viewport_ipc::event::StatusOsd::Volume
                    }));
                }
            }
            Bound::Brightness(delta) => {
                if self.status.set_brightness(delta) {
                    self.status_tick_with_osd(Some(viewport_ipc::event::StatusOsd::Brightness));
                }
            }
            Bound::Shell(command) => {
                // Split on whitespace so the shell gets a verb and arguments
                // rather than a string it has to parse again.
                let mut parts = command.split_whitespace();
                let event = viewport_ipc::Event::ShellCommand {
                    command: parts.next().unwrap_or_default().to_owned(),
                    args: parts.map(str::to_owned).collect(),
                };
                self.notify(&event);
            }
        }
    }

    /// Tell the shell where the pointer is, if it is the shell's to know.
    ///
    /// The transitions are the point. A pointer that moves onto a window has
    /// *left* the shell, and a page that is not told keeps a `:hover` lit under
    /// whatever the pointer went to — which the C build handled by sending a
    /// negative position, and so does this (`src/web.c:492`).
    fn shell_pointer_motion(&mut self, at: Point<f64, Logical>, on_shell: bool, time: u32) {
        if !self.shell_is_up() {
            return;
        }
        // While the shell holds the pointer it gets every position, wherever
        // the cursor has got to: that is what makes a drag survive crossing
        // onto a window.
        let on_shell = on_shell || self.pointer_grabbed_by_shell;
        if !on_shell && !self.pointer_on_shell {
            return;
        }
        self.pointer_on_shell = on_shell;
        // The modifiers are read only by a page, so they are only worth
        // computing when there is one.
        #[cfg(feature = "wpe")]
        {
            let modifiers = self.shell_modifiers();
            // Every page, and each in its own coordinates: a page that has the
            // pointer is told where, and one that does not is told it left. A
            // second page still showing a hover from the last time the pointer was
            // over it is what leaving out the second half looks like.
            for page in &self.shells {
                let local = if on_shell && page.contains(at) {
                    page.local(at)
                } else {
                    (-1.0, -1.0).into()
                };
                page.engine
                    .pointer_motion(time, local.x, local.y, modifiers);
            }
        }
        let _ = (at, time);
    }

    fn shell_pointer_button(
        &mut self,
        at: Point<f64, Logical>,
        button: u32,
        pressed: bool,
        time: u32,
    ) {
        // The modifiers are read only by a page, so they are only worth
        // computing when there is one.
        #[cfg(feature = "wpe")]
        {
            let modifiers = self.shell_modifiers();
            // Only the page under the pointer. A click is not an event every page
            // should see.
            if let Some(page) = self.shells.iter().find(|page| page.contains(at)) {
                let local = page.local(at);
                page.engine
                    .pointer_button(time, local.x, local.y, button, pressed, modifiers);
            }
        }
        let _ = (at, button, pressed, time);
    }

    fn shell_pointer_axis(
        &mut self,
        at: Point<f64, Logical>,
        dx: f64,
        dy: f64,
        precise: bool,
        time: u32,
    ) {
        // The modifiers are read only by a page, so they are only worth
        // computing when there is one.
        #[cfg(feature = "wpe")]
        {
            let modifiers = self.shell_modifiers();
            if let Some(page) = self.shells.iter().find(|page| page.contains(at)) {
                let local = page.local(at);
                page.engine
                    .pointer_axis(time, local.x, local.y, dx, dy, precise, modifiers);
            }
        }
        let _ = (at, dx, dy, precise, time);
    }

    fn shell_keyboard_key(&mut self, key: WebKey) {
        // The desktop page. There is one keyboard focus and the desktop is
        // what holds it when no window does; handing the same key to a `--url`
        // page on another monitor would type into a site nobody is looking at.
        #[cfg(feature = "wpe")]
        if let Some(shell) = self
            .shells
            .iter()
            .find(|page| page.desktop)
            .map(|page| &page.engine)
        {
            shell.keyboard_key(
                key.time,
                key.keycode,
                key.keysym,
                key.pressed,
                key.modifiers,
            );
        }
        let _ = key;
    }

    /// The modifiers as WPE numbers them.
    ///
    /// Only a page reads them, so without the web engine nobody calls this.
    #[cfg_attr(not(feature = "wpe"), allow(dead_code))]
    fn shell_modifiers(&self) -> u32 {
        // WPEModifiers, from WPEEvent.h. Named rather than computed, because
        // the bit order is not the one Wayland uses and a mistake here is a
        // page that thinks Control is held.
        const CONTROL: u32 = 1 << 0;
        const SHIFT: u32 = 1 << 1;
        const ALT: u32 = 1 << 2;
        const META: u32 = 1 << 3;
        const CAPS_LOCK: u32 = 1 << 4;

        let Some(keyboard) = self.seat.get_keyboard() else {
            return 0;
        };
        let held = keyboard.modifier_state();
        let mut out = 0;
        if held.ctrl {
            out |= CONTROL;
        }
        if held.shift {
            out |= SHIFT;
        }
        if held.alt {
            out |= ALT;
        }
        if held.logo {
            out |= META;
        }
        if held.caps_lock {
            out |= CAPS_LOCK;
        }
        out
    }

    /// Carry a drag as far as the pointer has got.
    ///
    /// Deltas rather than positions: what the shell is being asked is "this
    /// much further", which is the same question whether the window is
    /// floating, tiled or in a column, and the shell answers each of those
    /// differently. Sending a position instead would make it the compositor's
    /// business where a window may be, which is the one thing this design puts
    /// on the other side.
    fn drag_to(&mut self, pos: Point<f64, Logical>) {
        /// Fast enough to track the pointer, slow enough that a mouse
        /// reporting a thousand times a second does not ask the shell for a
        /// thousand messages.
        ///
        /// A quarter of a frame rather than half of one. This throttle used to
        /// be what stopped the shell laying the desktop out faster than it is
        /// drawn — the shell now does that itself, coalescing every delta that
        /// arrives between two frames into one relayout (`gestureRelayout` in
        /// geometry.js), so what is left here is only the cost of the message.
        ///
        /// What the interval costs is accuracy, not work: whatever has arrived
        /// since the last send is still sitting in `pending` when the frame is
        /// drawn, so the window is behind the pointer by up to one interval's
        /// worth of travel, and by a *different* amount each frame as the two
        /// clocks slide past each other. That is a drag that moves unevenly
        /// while keeping up on average, which is what this is being narrowed
        /// for.
        const EVERY: std::time::Duration = std::time::Duration::from_millis(4);

        let Some(drag) = self.pointer_drag.as_mut() else {
            return;
        };
        let delta = pos - drag.last;
        drag.last = pos;
        drag.pending.0 += delta.x;
        drag.pending.1 += delta.y;

        if drag.sent.is_some_and(|at| at.elapsed() < EVERY) {
            return;
        }
        let (dx, dy) = (drag.pending.0.trunc(), drag.pending.1.trunc());
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        // What is left over stays: a drag slow enough to move less than a
        // pixel between reports still moves.
        drag.pending.0 -= dx;
        drag.pending.1 -= dy;
        drag.sent = Some(std::time::Instant::now());

        let command = match drag.kind {
            crate::state::DragKind::Resize => "layout.resize.delta",
            crate::state::DragKind::Move => "layout.move.delta",
            crate::state::DragKind::Pan => "canvas.pan.delta",
        };
        // A pan is about the desktop and names no window, so it carries the
        // delta alone. Sending an id of nothing would be a window the shell
        // would then go looking for.
        let args = match drag.kind {
            crate::state::DragKind::Pan => {
                vec![(dx as i32).to_string(), (dy as i32).to_string()]
            }
            // A resize also carries the corner it is pulling, because a delta
            // alone cannot say it: dragging left is a window growing when the
            // hand is on its left edge and shrinking when it is on the right.
            // Named rather than signed, so the shell can also say which
            // neighbour gives up the space.
            crate::state::DragKind::Resize => vec![
                drag.id.to_string(),
                (dx as i32).to_string(),
                (dy as i32).to_string(),
                drag.edge
                    .unwrap_or_else(|| edge_name(drag.edges))
                    .to_owned(),
            ],
            crate::state::DragKind::Move => vec![
                drag.id.to_string(),
                (dx as i32).to_string(),
                (dy as i32).to_string(),
            ],
        };
        let event = viewport_ipc::Event::ShellCommand {
            command: command.to_owned(),
            args,
        };
        self.notify(&event);
    }

    /// End whichever shell-owned pointer gesture is active.
    pub(crate) fn finish_pointer_drag(&mut self) {
        if self.pointer_drag.take().is_none() {
            return;
        }
        self.notify(&viewport_ipc::Event::ShellCommand {
            command: "layout.drag.end".to_owned(),
            args: Vec::new(),
        });
    }
}

impl ViewportState {
    /// Move focus to the neighbouring window, or step through them.
    ///
    /// Falls back to the shell when there is no window that way: in sway a
    /// directional focus with nothing there moves to the monitor in that
    /// direction even when it is empty, and only the shell knows which output
    /// is active (`src/xdg_shell.c:1052`).
    fn focus_direction(&mut self, target: &str) {
        use crate::focus::{self, Candidate, Direction};

        if target == "next" || target == "prev" {
            let ids: Vec<u32> = self
                .views
                .iter()
                .filter(|v| v.mapped && v.visible)
                .map(|v| v.id)
                .collect();
            let current = (self.focused != crate::views::NO_VIEW).then_some(self.focused);
            if let Some(id) = focus::step(&ids, current, target == "next") {
                crate::apply::focus_view(self, id);
            }
            return;
        }

        let Some(direction) = Direction::parse(target) else {
            tracing::warn!("unknown focus direction {target:?}");
            return;
        };

        let candidates: Vec<Candidate> = self
            .views
            .iter()
            .filter(|v| v.mapped && v.visible && v.placed)
            .filter_map(|v| {
                self.space
                    .element_geometry(&v.window)
                    .map(|rect| Candidate { id: v.id, rect })
            })
            .collect();
        let from = candidates.iter().find(|c| c.id == self.focused).copied();

        if let Some(id) = focus::nearest(&candidates, from, direction) {
            crate::apply::focus_view(self, id);
            return;
        }

        // Nothing that way. Hand the direction to the shell rather than doing
        // nothing, so the focus moves to the next monitor even if it is empty.
        //
        // Unless that was turned off, in which case the edge of the monitor is
        // where directional focus stops and the keypress does nothing — which
        // is the point, rather than an omission.
        if !self.config.focus_crosses_outputs {
            return;
        }
        let event = viewport_ipc::Event::ShellCommand {
            command: "output.focus".to_owned(),
            args: vec![target.to_owned()],
        };
        self.notify(&event);
    }
}

/// The region a confined pointer is held inside, and the surface's origin in
/// layout coordinates.
///
/// Both are needed together because the region is surface-local and the
/// pointer is not.
type Confinement = (
    Vec<smithay::utils::Rectangle<i32, smithay::utils::Logical>>,
    smithay::utils::Point<i32, smithay::utils::Logical>,
);

impl ViewportState {
    /// Note a pointer motion in the capture narration, whatever moved it.
    ///
    /// What the compositor believes about capture right now is the one thing
    /// the symptom cannot tell you, and this is where that belief is kept.
    /// It runs before the lock check in every caller — a locked pointer is
    /// itself a state worth narrating — and it counts every motion, not only
    /// a mouse's: the injected and absolute paths once skipped all of this,
    /// and a remote session could move the cursor for hours while the debug
    /// story claimed nothing was moving at all.
    ///
    /// `from` and `delta` are where the pointer was and what it was told to
    /// move by, both in layout coordinates; the log line wants them as they
    /// arrived rather than after clamping, because the interesting case is
    /// the one at the edge. `over_surface` is whether that starting position
    /// hit a client, and is passed as the bool it is consumed as: the hit
    /// test itself belongs to the caller, which needs it for the relative
    /// event and should not pay for it twice.
    ///
    /// Every change of state unconditionally — there are a handful in a
    /// session and each one matters. The running commentary only when asked,
    /// because a gaming mouse sends thousands of these a second.
    fn note_pointer_motion(
        &mut self,
        from: Point<f64, Logical>,
        delta: Point<f64, Logical>,
        locked: bool,
        confine_to: Option<&Confinement>,
        over_surface: bool,
    ) {
        self.pointer_motions += 1;
        // The state as a constant, so the comparison costs nothing: this runs
        // for every motion event a gaming mouse sends and a string built to
        // be thrown away is a string built thousands of times a second. How
        // many rectangles the confinement has is worth having in the line and
        // is therefore formatted where the line is, not here.
        let state: &'static str = if locked {
            "locked"
        } else if confine_to.is_some() {
            "confined"
        } else {
            "free"
        };
        let changed = self.pointer_capture.as_deref() != Some(state);
        if changed {
            self.pointer_capture = Some(state.to_owned());
        }
        if changed || (crate::pointer::debug() && self.pointer_motions % 100 == 1) {
            let over = if over_surface {
                "a surface"
            } else {
                "the shell"
            };
            match confine_to.as_ref().filter(|_| !locked) {
                Some((region, _)) => tracing::info!(
                    "pointer: delta {delta:?} at {from:?}, confined to {} rect(s), over {over}",
                    region.len()
                ),
                None => {
                    tracing::info!("pointer: delta {delta:?} at {from:?}, {state}, over {over}")
                }
            }
        }
    }

    /// The one tail every relative pointer motion shares: clamp to the
    /// monitors, hold the confinement, land the pointer, drag what is being
    /// dragged, tell the shell.
    ///
    /// The caller has already sent the relative event, noted the event with
    /// [`ViewportState::note_pointer_motion`], and given up when the pointer
    /// is locked — those differ per device and stay above. What is below the
    /// lock check was the same three times over, copy-pasted, and drifting
    /// apart is exactly the fate copies meet; here is the one place it
    /// cannot.
    ///
    /// `from` and `delta` are where the pointer was and what it was told to
    /// move by, both in layout coordinates; the clamp wants them as they
    /// arrived, because the interesting case is the one at the edge.
    ///
    fn move_pointer(
        &mut self,
        from: Point<f64, Logical>,
        delta: Point<f64, Logical>,
        confine_to: Option<&Confinement>,
        time: InputTime,
    ) {
        // The caller has a pointer by definition — every one of them gave up
        // already when there was none — so this cannot fail.
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };

        let outputs: Vec<_> = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .collect();
        let mut pos = crate::cursor::clamp(&outputs, from, from + delta);

        // Confinement: still moves, but may not leave the region the client
        // nominated — a windowed game, or a map widget.
        if let Some((region, origin)) = confine_to {
            let local = pos - origin.to_f64();
            if let Some(snapped) = crate::pointer::confine(region, local) {
                pos = snapped + origin.to_f64();
            }
        }

        let serial = SERIAL_COUNTER.next_serial();
        let under = self.surface_under(pos);
        let on_shell = under.is_none();
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
        self.drag_to(pos);
        self.shell_pointer_motion(pos, on_shell, time.millis());
        // The cursor moved, and nothing else would draw it.
        self.needs_render = true;
    }

    /// Give the keyboard to whatever window a finger or a pen tip landed on.
    ///
    /// The three ways something touches this compositor — a scripted touch, a
    /// real touchscreen, a tablet tool's tip — all want the same thing to
    /// happen next, because from the window's side there is no difference:
    /// input arrived, and it is the one it went to. One opinion, then, not
    /// three that can drift.
    fn focus_touch_target(
        &mut self,
        under: Option<&(WlSurface, Point<f64, Logical>)>,
        serial: Serial,
    ) {
        let Some((surface, _)) = under else {
            return;
        };
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        // Through the view rather than the surface, as the click path does:
        // an X11 window focused as a bare surface gets `wl_keyboard.enter`
        // and no `SetInputFocus`, so the X server stays at `PointerRoot` and
        // the window is never told it has the keyboard.
        let focus = self
            .views
            .find_by_surface(surface)
            .and_then(|view| crate::keyboard_focus::KeyboardFocus::for_window(&view.window))
            .unwrap_or_else(|| surface.clone().into());
        keyboard.set_focus(self, Some(focus), serial);
    }

    /// Whether the surface under the pointer has captured it, and to what.
    ///
    /// Returns whether the pointer is locked, and the region it is confined to
    /// with the surface's origin in layout coordinates — the region is
    /// surface-local and the pointer is not.
    fn pointer_constraint(
        &self,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
        under: Option<&(
            WlSurface,
            smithay::utils::Point<f64, smithay::utils::Logical>,
        )>,
    ) -> (bool, Option<Confinement>) {
        use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraint};

        let Some((surface, origin)) = under else {
            return (false, None);
        };
        let origin = origin.to_i32_round();

        let mut locked = false;
        let mut confine = None;
        with_pointer_constraint(surface, pointer, |constraint| {
            let Some(constraint) = constraint else {
                return;
            };
            // A constraint that exists but has not been activated does not
            // apply: activation is what the client is told about, and acting
            // before it would capture a pointer the client is not expecting.
            if !constraint.is_active() {
                return;
            }
            match &*constraint {
                PointerConstraint::Locked(_) => locked = true,
                PointerConstraint::Confined(confined) => {
                    // The additive rectangles only. A region may also
                    // subtract, but a hole in a confinement region has no
                    // sensible edge to snap a cursor to — and no client asks
                    // for one. Ignoring the subtractions confines to slightly
                    // more than was asked, which is the safe direction: the
                    // cursor stays inside the surface either way.
                    use smithay::wayland::compositor::RectangleKind;
                    confine = Some(
                        confined
                            .region()
                            .map(|region| {
                                region
                                    .rects
                                    .iter()
                                    .filter(|(kind, _)| matches!(kind, RectangleKind::Add))
                                    .map(|(_, rect)| *rect)
                                    .collect()
                            })
                            // No region means the whole surface, not "nowhere".
                            // Every XWayland confinement arrives this way
                            // (`xwl_seat_confine_pointer` passes NULL), so
                            // reading it as an empty region left an X11 game's
                            // cursor free to walk off its own window.
                            .unwrap_or_else(|| {
                                let bbox = smithay::desktop::utils::bbox_from_surface_tree(
                                    surface,
                                    (0, 0),
                                );
                                // A surface with nothing committed to it has
                                // no area, and confining to that would pin the
                                // cursor to a corner. Leave it free instead.
                                if bbox.is_empty() {
                                    Vec::new()
                                } else {
                                    vec![bbox]
                                }
                            }),
                    );
                }
            }
        });
        (locked, confine.map(|region| (region, origin)))
    }
}

/// What a tablet event says about the pen, for the axes that changed.
///
/// Only the ones that changed: the protocol is a delta, and reporting an
/// unchanged pressure every event makes a stroke look like it was pressed
/// evenly when it was not.
fn axis_frame<
    E: smithay::backend::input::TabletToolEvent<I>,
    I: smithay::backend::input::InputBackend,
>(
    event: &E,
) -> smithay::input::tablet::tool::AxisFrame {
    smithay::input::tablet::tool::AxisFrame {
        pressure: event.pressure_has_changed().then(|| event.pressure()),
        distance: event.distance_has_changed().then(|| event.distance()),
        tilt: event.tilt_has_changed().then(|| event.tilt()),
        rotation: event.rotation_has_changed().then(|| event.rotation()),
        slider: event.slider_has_changed().then(|| event.slider_position()),
        wheel: event
            .wheel_has_changed()
            .then(|| (event.wheel_delta(), event.wheel_delta_discrete())),
    }
}

/// Keys from a virtual keyboard, given the same reading as keys from a real
/// one.
///
/// `zwp_virtual_keyboard_v1` hands its keys straight to the focused client, so
/// nothing a compositor does with a key applies to them: `wtype Return` types
/// into a terminal, and `wtype -k Return` at a chooser that has taken the
/// keyboard does nothing at all, because the chooser is a decision made in the
/// filter these keys never pass through. Under wlroots the same events arrive
/// as a keyboard on the seat and do reach the compositor, which is the
/// behaviour anything driving a session by script expects.
///
/// The hook this implements resolves the keysym against the virtual keyboard's
/// own keymap before offering it here — the client picked the keycode out of a
/// keymap it uploaded, and against the seat's it would mean another key.
///
/// The chooser reads the modified symbol, because that is the key it is named
/// by; a binding reads the unmodified one, because a chord is written
/// "Mod4+Shift+q" — the shift is in the modifiers and the key is still q.
impl smithay::wayland::virtual_keyboard::VirtualKeyboardKeyFilter for ViewportState {
    fn virtual_keyboard_key(
        &mut self,
        _seat: &smithay::input::Seat<Self>,
        keysym: Keysym,
        raw_keysym: Option<Keysym>,
        mods: ModifiersState,
        _keycode: u32,
        state: smithay::reexports::wayland_server::protocol::wl_keyboard::KeyState,
        _time: u32,
    ) -> bool {
        use smithay::reexports::wayland_server::protocol::wl_keyboard::KeyState;

        if state != KeyState::Pressed {
            // The release of a key whose press was kept. A client that saw
            // only the release would think the key was stuck.
            if let Some(at) = self.suppressed_keys.iter().position(|k| *k == keysym) {
                self.suppressed_keys.remove(at);
                return true;
            }
            return false;
        }

        // A client holding a shortcut inhibitor gets everything, exactly as it
        // does from the real keyboard.
        if self.shortcuts_inhibited() {
            return false;
        }

        // The chooser owns the keyboard while it is up, and owns it here too:
        // a keystroke that fell through would go to whatever was focused
        // before the share was asked for.
        if self.picker.is_some() {
            let pick = match keysym {
                Keysym::Escape => Some(Pick::Cancel),
                Keysym::Return | Keysym::KP_Enter | Keysym::space => Some(Pick::Confirm),
                Keysym::Up | Keysym::k => Some(Pick::Step(-1)),
                Keysym::Down | Keysym::j | Keysym::Tab => Some(Pick::Step(1)),
                _ => None,
            };
            self.suppressed_keys.push(keysym);
            self.handle_action(pick.map(Action::Pick).unwrap_or(Action::Swallow));
            return true;
        }

        if let Some(action) = shortcut(&mods, keysym) {
            if self.locked && !matches!(action, Action::SwitchVt(_)) {
                return false;
            }
            self.suppressed_keys.push(keysym);
            self.handle_action(action);
            return true;
        }

        // No binding fires while locked — one that spawns a terminal would put
        // it on top of the lock screen — but the key still goes to the client,
        // because the client is the lock screen and the key is the password.
        if self.locked {
            return false;
        }

        // The *unmodified* symbol, as the physical path uses. A chord is
        // written "Mod4+Shift+q": the shift is in the modifiers and the key is
        // still q, so matching the modified symbol would look for Q and never
        // find it.
        let unmodified = raw_keysym.unwrap_or(keysym).raw();
        match crate::binding::match_binding(&self.bindings, &mods, unmodified, &self.binding_mode) {
            Some(bound) => {
                let bound = bound.clone();
                self.suppressed_keys.push(keysym);
                self.handle_action(Action::Bound(bound));
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modifiers(ctrl: bool, alt: bool) -> ModifiersState {
        ModifiersState {
            ctrl,
            alt,
            ..Default::default()
        }
    }

    #[test]
    fn inject_touch_slots_are_named_fingers() {
        // A script says 0, 1, 2. Those have to be the same slots a real
        // two-finger gesture uses, or a scripted pinch is two unknown fingers
        // a client never tracks.
        assert_eq!(
            inject_touch_slot(0),
            smithay::backend::input::TouchSlot::from(Some(0))
        );
        assert_eq!(
            inject_touch_slot(1),
            smithay::backend::input::TouchSlot::from(Some(1))
        );
    }

    #[test]
    fn every_vt_switch_keysym_maps_to_its_number() {
        // xkb emits these only when Ctrl+Alt is held, so no modifier check is
        // needed — but the arithmetic has to be right or Ctrl+Alt+F3 switches
        // to the wrong terminal.
        for vt in 1..=12i32 {
            let raw = keysyms::KEY_XF86Switch_VT_1 + (vt as u32 - 1);
            assert_eq!(
                shortcut(&modifiers(false, false), Keysym::new(raw)),
                Some(Action::SwitchVt(vt)),
                "VT {vt}"
            );
        }
    }

    #[test]
    fn quit_needs_both_modifiers() {
        // Backspace alone is a key clients need. Swallowing it would make
        // every text field in every application unusable.
        let backspace = Keysym::new(keysyms::KEY_BackSpace);
        assert_eq!(shortcut(&modifiers(false, false), backspace), None);
        assert_eq!(shortcut(&modifiers(true, false), backspace), None);
        assert_eq!(shortcut(&modifiers(false, true), backspace), None);
        assert_eq!(
            shortcut(&modifiers(true, true), backspace),
            Some(Action::Quit)
        );
    }

    /// A press on what the shell drew in front is a click on it, not a gesture
    /// through it.
    ///
    /// The bar under `auto` is on screen only while Mod4 is held, so every
    /// click it can ever receive arrives with the gesture modifier down. Taken
    /// for a gesture, the workspace pills and the window titles in the taskbar
    /// do nothing at all — and with no window under the pointer it was not
    /// even a window drag that ate them, it was the pan.
    #[test]
    fn a_press_on_the_shells_own_furniture_is_not_a_gesture() {
        // Mod4 and a button, over the desktop: the gesture, as before.
        assert!(starts_gesture(true, false, false, true, BTN_LEFT));
        assert!(starts_gesture(true, false, false, true, BTN_RIGHT));

        // The same press over the bar, a notification or the chooser.
        assert!(!starts_gesture(true, false, true, true, BTN_LEFT));
        assert!(
            !starts_gesture(true, false, true, true, BTN_RIGHT),
            "the right button is a resize, and resizing the bar is nothing"
        );

        // And the rest of the rule, unchanged: a release starts nothing, a
        // grab already running owns the button, the modifier is required, and
        // only the two buttons a gesture is drawn on count.
        assert!(!starts_gesture(false, false, false, true, BTN_LEFT));
        assert!(!starts_gesture(true, true, false, true, BTN_LEFT));
        assert!(!starts_gesture(true, false, false, false, BTN_LEFT));
        assert!(!starts_gesture(true, false, false, true, 0x112));
    }

    /// A resize takes hold of the corner nearest the press, rather than the
    /// bottom right one whatever the hand is on.
    ///
    /// The whole window is the handle here — there is no border to aim at —
    /// so every point in it has to name a corner, and the halves of the window
    /// are how it does. What that fixes is a drag on the left edge of a window
    /// moving its right edge instead.
    #[test]
    fn a_resize_takes_the_corner_nearest_the_press() {
        let window = Rectangle::new((100, 200).into(), (400, 300).into());
        let at = |x: f64, y: f64| resize_edges((x, y).into(), window);

        assert_eq!(at(120.0, 220.0), (true, true), "top left");
        assert_eq!(at(480.0, 220.0), (false, true), "top right");
        assert_eq!(at(120.0, 480.0), (true, false), "bottom left");
        assert_eq!(at(480.0, 480.0), (false, false), "bottom right");

        // The middle belongs to the bottom right, which is where a resize has
        // always pulled from and what the wire spells with no corner at all.
        assert_eq!(at(300.0, 350.0), (false, false), "dead centre");
        assert_eq!(edge_name(at(300.0, 350.0)), "bottom-right");
        assert_eq!(edge_name(at(120.0, 220.0)), "top-left");
    }

    /// Mod4 and the left button moves a window; the right one resizes it. What
    /// happens in between is the part that had no answer: the other button
    /// pressed mid-drag restarted the drag as the other kind, and its release
    /// ended a drag whose own button was still held.
    #[test]
    fn a_drag_belongs_to_the_button_that_started_it() {
        assert_eq!(drag_effect(None, BTN_LEFT, true), DragEffect::Free);
        assert_eq!(drag_effect(None, BTN_LEFT, false), DragEffect::Free);

        // The other button, pressed and released, while the first is held.
        assert_eq!(
            drag_effect(Some(BTN_LEFT), BTN_RIGHT, true),
            DragEffect::Swallow
        );
        assert_eq!(
            drag_effect(Some(BTN_LEFT), BTN_RIGHT, false),
            DragEffect::Swallow
        );

        // A press of the drag's own button is not a second drag either.
        assert_eq!(
            drag_effect(Some(BTN_LEFT), BTN_LEFT, true),
            DragEffect::Swallow
        );

        // Only its release ends it.
        assert_eq!(
            drag_effect(Some(BTN_LEFT), BTN_LEFT, false),
            DragEffect::End
        );
        assert_eq!(
            drag_effect(Some(BTN_RIGHT), BTN_RIGHT, false),
            DragEffect::End
        );
    }

    #[test]
    fn the_page_only_gets_a_release_for_a_press_it_saw() {
        // Pressed over the shell, released over the shell.
        assert!(shell_gets_button(false, true, true));
        assert!(shell_gets_button(true, true, false));

        // Pressed over the shell, dragged onto a window, released there: the
        // grab holds, the way it does for a client.
        assert!(shell_gets_button(true, false, false));

        // Pressed over a window, dragged onto the shell, released there. The
        // press was never the page's, so neither is this.
        assert!(!shell_gets_button(false, true, false));

        // And an ordinary click on a window is not the page's either.
        assert!(!shell_gets_button(false, false, true));
    }

    #[test]
    fn ordinary_keys_are_left_alone() {
        // The compositor keeps two chords and forwards everything else; a
        // greedy match here would be invisible until an application lost a
        // keystroke.
        for raw in [
            keysyms::KEY_a,
            keysyms::KEY_Return,
            keysyms::KEY_F1,
            keysyms::KEY_Tab,
            keysyms::KEY_space,
        ] {
            assert_eq!(shortcut(&modifiers(true, true), Keysym::new(raw)), None);
        }
    }

    #[test]
    fn gesture_specs_use_binding_actions() {
        let swipe = parse_gesture("swipe:3:left", "shell workspace.next").unwrap();
        assert_eq!(swipe.kind, GestureKind::Swipe);
        assert_eq!(swipe.fingers, 3);
        assert_eq!(swipe.direction, GestureDirection::Left);
        assert_eq!(
            swipe.action,
            crate::binding::Action::Shell("workspace.next".to_owned())
        );
        assert!(parse_gesture("pinch:0:in", "close").is_none());
        assert!(parse_gesture("pinch:2:left", "close").is_none());
        assert!(parse_gesture("swipe:3:left:extra", "close").is_none());
    }

    #[test]
    fn swipe_requires_threshold_and_dominant_direction() {
        let bindings = [parse_gesture("swipe:3:left", "close").unwrap()];
        assert!(captures_gesture(&bindings, GestureKind::Swipe, 3));
        assert!(!captures_gesture(&bindings, GestureKind::Swipe, 4));
        assert_eq!(
            finish_gesture(
                GestureState::Swipe {
                    fingers: 3,
                    dx: -120.0,
                    dy: 30.0,
                },
                &bindings,
                false,
            ),
            Some(crate::binding::Action::Close)
        );
        for state in [
            GestureState::Swipe {
                fingers: 3,
                dx: -119.9,
                dy: 0.0,
            },
            GestureState::Swipe {
                fingers: 3,
                dx: -130.0,
                dy: 130.0,
            },
        ] {
            assert_eq!(finish_gesture(state, &bindings, false), None);
        }
        assert_eq!(
            finish_gesture(
                GestureState::Swipe {
                    fingers: 3,
                    dx: -200.0,
                    dy: 0.0,
                },
                &bindings,
                true,
            ),
            None,
            "cancelled gestures never act"
        );
    }

    #[test]
    fn pinch_uses_cumulative_scale_thresholds() {
        let bindings = [
            parse_gesture("pinch:2:out", "magnify in").unwrap(),
            parse_gesture("pinch:2:in", "magnify out").unwrap(),
        ];
        assert_eq!(
            finish_gesture(
                GestureState::Pinch {
                    fingers: 2,
                    scale: 1.2,
                },
                &bindings,
                false,
            ),
            Some(crate::binding::Action::Magnify(crate::magnify::Step::In))
        );
        assert_eq!(
            finish_gesture(
                GestureState::Pinch {
                    fingers: 2,
                    scale: 0.8,
                },
                &bindings,
                false,
            ),
            Some(crate::binding::Action::Magnify(crate::magnify::Step::Out))
        );
        assert_eq!(
            finish_gesture(
                GestureState::Pinch {
                    fingers: 2,
                    scale: 1.19,
                },
                &bindings,
                false,
            ),
            None
        );
    }
}
