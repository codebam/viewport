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
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;

use crate::state::ViewportState;
use crate::views::NO_VIEW;

impl ViewportState {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };
                // Compositor keybindings are not ported yet, so everything is
                // forwarded to the focused client.
                keyboard.input::<(), _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |_, _, _| FilterResult::Forward,
                );
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

    fn send_pending_configures(&mut self) {
        for window in self.space.elements() {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_pending_configure();
            }
        }
    }
}
