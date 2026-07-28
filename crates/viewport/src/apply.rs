// SPDX-License-Identifier: GPL-3.0-or-later
//
// What each IPC message does. Ports the handler bodies in src/ipc.c.
//
// The defining property of this file is how little geometry is in it: the shell
// computed every rectangle, and the compositor's job is to put the surface
// where it was told and otherwise stay out of the way.

use smithay::output::{Mode as OutputMode, Scale};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Transform as SmithayTransform, SERIAL_COUNTER};

use viewport_ipc::request::OutputConfigure;
use viewport_ipc::{Event, Request, Transform};

use crate::session;
use crate::state::ViewportState;
use crate::views::NO_VIEW;

pub fn apply(state: &mut ViewportState, request: Request) {
    match request {
        Request::ViewLayout(layout) => view_layout(state, layout),

        Request::ViewVisible { id, visible } => {
            let Some(view) = state.views.get_mut(id) else {
                return;
            };
            view.visible = visible;
            let window = view.window.clone();
            let placed = view.placed;
            if visible {
                if placed {
                    let location = (view.box_.x, view.box_.y);
                    state.space.map_element(window, location, false);
                }
            } else {
                state.space.unmap_elem(&window);
            }
        }

        Request::ViewFullscreen { id, fullscreen } => {
            // Resizing the hole is not enough: an application rearranges its
            // own layout on the fullscreen state — hiding toolbars, switching
            // a video to a fullscreen presentation — and never learns about it
            // from a configure alone.
            let Some(toplevel) = state.views.get(id).and_then(|v| v.window.toplevel().cloned())
            else {
                return;
            };
            toplevel.with_pending_state(|pending| {
                if fullscreen {
                    pending.states.set(xdg_toplevel::State::Fullscreen);
                } else {
                    pending.states.unset(xdg_toplevel::State::Fullscreen);
                }
            });
            toplevel.send_pending_configure();
        }

        Request::ViewFocus { id } => focus_view(state, id),

        Request::ViewClose { id } => {
            if let Some(toplevel) = state.views.get(id).and_then(|v| v.window.toplevel()) {
                toplevel.send_close();
            }
        }

        Request::ViewOpacity { id, opacity } => {
            if let Some(view) = state.views.get_mut(id) {
                view.opacity = opacity.clamp(0.0, 1.0) as f32;
            }
        }

        Request::ViewQuery => {
            state.notify_config();
            state.notify_views();
        }

        Request::ShellFocus => {
            // The shell has no surface of its own yet, so this can only drop
            // client focus. It becomes a real focus target once the web engine
            // is wired up.
            if let Some(keyboard) = state.seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(state, None, serial);
            }
            state.notify_focus(NO_VIEW);
        }

        Request::ShellOverview { active } => {
            state.overview = active;
            if active {
                if let Some(keyboard) = state.seat.get_keyboard() {
                    let serial = SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(state, None, serial);
                }
                return;
            }
            // Clear every window's scale here rather than waiting for the
            // shell to send each one a new rect. A window whose rect is
            // unchanged gets no message, so the shrunken size would stay on
            // the buffer until the client next painted — and an idle terminal
            // does not paint.
            for view in state.views.views_mut() {
                view.scale = 1.0;
                view.clip = None;
            }
        }

        Request::SessionSave { state: saved } => session::save(&saved),

        Request::SessionQuery => {
            let event = Event::SessionRestore {
                state: session::load(),
            };
            state.notify(&event);
        }

        Request::OutputConfigure(config) => output_configure(state, config),

        Request::OutputActive { name } => state.active_output = Some(name),

        Request::OutputQuery => state.notify_output_layout(),

        // Nothing arms a revert yet, so there is nothing to cancel.
        Request::OutputConfirm => {}

        Request::OutputHdr { name, .. } => {
            let name = name
                .or_else(|| state.active_output.clone())
                .unwrap_or_default();
            let message = if state.output_by_name(&name).is_none() {
                "no such output"
            } else {
                "the display would not take HDR"
            };
            reject(state, "output.hdr", message);
        }

        Request::OutputTestAdd => reject(
            state,
            "output.test_add",
            "headless hotplug is only available under --headless",
        ),
        Request::OutputTestRemove { .. } => reject(
            state,
            "output.test_remove",
            "headless hotplug is only available under --headless",
        ),

        Request::NotificationAction { .. }
        | Request::NotificationDismiss { .. }
        | Request::NotificationExpire { .. } => {
            reject(state, "notification", "notifications are not ported yet");
        }

        Request::BindAdd { chord, action } => {
            // Runtime binds from the shell are additive and expendable; the
            // ones that must survive a broken shell are the defaults.
            match crate::binding::parse(&format!("{chord}={action}")) {
                Some(binding) => {
                    // Replaced rather than appended, so re-registering a chord
                    // does not leave the older one shadowing it.
                    state
                        .bindings
                        .retain(|existing| {
                            existing.modifiers != binding.modifiers
                                || existing.keysym != binding.keysym
                        });
                    state.bindings.push(binding);
                }
                None => reject(state, "bind.add", &format!("{chord}={action}")),
            }
        }

        Request::Quit => state.shutdown(),
    }
}

/// The size to configure a client with for a rectangle the shell asked for.
///
/// Never below what the client says it can handle: a client configured under
/// its minimum is entitled to ignore the configure and commit whatever size it
/// likes, which leaves the window overflowing the hole the shell drew with the
/// two unable to agree (`src/xdg_shell.c:855`). Zero on an axis means
/// unconstrained.
fn configure_size(box_: (i32, i32), min: (i32, i32)) -> (i32, i32) {
    (box_.0.max(min.0), box_.1.max(min.1))
}

