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
    KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
};
use smithay::input::keyboard::{keysyms, FilterResult, Keysym, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;

use crate::state::ViewportState;
use crate::views::NO_VIEW;

/// A chord the compositor keeps for itself.
///
/// Deliberately tiny. Everything else a compositor binds belongs to the shell,
/// which sends `bind.add` — but these two cannot, because they have to work
/// when the shell is broken or absent. Without them a compositor on a real
/// TTY is inescapable: VT switching needs the compositor to act on the chord,
/// and there is otherwise no way to stop it from the machine it is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Ctrl+Alt+F1 through F12.
    SwitchVt(i32),
    /// Ctrl+Alt+Backspace.
    Quit,
    /// The release half of an intercepted chord.
    Swallow,
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
                            match shortcut(modifiers, keysym) {
                                Some(action) => {
                                    // Remember it so the release is swallowed
                                    // too; a client that saw only the release
                                    // would think the key was stuck.
                                    state.suppressed_keys.push(keysym);
                                    FilterResult::Intercept(Some(action))
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
            Action::Swallow => {}
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
