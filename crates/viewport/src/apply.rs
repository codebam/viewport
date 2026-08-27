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

/// Whether carrying this request out would move the keyboard.
///
/// The three that set focus outright. Everything else either does not touch the
/// seat or does so as a consequence of a window appearing, which the lock
/// screen's own surface is drawn over anyway.
fn moves_focus(request: &Request) -> bool {
    matches!(
        request,
        Request::ViewFocus { .. } | Request::BackgroundFocus | Request::ShellFocus
    )
}

/// Whether carrying this request out would *act* — on the host, as the user,
/// or on the session itself.
///
/// Moving the keyboard is one way to act on a locked session and not the only
/// one. `shell.exec` runs a line on the host; the three `input.*` requests
/// press keys and click as though a hand had, which behind a lock screen means
/// into whatever box the locker drew; `bind.add` installs a chord that the
/// key path will refuse while locked but that survives past the unlock; and
/// quitting takes the lock screen down with the compositor — a frozen session
/// is logind's to recover, not the socket's.
fn acts_while_locked(request: &Request) -> bool {
    matches!(
        request,
        Request::ShellExec { .. }
            | Request::InputKey { .. }
            | Request::InputButton { .. }
            | Request::InputPointer { .. }
            | Request::BindAdd { .. }
            | Request::AiLogin { .. }
            | Request::Quit
    )
}

