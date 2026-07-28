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
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::input::keyboard::{keysyms, FilterResult, Keysym, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;

use crate::state::ViewportState;
use crate::views::NO_VIEW;

/// Run a command, detached.
///
/// Double-forked through a shell so the compositor does not accumulate
/// zombies and a launched application outlives the key that started it.
pub fn spawn(command: &str) {
    use std::process::{Command, Stdio};

    tracing::info!("exec: {command}");
    let result = Command::new("/bin/sh")
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
    /// The release half of an intercepted chord.
    Swallow,
    /// A binding fired.
    Bound(crate::binding::Action),
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

    None
}

impl ViewportState {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // Anything at all counts. Device added and removed do not — they
        // arrive when a dock is plugged in with nobody at the machine — but
        // they are filtered out before this.
        if self.idle.activity() {
            // The screens were off. Bring them back through the same path the
            // deadline turned them off by.
            self.set_outputs_enabled(true);
        }

        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };
                let pressed = event.state() == smithay::backend::input::KeyState::Pressed;

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
                            if let Some(action) = shortcut(modifiers, keysym) {
                                // VT switching still works while locked — it
                                // is the session's, not this seat's, and a
                                // locked session on another VT is still
                                // locked. Quitting does not: it would drop the
                                // lock along with the compositor. Forwarded
                                // rather than swallowed, so Ctrl+Alt+Backspace
                                // still reaches the lock screen as keys.
                                if state.locked
                                    && !matches!(action, Action::SwitchVt(_))
                                {
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
                            ) {
                                Some(bound) => {
                                    state.suppressed_keys.push(keysym);
                                    FilterResult::Intercept(Some(Action::Bound(
                                        bound.clone(),
                                    )))
                                }
                                None => FilterResult::Forward,
                            }
                        } else if let Some(at) =
                            state.suppressed_keys.iter().position(|k| *k == keysym)
                        {
                            state.suppressed_keys.remove(at);
                            FilterResult::Intercept(Some(Action::Swallow))
                        } else {
                            FilterResult::Forward
                        }
                    },
                );

                if let Some(action) = action.flatten() {
                    self.handle_action(action);
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
                let outputs: Vec<_> = self
                    .space
                    .outputs()
                    .filter_map(|o| self.space.output_geometry(o))
                    .collect();
                let pos = crate::cursor::clamp(&outputs, from, from + event.delta());

                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(pos);
                pointer.motion(
                    self,
                    under.clone(),
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.relative_motion(
                    self,
                    under,
                    &smithay::input::pointer::RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );
                pointer.frame(self);
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
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                let serial = SERIAL_COUNTER.next_serial();
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let under = self.surface_under(pos);

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

                if state == ButtonState::Pressed && !pointer.is_grabbed() {
                    let hit = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(w, _)| w.clone());

                    match hit.filter(|_| !self.overview) {
                        Some(window) => {
                            let id = self
                                .views
                                .iter()
                                .find(|v| v.window == window)
                                .map(|v| v.id)
                                .unwrap_or(NO_VIEW);

                            self.space.raise_element(&window, true);
                            if let Some(toplevel) = window.toplevel() {
                                keyboard.set_focus(
                                    self,
                                    Some(toplevel.wl_surface().clone()),
                                    serial,
                                );
                            }
                            self.send_pending_configures();
                            if id != self.focused {
                                self.notify_focus(id);
                            }
                        }
                        None => {
                            // The pointer is over the shell, or the overview is
                            // up and every click belongs to it.
                            for window in self.space.elements() {
                                window.set_activated(false);
                            }
                            self.send_pending_configures();
                            keyboard.set_focus(self, Option::<WlSurface>::None, serial);
                            if self.focused != NO_VIEW {
                                self.notify_focus(NO_VIEW);
                            }
                        }
                    }
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
                pointer.axis(self, frame);
                pointer.frame(self);
            }

            _ => {}
        }
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
                if let Some(toplevel) = self
                    .views
                    .get(self.focused)
                    .and_then(|view| view.window.toplevel())
                {
                    toplevel.send_close();
                }
            }
            Bound::Reload => {
                #[cfg(feature = "wpe")]
                if let Some(shell) = self.shell.as_ref() {
                    shell.view.reload();
                }
            }
            Bound::Focus(target) => self.focus_direction(&target),
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

    fn send_pending_configures(&mut self) {
        for window in self.space.elements() {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_pending_configure();
            }
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
        let event = viewport_ipc::Event::ShellCommand {
            command: "output.focus".to_owned(),
            args: vec![target.to_owned()],
        };
        self.notify(&event);
    }
}
