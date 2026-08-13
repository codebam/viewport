// SPDX-License-Identifier: GPL-3.0-or-later
//
// What each IPC message does. Ports the handler bodies in src/ipc.c.
//
// The defining property of this file is how little geometry is in it: the shell
// computed every rectangle, and the compositor's job is to put the surface
// where it was told and otherwise stay out of the way.

use smithay::output::{Mode as OutputMode, Scale};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
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
                    // Mapping stacks on top, which is wrong for a tiled window
                    // coming back to a desktop that has a float over it.
                    state.restack();
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
            let Some(toplevel) = state
                .views
                .get(id)
                .and_then(|v| v.window.toplevel().cloned())
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
            // The outside list carries the state a taskbar draws from, and
            // fullscreen is one of the two it knows about.
            let activated = state.focused == id;
            state
                .foreign_management_state
                .set_state(id, activated, fullscreen);
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

        Request::BackgroundFocus => {
            state.toggle_background_focus();
        }

        Request::ShellFocus => {
            // With the engine in this process the shell has no surface, so
            // this can only drop client focus and let the key path forward to
            // WebKit. Out of process it is a surface like any other, and
            // focusing it is what makes the keys arrive.
            let target = state.shell_client_surface().cloned().map(Into::into);
            if let Some(keyboard) = state.seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(state, target, serial);
            }
            // Keys stopping at the shell is only half of it, and the other
            // half — no window still drawn as the focused one — is what
            // `notify_focus` does with `NO_VIEW`.
            state.notify_focus(NO_VIEW);
        }

        Request::ScreencastRect {
            x,
            y,
            width,
            height,
        } => {
            // The older, single-rectangle form of `shell.overlay`. A zero size
            // is the shell saying there is nothing above the windows now.
            let origin = state.dispatch_origin;
            let rects = if width > 0 && height > 0 {
                {
                    vec![smithay::utils::Rectangle::new(
                        (x + origin.x, y + origin.y).into(),
                        (width, height).into(),
                    )]
                }
            } else {
                Default::default()
            };
            // The chooser is a dialog: it is there to be clicked.
            state.set_shell_overlays(rects.clone(), rects);
        }

        Request::ShellOverlay { rects } => {
            // The sender's page coordinates, as in `view_layout`: these are
            // rectangles of the shell's own document, and the compositor holds
            // them in the layout's coordinates because it hit-tests the pointer
            // against them.
            let origin = state.dispatch_origin;
            let placed: Vec<_> = rects
                .into_iter()
                .filter(|rect| rect.width > 0 && rect.height > 0)
                .map(|rect| {
                    (
                        smithay::utils::Rectangle::new(
                            (rect.x + origin.x, rect.y + origin.y).into(),
                            (rect.width, rect.height).into(),
                        ),
                        rect.passthrough,
                    )
                })
                .collect();
            // Everything the shell floats is drawn above the windows; only the
            // ones that did not ask to be seen through take the pointer.
            let hits = placed
                .iter()
                .filter(|(_, passthrough)| !passthrough)
                .map(|(rect, _)| *rect)
                .collect();
            let all = placed.into_iter().map(|(rect, _)| rect).collect();
            state.set_shell_overlays(all, hits);
        }

        Request::ShellOverview { active } => {
            state.overview = active;
            // Every click belongs to the shell while the overview is up and to
            // the windows again when it is down, and neither transition
            // involves the pointer moving. Same reason as `set_shell_overlays`.
            state.refresh_pointer_focus();
            if active {
                // To the desktop page where the shell is a client: the overview
                // is drawn by the shell and driven from the keyboard, so keys
                // have to reach it. Clearing the focus outright is right only
                // for the in-process engine, which is not a client and is
                // handed keys by `Action::Web` precisely because the focus is
                // empty.
                if !state.focus_shell_at(None) {
                    if let Some(keyboard) = state.seat.get_keyboard() {
                        let serial = SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(state, None, serial);
                    }
                }
                // The overview owns the keys, so no window is focused while it
                // is up and none should be drawn as though it were.
                state.activate_view(NO_VIEW);
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
                view.frame = None;
            }
        }

        Request::SessionSave { state: saved } => session::save(&saved),

        Request::SessionQuery => {
            let event = Event::SessionRestore {
                state: session::load(),
            };
            state.notify(&event);
        }

        Request::WorkspaceList { workspaces } => {
            // The shell's list, whole. Publishing it is the compositor's only
            // part in workspaces; see `crate::workspace`.
            tracing::info!(
                "workspaces: {} from the shell{}",
                workspaces.len(),
                workspaces
                    .iter()
                    .map(|w| format!(
                        " [{} {:?}{}{}]",
                        w.id,
                        w.name,
                        w.output
                            .as_deref()
                            .map(|o| format!(" on {o}"))
                            .unwrap_or_default(),
                        if w.active { " active" } else { "" }
                    ))
                    .collect::<String>()
            );
            let outputs: Vec<_> = state.space.outputs().cloned().collect();
            let dh = state.display_handle.clone();
            state
                .workspace_state
                .set::<ViewportState>(&dh, &outputs, workspaces);
        }
        Request::OutputConfigure(config) => output_configure(state, config),

        Request::OutputActive { name } => state.active_output = Some(name),

        // Straight back out as the event a keybinding produces. No validation:
        // the shell is the only thing that knows its own verbs, and it already
        // warns about the ones it does not recognise.
        Request::ShellCommand { command, args } => {
            state.notify(&viewport_ipc::event::Event::ShellCommand { command, args });
        }

        // Run a shell command on the host — the bridge the bar's widgets use
        // to open things or drive the sink through wpctl. Same spawn path a
        // keybinding's `exec` uses; the shell composes the exact line.
        Request::ShellExec { command } => {
            crate::input::spawn(&command);
        }

        // Re-sample the status bar now. An audio widget drives the sink through
        // wpctl (above) and then asks for this, so the change shows up at once
        // instead of on the next two-second tick.
        Request::StatusRefresh => state.status_tick(),

        Request::OutputQuery => state.notify_output_layout(),

        // Nothing arms a revert yet, so there is nothing to cancel.
        Request::OutputConfirm => {}

        Request::OutputHdr { name, enabled } => {
            let name = name
                .or_else(|| state.active_output.clone())
                .unwrap_or_default();
            if state.output_by_name(&name).is_none() {
                reject(state, "output.hdr", "no such output");
                return;
            }
            // Absent toggles, which is what a keybinding wants and what a
            // settings panel does not (`src/ipc.c:1321`).
            let want = enabled.unwrap_or_else(|| !state.hdr_enabled(&name));
            if let Err(e) = state.set_hdr(&name, want) {
                reject(state, "output.hdr", &format!("{e:#}"));
            }
        }

        // Both go through the same path a real hotplug takes — map or unmap in
        // the `Space`, then tell the shell — so what the tests exercise is the
        // code a monitor being plugged in runs, not a simulation of it.
        Request::OutputTestAdd => match crate::headless::add(state) {
            Some(_) => state.notify_output_layout(),
            None => reject(
                state,
                "output.test_add",
                "headless hotplug is only available under --headless",
            ),
        },
        Request::OutputTestRemove { name } => {
            if crate::headless::remove(state, name.as_deref()) {
                state.notify_output_layout();
            } else {
                reject(
                    state,
                    "output.test_remove",
                    // Two different noes, deliberately not distinguished: a
                    // caller that gets this back cannot act differently on
                    // "wrong backend" than on "no such output".
                    "no such headless output",
                );
            }
        }

        // The shell drew the notification, so the shell is what knows the user
        // pressed a button or dismissed it. All three end the notification;
        // they differ in what the sender is told, and a sender does act on
        // that — one that sees Dismissed knows the user saw it.
        Request::NotificationAction { id, action } => {
            if let Some(action) = action.as_deref() {
                state.notifications.invoke_action(id, action);
            }
            state
                .notifications
                .closed(id, crate::notification::CloseReason::Dismissed);
        }
        Request::NotificationDismiss { id } => {
            state
                .notifications
                .closed(id, crate::notification::CloseReason::Dismissed);
        }
        Request::NotificationExpire { id } => {
            state
                .notifications
                .closed(id, crate::notification::CloseReason::Expired);
        }

        Request::BindAdd { chord, action } => {
            // Runtime binds from the shell are additive and expendable; the
            // ones that must survive a broken shell are the defaults.
            match crate::binding::parse(&format!("{chord}={action}")) {
                Some(binding) => {
                    // Replaced rather than appended, so re-registering a chord
                    // does not leave the older one shadowing it.
                    state.bindings.retain(|existing| {
                        existing.modifiers != binding.modifiers || existing.keysym != binding.keysym
                    });
                    state.bindings.push(binding);
                }
                None => reject(state, "bind.add", &format!("{chord}={action}")),
            }
        }

        // Driving the pointer from the socket. The same three calls the
        // libinput path makes, in the same order, so a scripted click and a
        // real one are the same event by the time anything sees it.
        Request::InputPointer { x, y } => {
            let Some(pointer) = state.seat.get_pointer() else {
                return;
            };
            let location = (x, y).into();
            let under = state.surface_under(location);
            let serial = SERIAL_COUNTER.next_serial();
            let time = state.start_time.elapsed().as_millis() as u32;
            pointer.motion(
                state,
                under,
                &smithay::input::pointer::MotionEvent {
                    location,
                    serial,
                    time,
                },
            );
            pointer.frame(state);
            // The cursor moved and nothing else would draw it.
            state.needs_render = true;
            // And a scripted pointer is a pointer being used, so the hide
            // deadline starts again — otherwise a test that drives the mouse
            // from the socket watches its cursor disappear underneath it.
            state.cursor_activity();
        }

        Request::InputButton { button, pressed } => {
            let Some(pointer) = state.seat.get_pointer() else {
                return;
            };
            // Who is about to be sent this, which is the question whenever a
            // click appears to do nothing: the pointer delivers to its current
            // focus, and "the shell" and "the window under the shell" are easy
            // to confuse from outside.
            if tracing::enabled!(tracing::Level::DEBUG) {
                let focus = pointer.current_focus();
                let shell = state.shell_client_surface().cloned();
                tracing::debug!(
                    "button {button} {} -> focus {:?}, shell surface {:?}, same: {}",
                    if pressed { "press" } else { "release" },
                    focus.as_ref().map(Resource::id),
                    shell.as_ref().map(Resource::id),
                    focus
                        .as_ref()
                        .map(|f| Some(f) == shell.as_ref())
                        .unwrap_or(false),
                );
            }
            let serial = SERIAL_COUNTER.next_serial();
            let time = state.start_time.elapsed().as_millis() as u32;
            pointer.button(
                state,
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
            pointer.frame(state);
            state.cursor_activity();
        }

        Request::InputKey { keycode, pressed } => state.inject_key(keycode, pressed),

        Request::ConfigGaps {
            inner,
            outer,
            smart,
        } => {
            // Only the fields given change; the others keep whatever they are.
            // This is how a keybinding on `inner` does not clobber an `outer`
            // set from the config file or an earlier IPC call.
            let current = state
                .config
                .gaps
                .get_or_insert_with(viewport_ipc::event::Gaps::default);
            let mut changed = false;
            if let Some(v) = inner {
                if v < 0 {
                    reject(state, "config.gaps", &format!("inner {v}"));
                    return;
                }
                current.inner = Some(v);
                changed = true;
            }
            if let Some(v) = outer {
                if v < 0 {
                    reject(state, "config.gaps", &format!("outer {v}"));
                    return;
                }
                current.outer = Some(v);
                changed = true;
            }
            if let Some(v) = smart {
                current.smart = Some(v);
                changed = true;
            }
            if changed {
                // The shell only reads the gaps through the CSS custom
                // properties a Config event carries, so changing the value is
                // a matter of updating the compositor's copy and re-announcing
                // it — the same path a config-file reload takes, without the
                // disk write.
                state.needs_render = true;
                state.notify_config();
            }
        }

        Request::ConfigBorder {
            radius,
            width,
            smart,
        } => {
            let current = state
                .config
                .border
                .get_or_insert_with(viewport_ipc::event::Border::default);
            let mut changed = false;
            if let Some(v) = radius {
                if v < 0 {
                    reject(state, "config.border", &format!("radius {v}"));
                    return;
                }
                current.radius = Some(v);
                changed = true;
            }
            if let Some(v) = width {
                if v < 0 {
                    reject(state, "config.border", &format!("width {v}"));
                    return;
                }
                current.width = Some(v);
                changed = true;
            }
            if let Some(v) = smart {
                current.smart = Some(v);
                changed = true;
            }
            if changed {
                // Both sides of the desktop read this one: the shell rounds
                // the frame it draws from the Config event, and the renderer
                // crops the client to the same corner out of `state.config`.
                // So a redraw is not a courtesy here — without it the windows
                // keep the corners they were last cropped to.
                state.needs_render = true;
                state.notify_config();
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

fn view_layout(state: &mut ViewportState, mut layout: viewport_ipc::request::ViewLayout) {
    // Out of the page's coordinates and into the layout's, before anything is
    // resolved against a rectangle that is already in the layout's.
    //
    // See `ViewportState::dispatch_origin`. A desktop confined to the second
    // monitor lays out from (0, 0), because that is where its document starts,
    // and a window placed at those numbers lands on the *first* monitor — while
    // the frame the shell drew for it, being part of the page, is offset
    // correctly and appears on the second. A border with no window in it, and a
    // window where nothing asked for one.
    //
    // Positions only: a width is a width wherever the page begins.
    let origin = state.dispatch_origin;
    let asked = (layout.box_.x, layout.box_.y);
    if origin.x != 0 || origin.y != 0 {
        layout.box_.x = layout.box_.x.map(|x| x + origin.x);
        layout.box_.y = layout.box_.y.map(|y| y + origin.y);
        if let Some(clip) = layout.clip.as_mut() {
            clip.x = clip.x.map(|x| x + origin.x);
            clip.y = clip.y.map(|y| y + origin.y);
        }
        if let Some(frame) = layout.frame.as_mut() {
            frame.x += origin.x;
            frame.y += origin.y;
        }
    }

    let Some(view) = state.views.get(layout.id) else {
        return;
    };
    let Some(resolved) = layout.resolve(view.box_) else {
        // A degenerate box is dropped without an error, as in the C build.
        return;
    };

    // Where the window ended up, in the layout's coordinates, and where the
    // page asked for it. The first question whenever a window is on the wrong
    // screen, and the two differ by exactly the page's origin.
    tracing::debug!(
        "view {}: placed at {}x{}{:+}{:+}; the page asked for {:?},{:?} from {:+}{:+}",
        layout.id,
        resolved.box_.width,
        resolved.box_.height,
        resolved.box_.x,
        resolved.box_.y,
        asked.0,
        asked.1,
        origin.x,
        origin.y
    );

    let window = view.window.clone();

    // Never ask a client for less than it says it can handle.
    //
    // A client configured below its minimum is entitled to ignore it and
    // commit whatever size it likes, which leaves the window overflowing the
    // hole the shell drew, with the layout and the reality unable to agree
    // (`src/xdg_shell.c:855`).
    let toplevel = window.toplevel().cloned();
    let (width, height) =
        configure_size((resolved.box_.width, resolved.box_.height), view.min_size());

    state.last_layout = Some(std::time::Instant::now());
    let view = state.views.get_mut(layout.id).expect("just looked it up");
    view.box_ = resolved.box_;
    view.scale = resolved.scale;
    view.clip = resolved.clip;
    // Absent means there is nothing of this window's frame that has to be
    // drawn above anything, which is every tiled window.
    view.frame = layout.frame;
    view.floating = layout.floating;
    // Smart radius: the shell drew this one square, so the compositor must not
    // cut a corner off the client that the frame around it no longer has.
    view.square = layout.square;
    view.placed = true;
    // A rectangle un-hides a window, as in C (`src/xdg_shell.c:832`).
    //
    // The shell hides a window by sending `view.visible false` — that is what
    // a workspace switch is — and shows it again by laying it out, without
    // sending `visible` a second time. Treating those as independent left a
    // window hidden for the rest of the session: the shell kept drawing its
    // frame, because as far as the shell is concerned it is on screen, and
    // the compositor drew nothing inside it.
    view.visible = true;
    let resize = view.configured != Some((width, height));
    if resize {
        view.configured = Some((width, height));
    }

    // And say so when that is not the size the shell asked for.
    //
    // The clamp above is right — a client configured below its minimum is
    // entitled to ignore it, so asking is worse than not — but it was silent,
    // and a silent clamp leaves the shell holding a rectangle for a window
    // that is a different size. Every sum built on that rectangle is then
    // wrong by the difference: centring a dialog on its parent is out by half
    // of it, which is what this was found by.
    //
    // Only on a change, so a shell resending the same rectangle on every frame
    // of an animation is told once and not sixty times. The shell adopting the
    // size settles it: the next `view.layout` asks for what the client already
    // has, the clamp does nothing, and nothing more is sent.
    let mismatch = (width, height) != (resolved.box_.width, resolved.box_.height);
    let announce = resize && mismatch;
    let id = layout.id;

    if announce {
        state.notify(&viewport_ipc::Event::ViewConfigured { id, width, height });
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

    // An X client is told its whole rectangle, position included: X has no
    // separate notion of "the compositor placed you", so a window that is
    // moved and not reconfigured believes it is still where it was and draws
    // its menus there.
    if let Some(x11) = window.x11_surface() {
        let rect = smithay::utils::Rectangle::new(
            (resolved.box_.x, resolved.box_.y).into(),
            (width, height).into(),
        );
        if let Err(e) = x11.configure(rect) {
            tracing::warn!("could not configure an X11 window: {e}");
        }
    }

    state
        .space
        .map_element(window, (resolved.box_.x, resolved.box_.y), false);

    // Mapping puts a window on top of the stack, and the shell lays every
    // window out on every frame of an animation — so the last one it happened
    // to send ended up in front, which is a floating window disappearing
    // behind the tiled ones it is supposed to sit over. Stacking follows
    // focus, as it does in C (`src/xdg_shell.c:940`), so the focused window
    // goes back on top.
    if state.focused != layout.id {
        if let Some(window) = state
            .views
            .get(state.focused)
            .map(|view| view.window.clone())
        {
            state.space.raise_element(&window, false);
        }
    }

    // And whatever the focused window is, the floats stay over it.
    state.restack();

    // A window that just crossed onto another monitor is being shown in a
    // different colour space than the one it was drawing for. Nothing else
    // notices: the outputs themselves did not change, so `notify_output_colour`
    // never runs, and a client that asked once would keep the answer it got on
    // the screen it started on.
    state.notify_surface_colour();
}

pub fn focus_view(state: &mut ViewportState, id: u32) {
    // By window, not by surface: an X11 window's surface would take the
    // keyboard on the Wayland side while the X server went on believing
    // nothing was focused.
    let Some(focus) = state
        .views
        .get(id)
        .and_then(|view| crate::keyboard_focus::KeyboardFocus::for_window(&view.window))
    else {
        return;
    };
    if let Some(keyboard) = state.seat.get_keyboard() {
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(state, Some(focus), serial);
    }
    if let Some(view) = state.views.get(id) {
        let window = view.window.clone();
        // Raised without activating: `notify_focus` owns that, because the
        // state Smithay sets here only reaches the client on a configure and a
        // raise sends none.
        state.space.raise_element(&window, false);
    }
    state.restack();
    state.notify_focus(id);
}

fn output_configure(state: &mut ViewportState, config: OutputConfigure) {
    // Including one that is already off, or it can never be turned back on:
    // a disabled output is unmapped from the space, and `output_by_name` only
    // sees what is mapped.
    let Some(output) = state.any_output_by_name(&config.name) else {
        reject(state, "output.configure", "no such output");
        return;
    };

    // Turning a screen off, which this request has documented since it was
    // written and never did.
    //
    // `enabled` is parsed, listed in docs/ipc.md, and was read by nothing —
    // `grep -c config.enabled` came back zero. So asking the compositor to
    // switch a monitor off over its own control socket did nothing at all and
    // reported success. The wlr-output-management path has always honoured it,
    // which is why this went unnoticed: every tool that turns a screen off
    // uses that protocol, and the one place the compositor offers the same
    // thing itself quietly ignored it.
    //
    // The last one on is refused, matching `apply_output_configuration`. A
    // desktop with every output disabled is not a state anything can be
    // recovered from by pointing at a screen.
    if let Some(enabled) = config.enabled {
        if !enabled && state.space.outputs().count() <= 1 {
            reject(
                state,
                "output.configure",
                "refusing to turn off the only output left on",
            );
            return;
        }
        state.set_output_enabled(&output, enabled);
        if !enabled {
            // Nothing below applies to a screen that is off, and a mode or a
            // position for one is not an error worth reporting either.
            return;
        }
    }

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

    let scale = config.scale.filter(|s| *s > 0.0).map(Scale::Fractional);
    let transform = config.transform.map(to_smithay_transform);
    // Said out loud, because a rotated output has three sizes that must agree
    // and only one of them is visible from any given place: the mode the panel
    // is driven at, the logical rectangle the layout gives it, and the page the
    // shell draws that rectangle into. A picture that comes out wrong on
    // rotation is one of the three not having moved.
    if let Some(transform) = transform {
        tracing::info!("{}: transform {transform:?}", output.name());
    }

    output.change_current_state(mode, transform, scale, None);
    if let Some(mode) = mode {
        output.set_preferred(mode);
    }

    // The layer map holds the output's shape from when it was last arranged,
    // and everything reserved against it — a bar's exclusive zone, and the
    // area left over for windows. A mode change or a rotation makes all of
    // that the wrong shape, and nothing else recomputes it.
    //
    // Rotating a monitor without this left the usable area landscape on a
    // portrait screen: the shell was told the output was 1440x2560 and that
    // windows could use 2560x1440 of it, so it laid out a desktop wider than
    // the screen and half of it fell off the side.
    if mode.is_some() || transform.is_some() || scale.is_some() {
        let current = state.space.output_geometry(&output).unwrap_or_default();
        let x = config.x.unwrap_or(current.loc.x);
        let y = config.y.unwrap_or(current.loc.y);
        state.map_output_at(&output, (x, y));
        state.output_reshaped(&output);
    } else if config.x.is_some() || config.y.is_some() {
        let current = state.space.output_geometry(&output).unwrap_or_default();
        let x = config.x.unwrap_or(current.loc.x);
        let y = config.y.unwrap_or(current.loc.y);
        state.map_output_at(&output, (x, y));
    }

    // Where this monitor goes when it is plugged in again. Asked for rather
    // than drifted into, which is the only kind of arrangement worth restoring.
    state.remember_output(&output);
    state.notify_output_layout();
}

/// What the shell is told an output is turned to.
///
/// It was `Normal` unconditionally, which is a rotated monitor described to
/// the shell as though it were not.
pub fn from_smithay_transform(transform: SmithayTransform) -> Transform {
    match transform {
        SmithayTransform::Normal => Transform::Normal,
        SmithayTransform::_90 => Transform::_90,
        SmithayTransform::_180 => Transform::_180,
        SmithayTransform::_270 => Transform::_270,
        SmithayTransform::Flipped => Transform::Flipped,
        SmithayTransform::Flipped90 => Transform::Flipped90,
        SmithayTransform::Flipped180 => Transform::Flipped180,
        SmithayTransform::Flipped270 => Transform::Flipped270,
    }
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