pub fn apply(state: &mut ViewportState, request: Request) {
    // Everything except another layout pays off what the last run of layouts
    // left owing, first.
    //
    // `view.layout` is the only thing that defers work — see the end of
    // `view_layout` — and messages are handled one at a time, so a request that
    // reads the stack (`input.pointer` hit-tests against it, `view.list`
    // reports it) is the only way anything inside this dispatch could see it
    // stale. Flushing here rules that out while still letting a run of layouts,
    // which is what an animation frame is, coalesce into one restack.
    if !matches!(request, Request::ViewLayout(_)) {
        state.settle();
    }

    // Nothing moves the keyboard while the session is locked.
    //
    // The key path already refuses to run a binding then (`input.rs`), for the
    // reason that a chord which spawned a terminal would put one on top of the
    // lock screen. This is the same rule for the same reason, and the theft is
    // worse here: focus taken off the lock screen is the password being typed
    // into whatever took it, and every one of these requests is a line on a
    // socket the shell is not the only thing that can reach.
    if state.locked && moves_focus(&request) {
        tracing::debug!("ignoring {request:?} while the session is locked");
        return;
    }
    // The same rule where the stakes are not the keyboard but anything at all:
    // these do not need the focus to matter.
    if state.locked && acts_while_locked(&request) {
        tracing::warn!("refusing {request:?} while the session is locked");
        return;
    }

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
                // The renderer draws from the space, and nothing else in this
                // arm changes a pixel: without marking it, the closed
                // window's last frame stayed on screen until something else
                // happened to cause a redraw.
                state.needs_render = true;
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

        Request::ViewCapture { id, capture } => {
            let Some(view) = state.views.get_mut(id) else {
                return;
            };
            tracing::debug!("view {id}: capture policy set to {capture}");
            if view.capture_allowed != capture {
                view.capture_allowed = capture;
                // Capture is serviced from a render pass. Wake an idle backend
                // so the changed policy reaches any frame waiting on it.
                state.needs_render = true;
            }
        }

        Request::ViewQuery => {
            state.notify_config();
            state.notify_views();
            state.ai_usage.replay();
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
                        // Saturating, as every translation of a wire
                        // coordinate below: serde accepts up to i32::MAX, an
                        // origin for a second monitor is real pixels, and
                        // the sum of the two overflowing is a window on the
                        // wrong screen — the very fault the origin exists to
                        // prevent — or a panic, depending on the build.
                        (x.saturating_add(origin.x), y.saturating_add(origin.y)).into(),
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
                            // Saturating, as in `screencast.rect` above.
                            (
                                rect.x.saturating_add(origin.x),
                                rect.y.saturating_add(origin.y),
                            )
                                .into(),
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

            // A page asking for the saved layout is a page that has just
            // started — a restart after a crash, a reload, the first load of
            // the session. Whatever the last page had drawn is therefore not
            // on screen, and this one has drawn nothing yet.
            //
            // So the lock screen is withdrawn and asked for again, in that
            // order. Withdrawn, because the flag is per lock and not per page
            // and a restart inside one lock would otherwise inherit it — the
            // compositor would draw the new page's first frames, which are the
            // desktop coming up, over a locked session. Asked for again,
            // because the page that was told to lock is gone and the one that
            // replaced it has no idea the session is locked; without this it
            // draws the desktop and waits to be clicked.
            if state.locked && state.lock_mode.is_built_in() {
                state.forget_lock_screen();
                let generation = state.lock_generation;
                let can_authenticate = state.authenticator.online();
                tracing::info!(
                    "lock: telling a page that has just started to draw lock {generation}"
                );
                state.notify(&Event::SessionLock {
                    generation,
                    can_authenticate,
                });
                state.focus_lock_shell();
            }
        }

        // The one thing the `lock` binding, the idle deadline and the lid all
        // do. Nothing here decides what locking means; `lock_session` does,
        // once, from the config.
        Request::SessionLock => state.lock_session(),

        Request::SessionLockDrawn { generation } => state.lock_screen_drawn(generation),

        Request::SessionUnlock {
            generation,
            password,
        } => state.try_unlock(generation, password),

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

        // Re-sample the status bar now, for anything that changed something it
        // reports and does not want to wait for the next two-second tick.
        Request::StatusRefresh => state.status_tick(),

        Request::StatusVolume {
            target,
            delta,
            mute,
        } => {
            // The node names wpctl knows. Refused rather than passed through:
            // this is a string from a page, and `wpctl` takes an id where it
            // does not recognise a name.
            let node = match target.as_str() {
                "sink" => crate::status::SINK,
                "source" => crate::status::SOURCE,
                other => {
                    reject(state, "status.volume", &format!("no such target {other:?}"));
                    return;
                }
            };

            // Changed and then sampled, in that order, and neither on this
            // thread: the worker runs `wpctl` and measures what it left, and
            // the answer comes back through the status channel, which reports
            // it to the shell the moment the audio state differs. The
            // compositor waiting for two forks per scroll was a stall the
            // scroll wheel could outrun.
            //
            // The reply is true only where there is no worker to ask, and then
            // the change has already been made in line.
            if state.status.set_audio(node, delta, mute) {
                state.status_tick();
            }
        }

        Request::OutputQuery => state.notify_output_layout(),

        // Somebody can see the screen they just changed, so the countdown
        // `output_configure` started can lapse. See
        // `ViewportState::arm_output_revert`.
        Request::OutputConfirm => state.confirm_output_revert(),

        // And the other answer: put it back now rather than in what is left of
        // the twelve seconds. Nothing pending is not a refusal — the deadline
        // may have fired a moment before the click, and the desk is then in
        // exactly the state the click asked for.
        Request::OutputRevert => state.output_revert_tick(),

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
            // Acted on is finished with. A message whose button has been
            // pressed — a mail opened, an update started — is not something
            // to go back to later, which is what the centre is for.
            //
            // Dismissing and expiring do not do this, deliberately: both mean
            // the popup went, and the popup going is the reason a centre
            // exists at all.
            if state.notification_history.forget(id) {
                state.publish_notification_history();
            }
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

        // The centre. Kept by the compositor rather than by the page, because
        // the page is restarted when it crashes and reloaded when its
        // stylesheet changes, and a history that lived there would be lost by
        // both — see `crate::notification::History`.
        Request::NotificationList => {
            state.publish_notification_history();
        }

        Request::NotificationForget { id } => {
            // Forgetting is not closing, so no sender is told: it was already
            // told when the popup went, and being tidied out of a list it
            // cannot see is not an event it has any use for.
            let changed = match id {
                Some(id) => state.notification_history.forget(id),
                None => state.notification_history.clear(),
            };
            if changed {
                state.publish_notification_history();
            }
        }

        // The shell draws the tray, so the shell is what knows which icon was
        // hit and where it sits. Both go straight out to the application that
        // owns the item, on the tray's own thread — an application that has
        // stopped answering the bus must not stall the compositor.
        Request::TrayActivate { id, button, x, y } => match button.as_str() {
            "" | "primary" | "secondary" | "menu" => state.tray.activate(id, button, x, y),
            other => reject(state, "tray.activate", &format!("no such button {other:?}")),
        },
        // The clipboard history: what is in it, putting one back, and
        // forgetting. Everything the compositor already sees — it brokers
        // every selection on the session — so none of this is a second daemon
        // holding a data-control connection open.
        Request::ClipboardQuery => state.notify_clipboard(),
        Request::ClipboardPaste { id } => {
            let Some(_text) = state.clipboard.take(id) else {
                // Gone: the history was cleared, or the limit dropped, while
                // the picker was open. Not worth refusing over — the picker is
                // told what the history is now.
                state.notify_clipboard();
                return;
            };
            state.offer_clipboard();
            state.notify_clipboard();
        }
        Request::ClipboardForget { id } => {
            match id {
                Some(id) => {
                    state.clipboard.remove(id);
                }
                // Everything, which is what somebody asks for after copying a
                // password. The selection itself is left alone: taking the
                // clipboard away from the application that owns it is not
                // this compositor's to do.
                None => state.clipboard.clear(),
            }
            state.notify_clipboard();
        }

        // The launcher: the list, and starting what was chosen. The scan and
        // the token are the state's; this is only the routing, which is what
        // this file is.
        Request::LauncherQuery { filter } => state.launcher_query(filter),
        Request::LauncherLaunch { id, generation } => state.launcher_launch(id, generation),

        // A button on the bar's media widget, sent to whichever player the
        // compositor last reported. The action is checked there rather than
        // here, where the list of methods lives.
        Request::MprisControl { action } => state.mpris.control(action),
        Request::AiLogin { provider } => {
            if provider == "openai" {
                state.ai_usage.login_openai();
            } else {
                reject(
                    state,
                    "ai.login",
                    &format!("no OAuth login for {provider:?}"),
                );
            }
        }
        Request::PowerProfile { profile } => state.power.set_profile(profile),
        // The bar's power picker. Three go to logind through the UPower
        // worker — the call the lid policy makes when a hinge closes, and its
        // two siblings — and the one this compositor owns outright is not in
        // the group: quitting is a `Request::Quit`, and routing it here would
        // leave two ways to do one thing.
        Request::PowerAction { action } => match action.as_str() {
            "suspend" => state.power.suspend(),
            "reboot" => state.power.reboot(),
            "poweroff" => state.power.poweroff(),
            other => reject(state, "power.action", &format!("no such action {other:?}")),
        },

        // The on-screen keyboard, and the only place it touches this file:
        // everything else it needs — the layout to draw, when to show
        // itself — is decided in the shell and in `sync_osk_wanted`. A tap is
        // `pressed: true` immediately followed by `pressed: false`; a key
        // held down is `true` once and `false` on lift, and the seat's own
        // keyboard repeat takes it from there, the same as a hardware key.
        // See `Request::OskKey`'s doc comment for why this is `inject_keysym`
        // and not `commit_string` through the input-method path.
        Request::OskKey { keysym, pressed } => {
            state.inject_keysym(keysym, pressed);
        }

        // The two radios. Everything here is handed straight to the worker
        // that owns the bus connection — the checking is done there, where the
        // list of what NetworkManager and BlueZ will be asked lives, and the
        // compositor's loop never waits for either of them.
        //
        // `network.scan` and `bluetooth.scan` are also how the shell says a
        // picker is open: absent `enabled` means yes, which is what a picker
        // sends when it opens, and `false` is what it sends when it goes away.
        Request::NetworkScan { enabled } => state.network.watch(enabled.unwrap_or(true)),
        Request::NetworkConnect { ssid, passphrase } => state.network.connect(ssid, passphrase),
        Request::NetworkDisconnect => state.network.disconnect(),
        Request::NetworkRadio { enabled } => state.network.radio(enabled),
        Request::BluetoothPower { enabled } => state.bluetooth.power(enabled),
        Request::BluetoothScan { enabled } => state.bluetooth.watch(enabled.unwrap_or(true)),
        Request::BluetoothDevice { address, action } => state.bluetooth.device(address, action),

        // Both halves of an open menu: a row chosen, and the menu going away
        // without one. The application is told either way, because a menu it
        // is never told about closing is one it believes is still on screen.
        Request::TrayMenuClick { id, item } => state.tray.menu_click(id, item),
        Request::TrayMenuClosed { id } => state.tray.menu_closed(id),
        Request::TrayScroll {
            id,
            delta,
            orientation,
        } => match orientation.as_str() {
            "" | "vertical" | "horizontal" => state.tray.scroll(id, delta, orientation),
            other => reject(
                state,
                "tray.scroll",
                &format!("no such orientation {other:?}"),
            ),
        },

        Request::BindAdd { chord, action } => {
            // Runtime binds from the shell are additive and expendable; the
            // ones that must survive a broken shell are the defaults.
            if !install_binding(&mut state.bindings, &chord, &action) {
                reject(state, "bind.add", &format!("{chord}={action}"));
            }
        }

        // Driving the pointer from the socket. The same three calls the
        // libinput path makes, in the same order, so a scripted click and a
        // real one are the same event by the time anything sees it.
        Request::InputPointer { x, y } => state.inject_pointer(x, y),

        Request::InputButton { button, pressed } => state.inject_button(button, pressed),

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

        Request::ConfigWallpaper { path, mode } => {
            // Resolved before anything is changed, so a message naming a
            // picture that is not there and a mode that is leaves neither
            // half applied — a wallpaper cycler pointed at a directory that
            // has been moved should not also lose the mode it was using.
            let url = match path.as_deref() {
                // The empty string is "take it away", which is the only way
                // back to the shell's own background.
                Some(path) if path.trim().is_empty() => Some(None),
                Some(path) => match crate::config::wallpaper_value(path, "config.wallpaper") {
                    Ok(url) => Some(Some(url)),
                    Err(e) => {
                        reject(state, "config.wallpaper", &e.to_string());
                        return;
                    }
                },
                None => None,
            };
            let mode = match mode.as_deref() {
                Some(mode) => match crate::config::parse_wallpaper_mode(mode) {
                    Ok(mode) => Some(mode),
                    Err(e) => {
                        reject(state, "config.wallpaper", &e.to_string());
                        return;
                    }
                },
                None => None,
            };

            let mut changed = false;
            if let Some(url) = url {
                state.config.wallpaper = url;
                changed = true;
            }
            if let Some(mode) = mode {
                state.config.wallpaper_mode = Some(mode);
                changed = true;
            }
            if changed {
                // The shell paints the desktop background, so the picture
                // arrives the same way the gaps do: the compositor's copy of
                // the config, re-announced. Nothing here draws it, which is
                // why there is no `needs_render` — the page repaints itself
                // and the frame that follows is the shell's.
                state.notify_config();
            }
        }

        Request::ConfigDarkMode { enabled } => {
            // Absent toggles, on the same terms as `output.hdr`: that is what
            // the `appearance toggle` keybinding wants, and a panel drawing a
            // switch sends the state it wants so that two clicks in a row do
            // not land on whichever value the desk happened to be on.
            //
            // Read back off `appearance` rather than off `state.dark_mode`,
            // because the portal is where the answer actually lives: a client
            // that set the scheme over the bus moved it, and toggling from a
            // stale copy would flip to the value it already has.
            let want = enabled.unwrap_or_else(|| !state.appearance.is_dark());
            state.dark_mode = want;
            state.appearance.set_dark(want);
            // The shell has to be able to draw the switch in the position it
            // is really in, which means the config event has to carry it. Sent
            // even when nothing changed: a panel that asked for the value it
            // already had is a panel waiting for an answer, and silence is
            // indistinguishable from a message that was dropped.
            state.config.dark_mode = want;
            state.notify_config();
        }

        Request::ConfigSave => config_save(state),

        Request::Quit => state.shutdown(),
    }
}

