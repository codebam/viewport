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
            state.ipc.broadcast(&event);
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

        Request::BindAdd { chord, .. } => {
            reject(state, "bind.add", &format!("{chord}: bindings are not ported yet"));
        }

        Request::Quit => state.loop_signal.stop(),
    }
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

    let view = state.views.get_mut(layout.id).expect("just looked it up");
    view.box_ = resolved.box_;
    view.scale = resolved.scale;
    view.clip = resolved.clip;
    view.placed = true;

    if visible {
        state
            .space
            .map_element(window, (resolved.box_.x, resolved.box_.y), false);
    }
}

fn focus_view(state: &mut ViewportState, id: u32) {
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
    state.ipc.broadcast(&event);
}