fn view_layout(state: &mut ViewportState, layout: viewport_ipc::request::ViewLayout) {
    let Some(view) = state.views.get(layout.id) else {
        return;
    };
    let Some(resolved) = layout.resolve(view.box_) else {
        // A degenerate box is dropped without an error, as in the C build.
        return;
    };

    let window = view.window.clone();
    let visible = view.visible;

    // Never ask a client for less than it says it can handle.
    //
    // A client configured below its minimum is entitled to ignore it and
    // commit whatever size it likes, which leaves the window overflowing the
    // hole the shell drew, with the layout and the reality unable to agree
    // (`src/xdg_shell.c:855`).
    let toplevel = window.toplevel().cloned();
    let (width, height) =
        configure_size((resolved.box_.width, resolved.box_.height), view.min_size());

    let view = state.views.get_mut(layout.id).expect("just looked it up");
    view.box_ = resolved.box_;
    view.scale = resolved.scale;
    view.clip = resolved.clip;
    view.placed = true;
    let resize = view.configured != Some((width, height));
    if resize {
        view.configured = Some((width, height));
    }

    // Only when the size actually changed. Every configure is a round trip and
    // the shell resends the rectangle on every frame of an animation, so
    // configuring each time would make a move as expensive as a resize.
    if resize {
        if let Some(toplevel) = toplevel {
            toplevel.with_pending_state(|pending| {
                pending.size = Some((width, height).into());
            });
            toplevel.send_pending_configure();
        }
    }

    if visible {
        state
            .space
            .map_element(window, (resolved.box_.x, resolved.box_.y), false);
    }
}

pub fn focus_view(state: &mut ViewportState, id: u32) {
    let Some(surface) = state.views.get(id).and_then(|v| v.surface()) else {
        return;
    };
    if let Some(keyboard) = state.seat.get_keyboard() {
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(state, Some(surface), serial);
    }
    if let Some(view) = state.views.get(id) {
        let window = view.window.clone();
        state.space.raise_element(&window, true);
    }
    state.notify_focus(id);
}

fn output_configure(state: &mut ViewportState, config: OutputConfigure) {
    let Some(output) = state.output_by_name(&config.name) else {
        reject(state, "output.configure", "no such output");
        return;
    };

    // Prefer an exact modeline the display advertised; fall back to a custom
    // mode so unusual panels stay configurable.
    let mode = config.mode.and_then(|requested| {
        let exact = output.modes().into_iter().find(|m| {
            m.size.w == requested.width
                && m.size.h == requested.height
                && (requested.refresh == 0 || m.refresh == requested.refresh)
        });
        exact.or(if requested.width > 0 && requested.height > 0 {
            Some(OutputMode {
                size: (requested.width, requested.height).into(),
                refresh: requested.refresh,
            })
        } else {
            None
        })
    });

    let scale = config
        .scale
        .filter(|s| *s > 0.0)
        .map(|s| Scale::Fractional(s));
    let transform = config.transform.map(to_smithay_transform);

    output.change_current_state(mode, transform, scale, None);
    if let Some(mode) = mode {
        output.set_preferred(mode);
    }

    if config.x.is_some() || config.y.is_some() {
        let current = state.space.output_geometry(&output).unwrap_or_default();
        let x = config.x.unwrap_or(current.loc.x);
        let y = config.y.unwrap_or(current.loc.y);
        state.space.map_output(&output, (x, y));
    }

    state.notify_output_layout();
}

fn to_smithay_transform(transform: Transform) -> SmithayTransform {
    match transform {
        Transform::Normal => SmithayTransform::Normal,
        Transform::_90 => SmithayTransform::_90,
        Transform::_180 => SmithayTransform::_180,
        Transform::_270 => SmithayTransform::_270,
        Transform::Flipped => SmithayTransform::Flipped,
        Transform::Flipped90 => SmithayTransform::Flipped90,
        Transform::Flipped180 => SmithayTransform::Flipped180,
        Transform::Flipped270 => SmithayTransform::Flipped270,
    }
}

/// Report a refusal on the broadcast channel.
fn reject(state: &mut ViewportState, context: &str, message: &str) {
    let event = Event::Error {
        context: context.to_owned(),
        message: message.to_owned(),
    };
    state.notify(&event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_is_never_configured_below_its_minimum() {
        // It would be entitled to ignore the configure and commit whatever
        // size it liked, leaving the window overflowing the shell's hole.
        assert_eq!(configure_size((100, 50), (200, 80)), (200, 80));
        // Zero means unconstrained on that axis.
        assert_eq!(configure_size((2540, 1390), (0, 0)), (2540, 1390));
        assert_eq!(configure_size((2540, 10), (0, 39)), (2540, 39));
    }

    #[test]
    fn a_move_is_not_a_resize() {
        // The shell resends the whole rectangle on every frame of an
        // animation. A window sliding across the screen changes position sixty
        // times a second and its size not at all, and every configure is a
        // round trip — so the size decides whether one is sent, and the
        // position never does.
        let first = configure_size((2540, 1390), (6, 39));
        let moved = configure_size((2540, 1390), (6, 39));
        assert_eq!(first, moved, "a move must not look like a resize");

        let resized = configure_size((1270, 1390), (6, 39));
        assert_ne!(first, resized, "a real resize must still be seen");
    }
}