/// Write the runtime settings out so they survive the next start.
///
/// The reasoning for the overlay file — and for why this is one explicit
/// request rather than something each setter does — is in `crate::settings`.
/// What is decided here is only *what* goes into it: the settings a panel can
/// reach, read back off the compositor's own copy rather than remembered as
/// they were set, so that a value the compositor adjusted or refused is saved
/// as what is actually on screen.
fn config_save(state: &mut ViewportState) {
    let Some(config_path) = state.config_file_path.clone() else {
        // No config file path means no home directory to hang one off, which
        // is a session started by a test harness or an init script with the
        // environment stripped. Refusing is honest; inventing a path under
        // `/` and failing to write it would be the same refusal with a worse
        // message.
        reject(
            state,
            "config.save",
            "there is no configuration file path to save beside",
        );
        return;
    };
    let path = crate::settings::path(&config_path);

    let mut overlay = crate::settings::Overlay {
        dark_mode: Some(state.dark_mode),
        wallpaper: state.config.wallpaper.clone(),
        wallpaper_mode: state.config.wallpaper_mode.clone(),
        gaps: state.config.gaps.clone(),
        border: state.config.border.clone(),
        outputs: Default::default(),
    };

    // Only the monitors somebody actually configured this session.
    //
    // Writing every head would be the easy version and the wrong one: it would
    // freeze whatever mode the backend happened to pick for a screen nobody
    // has an opinion about, and it would put a hand-written `outputs` block in
    // the config file permanently behind an overlay that merely restates it.
    // A monitor arrives here because a message named it — see
    // `ViewportState::output_settings_touched`.
    let touched = state.output_settings_touched.clone();
    for output in state.physical_outputs() {
        let name = output.name();
        if !touched.contains(&name) {
            continue;
        }
        let mode = output.current_mode().map(|mode| {
            // Hertz, because that is what `config::parse_mode` reads and what
            // a person writes; the kernel counts in millihertz and the third
            // decimal is load-bearing — 143.998 is a real refresh rate and
            // rounding it to 144 is a mode the connector does not have.
            format!(
                "{}x{}@{}",
                mode.size.w,
                mode.size.h,
                f64::from(mode.refresh) / 1000.0
            )
        });
        // Through serde rather than a match of its own: the strings are
        // `Transform`'s `#[serde(rename)]`s, the shell compares against them
        // literally, and a second table here is a second thing to keep in
        // step with the first.
        let transform = serde_json::to_value(from_smithay_transform(output.current_transform()))
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned));
        let position = state
            .space
            .output_geometry(&state.mirror_source(&output))
            .map(|geometry| geometry.loc)
            .or_else(|| {
                state
                    .output_memory
                    .get(&name)
                    .map(|saved| (saved.x, saved.y).into())
            })
            .unwrap_or_default();
        overlay.outputs.insert(
            name,
            crate::settings::OutputOverlay {
                enabled: Some(state.output_is_enabled(&output)),
                mode,
                scale: Some(output.current_scale().fractional_scale()),
                transform,
                x: Some(position.x),
                y: Some(position.y),
                mirror: state.output_mirrors.get(&output.name()).cloned(),
                vrr: Some(state.configured_vrr(&output.name())),
            },
        );
    }

    match crate::settings::save(&path, &overlay) {
        Ok(()) => {
            tracing::info!("saved the runtime settings to {}", path.display());
            let event = Event::ConfigSaved {
                path: path.to_string_lossy().into_owned(),
            };
            let client = state.dispatch_client;
            if client == 0 {
                state.notify(&event);
            } else {
                state.ipc.send_to(client, &event);
            }
        }
        Err(e) => reject(state, "config.save", &format!("{e:#}")),
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
        // Saturating rather than wrapping: these numbers come off the socket,
        // where serde takes an i32, and the origin is the real position of
        // the monitor the shell lives on. A page coordinate near i32::MAX is
        // nonsense but arrives without complaint, and adding to it used to
        // wrap in a release build — putting the window back on the first
        // monitor, the exact bug this translation is here to prevent — and
        // panic in a debug one. Clamped to the edge of the coordinate space,
        // a nonsense rectangle stays nonsense without taking the desktop
        // with it.
        layout.box_.x = layout.box_.x.map(|x| x.saturating_add(origin.x));
        layout.box_.y = layout.box_.y.map(|y| y.saturating_add(origin.y));
        if let Some(clip) = layout.clip.as_mut() {
            clip.x = clip.x.map(|x| x.saturating_add(origin.x));
            clip.y = clip.y.map(|y| y.saturating_add(origin.y));
        }
        if let Some(frame) = layout.frame.as_mut() {
            frame.x = frame.x.saturating_add(origin.x);
            frame.y = frame.y.saturating_add(origin.y);
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
    //
    // Owed rather than done: the shell lays every window out on every frame of
    // an animation, and the stack that comes out of eight of those is the stack
    // that comes out of the first. `settle` does it once, before anything can
    // look — see the note on `apply` for why nothing can look before then.
    state.needs_restack = true;

    // A window that just crossed onto another monitor is being shown in a
    // different colour space than the one it was drawing for. Nothing else
    // notices: the outputs themselves did not change, so `notify_output_colour`
    // never runs, and a client that asked once would keep the answer it got on
    // the screen it started on.
    //
    // Owed for the same reason, and here there is not even a reader to be
    // early for: the answer is an event to a client, and a client cannot see
    // one that has not been flushed yet.
    state.needs_colour_notify = true;

    // And the taskbars: a window that crossed onto another monitor belongs in
    // a different monitor's list now. Owed like the rest — `settle` diffs the
    // whole set once per batch of messages, and an unchanged window sends
    // nothing.
    state.needs_foreign_outputs = true;
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

    // A non-positive scale is refused outright, as `apply_output_configuration`
    // refuses it: a scale of zero or less is not a thing a monitor can have,
    // and it used to be dropped silently, which reported success for a change
    // that was never made. Before the enabled check below, so a message that
    // carries both is refused whole rather than half-applied — the same
    // validate-everything-first rule the management path runs.
    if let Some(scale) = config.scale {
        if scale <= 0.0 {
            reject(state, "output.configure", &format!("scale {scale}"));
            return;
        }
    }

    if config.mirror.is_some()
        && (config.enabled.is_some()
            || config.mode.is_some()
            || config.scale.is_some()
            || config.transform.is_some()
            || config.x.is_some()
            || config.y.is_some())
    {
        reject(
            state,
            "output.configure",
            "mirror must be configured separately from mode, scale, transform, position or power",
        );
        return;
    }
    if (config.mode.is_some() || config.scale.is_some() || config.transform.is_some())
        && (state.output_mirrors.contains_key(&config.name)
            || state
                .output_mirrors
                .values()
                .any(|source| source == &config.name))
    {
        reject(
            state,
            "output.configure",
            "detach this mirror group before changing mode, scale or transform",
        );
        return;
    }
    if (config.x.is_some() || config.y.is_some()) && state.output_mirrors.contains_key(&config.name)
    {
        reject(
            state,
            "output.configure",
            "a mirror sink has no independent logical position",
        );
        return;
    }
    if let Some(source) = config.mirror.as_deref() {
        if let Err(e) = state.configure_mirror(&output, Some(source)) {
            reject(state, "output.configure", &e);
            return;
        }
    }

    // The last refusal, hoisted out of the `enabled` branch below so that
    // every `return` meaning "nothing happened" is above the point where the
    // change becomes provisional. A refused configuration that had already
    // armed a countdown would spend twelve seconds waiting to put back a
    // desktop nothing had moved.
    //
    // A desk with every output disabled is not a state anything can be
    // recovered from by pointing at a screen, which is why this is refused at
    // all — the same rule `apply_output_configuration` follows.
    let disabling_logical = state.space.outputs().any(|candidate| candidate == &output);
    let has_surviving_mirror = state.output_mirrors.iter().any(|(sink, source)| {
        source == &config.name
            && state
                .any_output_by_name(sink)
                .is_some_and(|output| state.output_is_enabled(&output))
    });
    if config.enabled == Some(false)
        && disabling_logical
        && state.space.outputs().count() <= 1
        && !has_surviving_mirror
    {
        reject(
            state,
            "output.configure",
            "refusing to turn off the only output left on",
        );
        return;
    }

    // Everything above this point refuses; everything below it changes the
    // hardware. So this is where the change becomes provisional and where the
    // monitor is written down as one somebody has an opinion about — after the
    // last `return` that means "nothing happened", and before the first line
    // that means something did.
    //
    // Which fields count: a mode, a scale, a rotation or the power. Those are
    // the four that can leave a person looking at a screen they can no longer
    // read the undo button on. A position cannot — a monitor moved in the
    // layout is still showing what it showed — so moving one does not start a
    // countdown, and a panel dragging a monitor about does not have to answer
    // a dialog for every pixel of it.
    let risky = config.mode.is_some()
        || config.scale.is_some()
        || config.transform.is_some()
        || config.enabled.is_some();
    if !state.output_config_replay {
        state.output_settings_touched.insert(config.name.clone());
        if risky {
            state.arm_output_revert();
        }
    }

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
    // The last one on is refused, matching `apply_output_configuration` — that
    // check is a few lines above rather than here, so that a refusal cannot
    // leave a revert armed behind it.
    if let Some(enabled) = config.enabled {
        if !enabled {
            // Written down before the unmap, not after: `remember_output`
            // reads the position out of the space, and once the output is
            // unmapped there is nothing left to read — the memory is the only
            // record of where it goes when it is turned back on.
            state.remember_output(&output);
            state.set_output_enabled(&output, false);
            // Nothing below applies to a screen that is off, and a mode or a
            // position for one is not an error worth reporting either. What
            // does apply is the tail every applied configuration runs on the
            // management path, which the early return here used to skip: the
            // windows it carried were never put back anywhere, output-management
            // clients kept heads describing a screen that had gone dark, and
            // the frame that showed the desktop without it was never asked
            // for.
            state.remap_placed_views();
            state.notify_output_layout();
            state.advertise_outputs();
            state.needs_render = true;
            return;
        }
        state.set_output_enabled(&output, enabled);
    }

    if let Some(mode) = config.vrr.or(config.adaptive_sync.map(|enabled| {
        if enabled {
            viewport_ipc::event::VrrMode::Always
        } else {
            viewport_ipc::event::VrrMode::Off
        }
    })) {
        state.output_vrr.insert(config.name.clone(), mode);
        state.output_vrr_wanted.remove(&config.name);
        state.needs_render = true;
    }

    // Prefer an exact modeline the display advertised; fall back to a custom
    // mode so unusual panels stay configurable. The fallback needs a refresh:
    // the kernel takes a whole modeline, and a custom mode with none of its
    // own was a silent no-op waiting to be programmed.
    let mode = config.mode.and_then(|requested| {
        let exact = output.modes().into_iter().find(|m| {
            m.size.w == requested.width
                && m.size.h == requested.height
                && (requested.refresh == 0 || m.refresh == requested.refresh)
        });
        exact.or(
            if requested.width > 0 && requested.height > 0 && requested.refresh > 0 {
                Some(OutputMode {
                    size: (requested.width, requested.height).into(),
                    refresh: requested.refresh,
                })
            } else {
                None
            },
        )
    });

    // Refused above when non-positive, so what is left is taken as given.
    let scale = config.scale.map(Scale::Fractional);
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
    state.advertise_outputs();
    state.needs_render = true;
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

/// Report a refusal to the client that earned it.
fn reject(state: &mut ViewportState, context: &str, message: &str) {
    let event = Event::Error {
        context: context.to_owned(),
        message: message.to_owned(),
    };
    let client = state.dispatch_client;
    if client == 0 {
        state.notify(&event);
    } else {
        state.ipc.send_to(client, &event);
    }
}

/// Whether two bindings are drawn on the same chord, in the same mode — and
/// so the thing a runtime bind replaces.
///
/// The whole identity, every field of it. A wheel binding and a mouse-button
/// binding both carry keysym 0, because each is drawn on no key at all, so
/// matched on modifiers and keysym alone they are indistinguishable from
/// each other and from every chord those modifiers leave unmapped — which is
/// how registering `Mod4+WheelUp` used to delete `Mod4+WheelDown` and
/// `Mod4+Mouse4` with it. And the mode is part of what a chord *is*: a plain
/// `h` shares its keysym with the resize mode's `h`, and replacing one is
/// not replacing the other.
///
/// (`binding::shadows` looks close and is not: it asks whether an earlier
/// key binding swallows a later one, which is a question about
/// reachability, not identity.)
fn same_chord(a: &crate::binding::Binding, b: &crate::binding::Binding) -> bool {
    a.mode == b.mode
        && a.modifiers == b.modifiers
        && a.keysym == b.keysym
        && a.button == b.button
        && a.wheel == b.wheel
}

/// Register a runtime binding from a `bind.add` message.
///
/// Replaced rather than appended when the chord is already taken, so
/// re-registering one does not leave the older binding shadowing it — but
/// only *its own* chord goes, by the full identity [`same_chord`]
/// matches on.
fn install_binding(bindings: &mut Vec<crate::binding::Binding>, chord: &str, action: &str) -> bool {
    match crate::binding::parse(&format!("{chord}={action}")) {
        Some(binding) => {
            bindings.retain(|existing| !same_chord(existing, &binding));
            bindings.push(binding);
            true
        }
        None => false,
    }
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

    #[test]
    fn bind_add_replaces_the_same_chord_and_nothing_else() {
        use crate::binding::{Action, Wheel};
        use smithay::input::keyboard::{keysyms, ModifiersState};

        let mut bindings = crate::binding::defaults("foot", Some("wmenu-run"), "tiling");

        // A plain `h` shares its modifiers and its keysym with the resize
        // mode's `h`. Replaced on those two alone, adding the first deleted
        // the second — and resize mode lost half its keymap to a bind that
        // never mentioned it.
        assert!(install_binding(
            &mut bindings,
            "h",
            "shell layout.focus left"
        ));
        let free = ModifiersState::default();
        assert!(
            crate::binding::match_binding(&bindings, &free, keysyms::KEY_h, "resize").is_some(),
            "the resize-mode binding for h is gone"
        );
        assert!(crate::binding::match_binding(&bindings, &free, keysyms::KEY_h, "").is_some());

        // Wheel and button bindings carry keysym 0, because each is drawn on
        // no key at all. Replaced on the keysym alone, registering one wheel
        // direction deleted the other direction and the side button with it.
        assert!(install_binding(&mut bindings, "Mod4+Mouse4", "close"));
        assert!(install_binding(
            &mut bindings,
            "Mod4+WheelUp",
            "shell workspace.next"
        ));
        assert!(install_binding(
            &mut bindings,
            "Mod4+WheelDown",
            "shell workspace.prev"
        ));
        let held = ModifiersState {
            logo: true,
            ..Default::default()
        };
        assert!(
            crate::binding::match_button(&bindings, &held, 0x113, "").is_some(),
            "the Mouse4 binding is gone"
        );
        assert_eq!(
            crate::binding::match_wheel(&bindings, &held, Wheel::Down, ""),
            Some(&Action::Shell("workspace.prev".to_owned())),
            "the WheelDown binding is gone"
        );

        // And the chord that *was* named is replaced rather than appended
        // to, so re-registering one does not leave the older binding
        // shadowing it.
        let before = bindings.len();
        assert!(install_binding(
            &mut bindings,
            "Mod4+WheelUp",
            "shell workspace.switch 2"
        ));
        assert_eq!(bindings.len(), before);
        assert_eq!(
            crate::binding::match_wheel(&bindings, &held, Wheel::Up, ""),
            Some(&Action::Shell("workspace.switch 2".to_owned()))
        );
    }
}
