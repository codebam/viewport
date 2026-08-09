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
    InputEvent, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    TouchEvent,
};
use smithay::input::keyboard::{keysyms, FilterResult, Keysym, ModifiersState};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, MotionEvent,
};
use smithay::input::tablet::{TabletDescriptor, TabletSeatTrait as _};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

/// The two buttons a drag can start with, as libinput numbers them.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

use crate::state::ViewportState;
use crate::views::NO_VIEW;

/// Run a command, detached.
///
/// Double-forked through a shell so the compositor does not accumulate
/// zombies and a launched application outlives the key that started it.
pub fn spawn(command: &str) {
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
    let result = child
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match result {
        Ok(mut child) => {
            // Reaped immediately: sh exits as soon as it has exec'd, and
            // waiting for the application itself would block the compositor.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::error!("could not run {command}: {e}"),
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
        let time = self.start_time.elapsed().as_millis() as u32;
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
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // Anything at all counts. Device added and removed do not — they
        // arrive when a dock is plugged in with nobody at the machine — but
        // they are filtered out before this.
        if self.idle.activity() {
            // The screens were off. Bring them back through the same path the
            // deadline turned them off by.
            self.set_outputs_enabled(true);
        }
        // And any client that asked to be told when the session goes idle —
        // a chat program marking you away, which is not the compositor's
        // business to decide but is its business to report.
        let seat = self.seat.clone();
        self.idle_notifier_state.notify_activity(&seat);

        // And the pointer's own deadline, which counts a narrower set of
        // events than either of those — see `uses_the_pointer`.
        if uses_the_pointer(&event) {
            self.cursor_activity();
        }

        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
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
                let modifiers_now = self.shell_modifiers();

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
                                        time,
                                    })))
                                }
                                None => FilterResult::Forward,
                            }
                        } else if let Some(at) =
                            state.suppressed_keys.iter().position(|k| *k == keysym)
                        {
                            state.suppressed_keys.remove(at);
                            // A key the page was given has to be released to
                            // it as well, or the page has one held down for
                            // ever.
                            if to_shell {
                                FilterResult::Intercept(Some(Action::Web(WebKey {
                                    keycode: handle.raw_code().raw() + 8,
                                    keysym: keysym.raw(),
                                    pressed: false,
                                    modifiers: modifiers_now,
                                    time,
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

                {
                    // What the compositor believes about capture right now,
                    // which is the one thing the symptom cannot tell you.
                    //
                    // Every change of state unconditionally — there are a
                    // handful in a session and each one matters. The running
                    // commentary only when asked, because a gaming mouse sends
                    // thousands of these a second.
                    self.pointer_motions += 1;
                    let state = if locked {
                        "locked".to_owned()
                    } else if let Some((region, _)) = confine_to.as_ref() {
                        format!("confined to {} rect(s)", region.len())
                    } else {
                        "free".to_owned()
                    };
                    let changed = self.pointer_capture.as_deref() != Some(state.as_str());
                    if changed {
                        self.pointer_capture = Some(state.clone());
                    }
                    if changed || (crate::pointer::debug() && self.pointer_motions % 100 == 1) {
                        tracing::info!(
                            "pointer: delta {:?} at {from:?}, {state}, over {}",
                            event.delta(),
                            if under.is_some() {
                                "a surface"
                            } else {
                                "the shell"
                            }
                        );
                    }
                }

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
                        utime: event.time(),
                    },
                );

                if locked {
                    // The cursor does not move at all. That is the point: it
                    // neither escapes onto the other monitor mid-fight nor
                    // generates absolute motion the game would misread.
                    pointer.frame(self);
                    return;
                }

                let outputs: Vec<_> = self
                    .space
                    .outputs()
                    .filter_map(|o| self.space.output_geometry(o))
                    .collect();
                let mut pos = crate::cursor::clamp(&outputs, from, from + event.delta());

                // Confinement: still moves, but may not leave the region the
                // client nominated — a windowed game, or a map widget.
                if let Some((region, origin)) = confine_to {
                    let local = pos - origin.to_f64();
                    if let Some(snapped) = crate::pointer::confine(&region, local) {
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
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                self.drag_to(pos);
                self.shell_pointer_motion(pos, on_shell, event.time_msec());
                // The cursor moved, and nothing else would draw it.
                self.needs_render = true;
            }

            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };
                let mut pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };

                // A capture holds whatever device is driving the cursor, not
                // just a mouse. A pen or a touchscreen walking through an
                // active lock would move a cursor the client has been told is
                // standing still, and hand a game absolute positions it reads
                // as an enormous jump.
                let from = pointer.current_location();
                let (locked, confine_to) =
                    self.pointer_constraint(&pointer, self.surface_under(from).as_ref());

                // The delta this absolute position implies. It is what a
                // captured client is driven by, and the only thing it gets:
                // the position itself is exactly what a lock withholds.
                let delta = pos - from;
                pointer.relative_motion(
                    self,
                    self.surface_under(from),
                    &smithay::input::pointer::RelativeMotionEvent {
                        delta,
                        // Nothing accelerated it, so the raw delta is the
                        // unaccelerated one.
                        delta_unaccel: delta,
                        utime: event.time(),
                    },
                );
                if locked {
                    pointer.frame(self);
                    return;
                }
                if let Some((region, origin)) = confine_to {
                    let local = pos - origin.to_f64();
                    if let Some(snapped) = crate::pointer::confine(&region, local) {
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
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                self.drag_to(pos);
                self.shell_pointer_motion(pos, on_shell, event.time_msec());
                self.needs_render = true;
            }

            InputEvent::PointerButton { event, .. } => {
                let (Some(pointer), Some(keyboard)) =
                    (self.seat.get_pointer(), self.seat.get_keyboard())
                else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let state = event.state();

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
                        self.pointer_drag = None;
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
                        return;
                    }
                }

                if state == ButtonState::Pressed
                    && !pointer.is_grabbed()
                    && keyboard.modifier_state().logo
                    && matches!(event.button_code(), BTN_LEFT | BTN_RIGHT)
                {
                    let hit = self
                        .window_under(pointer.current_location())
                        // Not through something the shell drew in front. A
                        // notification sitting over a window is not a handle
                        // for dragging that window about.
                        .filter(|_| {
                            !crate::pointer::over_overlay(
                                &self.shell_overlays,
                                pointer.current_location(),
                            )
                        });
                    if let Some(id) = hit.and_then(|window| {
                        self.views.iter().find(|v| v.window == window).map(|v| v.id)
                    }) {
                        self.pointer_drag = Some(crate::state::PointerDrag {
                            id,
                            button: event.button_code(),
                            resize: event.button_code() == BTN_RIGHT,
                            last: pointer.current_location(),
                            pending: (0.0, 0.0),
                            sent: None,
                        });
                        // Not forwarded. The client did not ask to be dragged
                        // and a button it sees pressed and never released is a
                        // button it thinks is still down.
                        return;
                    }
                }

                if state == ButtonState::Pressed && !pointer.is_grabbed() {
                    let hit = self.window_under(pointer.current_location());
                    // Clicking a notification must not raise and focus the
                    // window behind it — the click never reached that window.
                    let on_overlay = crate::pointer::over_overlay(
                        &self.shell_overlays,
                        pointer.current_location(),
                    );

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
                    self.shell_pointer_button(at, event.button_code(), pressed, event.time_msec());
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
                        time: event.time_msec(),
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

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
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
                        event.time_msec(),
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
                let time = event.time_msec();
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
                let time = event.time_msec();
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
                let time = event.time_msec();
                match event.tip_state() {
                    smithay::backend::input::TabletToolTipState::Down => {
                        tool.down(
                            self,
                            &smithay::input::tablet::tool::DownEvent { serial, time },
                        );
                        let at = self
                            .seat
                            .get_pointer()
                            .map(|pointer| pointer.current_location());
                        if let (Some(at), Some(keyboard)) = (at, self.seat.get_keyboard()) {
                            if let Some((surface, _)) = self.surface_under(at) {
                                keyboard.set_focus(self, Some(surface.into()), serial);
                            }
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
                let time = event.time_msec();
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

            // Touchpad gestures, forwarded whole. A client that cannot see
            // them has no way to tell a three-finger swipe from a scroll,
            // because scroll is all it would otherwise be sent — and pinch to
            // zoom in a browser or a map is exactly this.
            InputEvent::GestureSwipeBegin { event, .. } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_swipe_begin(
                        self,
                        &GestureSwipeBeginEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                            fingers: event.fingers(),
                        },
                    );
                }
            }
            InputEvent::GestureSwipeUpdate { event, .. } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_swipe_update(
                        self,
                        &GestureSwipeUpdateEvent {
                            time: event.time_msec(),
                            delta: event.delta(),
                        },
                    );
                }
            }
            InputEvent::GestureSwipeEnd { event, .. } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_swipe_end(
                        self,
                        &GestureSwipeEndEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                            // A gesture the touchpad gave up on is not a
                            // gesture that finished, and a client that is told
                            // it finished acts on it.
                            cancelled: event.cancelled(),
                        },
                    );
                }
            }
            InputEvent::GesturePinchBegin { event, .. } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_pinch_begin(
                        self,
                        &GesturePinchBeginEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                            fingers: event.fingers(),
                        },
                    );
                }
            }
            InputEvent::GesturePinchUpdate { event, .. } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_pinch_update(
                        self,
                        &GesturePinchUpdateEvent {
                            time: event.time_msec(),
                            delta: event.delta(),
                            scale: event.scale(),
                            rotation: event.rotation(),
                        },
                    );
                }
            }
            InputEvent::GesturePinchEnd { event, .. } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_pinch_end(
                        self,
                        &GesturePinchEndEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
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
                            time: event.time_msec(),
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
                            time: event.time_msec(),
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
                let Some(touch) = self.seat.get_touch() else {
                    return;
                };
                let Some(position) = self.touch_position(&event) else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(position);

                // The window under the finger takes the keyboard too. There is
                // no other way to focus something on a touchscreen: there is no
                // pointer to click with and no way to reach a chord.
                if let Some((surface, _)) = under.as_ref() {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        keyboard.set_focus(self, Some(surface.clone().into()), serial);
                    }
                }

                touch.down(
                    self,
                    under,
                    &smithay::input::touch::DownEvent {
                        slot: event.slot(),
                        location: position,
                        serial,
                        time: event.time_msec(),
                    },
                );
                self.needs_render = true;
            }

            InputEvent::TouchMotion { event, .. } => {
                let Some(touch) = self.seat.get_touch() else {
                    return;
                };
                let Some(position) = self.touch_position(&event) else {
                    return;
                };
                let under = self.surface_under(position);
                touch.motion(
                    self,
                    under,
                    &smithay::input::touch::MotionEvent {
                        slot: event.slot(),
                        location: position,
                        time: event.time_msec(),
                    },
                );
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
                        time: event.time_msec(),
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
        Some(event.position_transformed(geometry.size) + geometry.loc.to_f64())
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
            }
            Bound::Lock => {
                tracing::info!("lock binding");
                self.lock_session();
            }
            Bound::Blank => {
                tracing::info!("blank binding");
                self.blank_screens();
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
        let (x, y) = if on_shell { (at.x, at.y) } else { (-1.0, -1.0) };
        self.pointer_on_shell = on_shell;
        let modifiers = self.shell_modifiers();
        // Every page, and each in its own coordinates: a page that has the
        // pointer is told where, and one that does not is told it left. A
        // second page still showing a hover from the last time the pointer was
        // over it is what leaving out the second half looks like.
        #[cfg(feature = "wpe")]
        for page in &self.shells {
            let local = if on_shell && page.contains(at) {
                page.local(at)
            } else {
                (-1.0, -1.0).into()
            };
            page.engine
                .pointer_motion(time, local.x, local.y, modifiers);
        }
        let _ = (x, y, time, modifiers);
    }

    fn shell_pointer_button(
        &mut self,
        at: Point<f64, Logical>,
        button: u32,
        pressed: bool,
        time: u32,
    ) {
        let modifiers = self.shell_modifiers();
        // Only the page under the pointer. A click is not an event every page
        // should see.
        #[cfg(feature = "wpe")]
        if let Some(page) = self.shells.iter().find(|page| page.contains(at)) {
            let local = page.local(at);
            page.engine
                .pointer_button(time, local.x, local.y, button, pressed, modifiers);
        }
        let _ = (at, button, pressed, time, modifiers);
    }

    fn shell_pointer_axis(
        &mut self,
        at: Point<f64, Logical>,
        dx: f64,
        dy: f64,
        precise: bool,
        time: u32,
    ) {
        let modifiers = self.shell_modifiers();
        #[cfg(feature = "wpe")]
        if let Some(page) = self.shells.iter().find(|page| page.contains(at)) {
            let local = page.local(at);
            page.engine
                .pointer_axis(time, local.x, local.y, dx, dy, precise, modifiers);
        }
        let _ = (at, dx, dy, precise, time, modifiers);
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
        /// reporting a thousand times a second does not ask the shell to lay
        /// the desktop out a thousand times.
        const EVERY: std::time::Duration = std::time::Duration::from_millis(8);

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

        let command = if drag.resize {
            "layout.resize.delta"
        } else {
            "layout.move.delta"
        };
        let args = vec![
            drag.id.to_string(),
            (dx as i32).to_string(),
            (dy as i32).to_string(),
        ];
        let event = viewport_ipc::Event::ShellCommand {
            command: command.to_owned(),
            args,
        };
        self.notify(&event);
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
}
